package io.github.rosinality.artty

import android.content.Context
import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import java.io.File
import java.security.KeyStore
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec
import org.json.JSONObject
import uniffi.artty_android.keyInfo

/**
 * A private key added to the app: what the ssh config's `IdentityFile` names it by (its file
 * name: `IdentityFile ~/.ssh/work` means `work`), its text, its passphrase if kept, and its
 * public half.
 */
data class AddedKey(val name: String, val text: String, val passphrase: String?, val public: String, val encrypted: Boolean)

/**
 * The keys added, each in a file of the app's encrypted with a key of Android Keystore's (AES,
 * which can't be read out): what's on disk is no use off this phone. Unlike this phone's own
 * key, they are copies, as on any machine that has them.
 */
class AddedKeys(context: Context) {
    private val dir = File(context.filesDir, "keys").also { it.mkdirs() }

    fun list(): List<AddedKey> =
        (dir.listFiles() ?: emptyArray()).filter { it.name.endsWith(".key") }.mapNotNull { read(it) }.sortedBy { it.name }

    /** Add (or replace) `name`: fails, saying why, if `text` isn't a private key. */
    fun add(name: String, text: String, passphrase: String?): Result<AddedKey> = runCatching {
        require(name.isNotBlank() && name.none { it == '/' }) { "A name is a file name: no slashes" }
        val info = keyInfo(text)
        val key = AddedKey(name.trim(), text.trim() + "\n", passphrase?.ifEmpty { null }, info.public, info.encrypted)
        write(key)
        key
    }

    fun remove(name: String) {
        file(name).delete()
    }

    fun rename(key: AddedKey, to: String): Result<AddedKey> = runCatching {
        require(to.isNotBlank() && to.none { it == '/' }) { "A name is a file name: no slashes" }
        require(!file(to).exists()) { "There's a key named $to already" }
        val renamed = key.copy(name = to.trim())
        write(renamed)
        remove(key.name)
        renamed
    }

    /** Keep (or forget, `null`) a key's passphrase. */
    fun setPassphrase(name: String, passphrase: String?) {
        val key = read(file(name)) ?: return
        write(key.copy(passphrase = passphrase?.ifEmpty { null }))
    }

    private fun file(name: String) = File(dir, "$name.key")

    private fun write(key: AddedKey) {
        val json = JSONObject()
            .put("name", key.name)
            .put("text", key.text)
            .put("passphrase", key.passphrase ?: "")
            .put("public", key.public)
            .put("encrypted", key.encrypted)
        val cipher = Cipher.getInstance(CIPHER).apply { init(Cipher.ENCRYPT_MODE, secret()) }
        val sealed = cipher.iv + cipher.doFinal(json.toString().toByteArray())
        val temporary = File(dir, "${key.name}.tmp")
        temporary.writeBytes(sealed)
        temporary.renameTo(file(key.name))
    }

    private fun read(file: File): AddedKey? = runCatching {
        val sealed = file.readBytes()
        val cipher = Cipher.getInstance(CIPHER).apply {
            init(Cipher.DECRYPT_MODE, secret(), GCMParameterSpec(128, sealed, 0, IV))
        }
        val json = JSONObject(String(cipher.doFinal(sealed, IV, sealed.size - IV)))
        AddedKey(
            json.getString("name"),
            json.getString("text"),
            json.optString("passphrase").ifEmpty { null },
            json.getString("public"),
            json.optBoolean("encrypted"),
        )
    }.getOrNull()

    @Synchronized
    private fun secret(): SecretKey {
        val keyStore = KeyStore.getInstance(PROVIDER).apply { load(null) }
        (keyStore.getKey(ALIAS, null) as? SecretKey)?.let { return it }
        val spec = KeyGenParameterSpec.Builder(ALIAS, KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT)
            .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
            .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
            .setKeySize(256)
            .build()
        return KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, PROVIDER).run {
            init(spec)
            generateKey()
        }
    }

    private companion object {
        const val PROVIDER = "AndroidKeyStore"
        const val ALIAS = "artty-added-keys"
        const val CIPHER = "AES/GCM/NoPadding"
        const val IV = 12
    }
}
