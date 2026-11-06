package io.github.rosinality.artty

import android.os.Build
import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyInfo
import android.security.keystore.KeyProperties
import android.security.keystore.StrongBoxUnavailableException
import java.security.KeyFactory
import java.security.KeyPairGenerator
import java.security.KeyStore
import java.security.PrivateKey
import java.security.Signature
import java.security.spec.ECGenParameterSpec
import uniffi.artty_android.KeystoreKey

/**
 * This phone's ssh key, made in Android Keystore — in the phone's secure hardware (StrongBox
 * where it has it) — and signing there: its private half can't be read out, backed up or
 * copied, by the app included. ECDSA P-256, which Keystore has everywhere (and OpenSSH takes).
 */
class PhoneKey : KeystoreKey {
    private val keyStore = KeyStore.getInstance(PROVIDER).apply { load(null) }

    /** Its public half: X.509 `SubjectPublicKeyInfo`, as the Rust side reads it. */
    val publicKey: ByteArray
        get() {
            ensure()
            return keyStore.getCertificate(ALIAS).publicKey.encoded
        }

    /** Where it's kept, said for the user. */
    val keptIn: String
        get() {
            ensure()
            val key = keyStore.getKey(ALIAS, null) as PrivateKey
            val info = runCatching {
                KeyFactory.getInstance(key.algorithm, PROVIDER).getKeySpec(key, KeyInfo::class.java)
            }.getOrNull() ?: return "this phone's Keystore"
            return if (Build.VERSION.SDK_INT >= 31) {
                when (info.securityLevel) {
                    KeyProperties.SECURITY_LEVEL_STRONGBOX -> "this phone's security chip (StrongBox)"
                    KeyProperties.SECURITY_LEVEL_TRUSTED_ENVIRONMENT -> "this phone's secure hardware (TEE)"
                    else -> "this phone's Keystore (in software)"
                }
            } else {
                @Suppress("DEPRECATION")
                if (info.isInsideSecureHardware) "this phone's secure hardware" else "this phone's Keystore (in software)"
            }
        }

    override fun sign(data: ByteArray): ByteArray? = runCatching {
        ensure()
        Signature.getInstance("SHA256withECDSA").run {
            initSign(keyStore.getKey(ALIAS, null) as PrivateKey)
            update(data)
            sign()
        }
    }.getOrNull()

    @Synchronized
    private fun ensure() {
        if (keyStore.containsAlias(ALIAS)) return
        fun make(strongBox: Boolean) {
            val spec = KeyGenParameterSpec.Builder(ALIAS, KeyProperties.PURPOSE_SIGN)
                .setAlgorithmParameterSpec(ECGenParameterSpec("secp256r1"))
                .setDigests(KeyProperties.DIGEST_SHA256)
                .setIsStrongBoxBacked(strongBox)
                .build()
            KeyPairGenerator.getInstance(KeyProperties.KEY_ALGORITHM_EC, PROVIDER).run {
                initialize(spec)
                generateKeyPair()
            }
        }
        try {
            make(strongBox = true)
        } catch (e: StrongBoxUnavailableException) {
            make(strongBox = false)
        }
    }

    private companion object {
        const val PROVIDER = "AndroidKeyStore"
        const val ALIAS = "artty-ssh"
    }
}
