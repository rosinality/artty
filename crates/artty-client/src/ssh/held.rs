//! Signing with a key kept elsewhere (Android Keystore): its DER signatures
//! as ssh sends them.

use russh::keys::{PublicKey, ssh_key};

use super::Held;

/// Signing for russh with a [`HeldKey`].
pub(super) struct HeldSigner<'a>(pub(super) &'a Held);

#[derive(Debug)]
pub(super) struct HeldError;

impl From<russh::SendError> for HeldError {
    fn from(_: russh::SendError) -> Self {
        HeldError
    }
}

impl russh::Signer for HeldSigner<'_> {
    type Error = HeldError;

    #[allow(clippy::manual_async_fn)]
    fn auth_sign(
        &mut self,
        _key: &russh::keys::agent::AgentIdentity,
        _hash_alg: Option<ssh_key::HashAlg>,
        mut to_sign: Vec<u8>,
    ) -> impl Future<Output = Result<Vec<u8>, HeldError>> + Send {
        let key = self.0.key.clone();
        async move {
            // It may take a moment (StrongBox), off the runtime's threads.
            let data = to_sign.clone();
            let der = tokio::task::spawn_blocking(move || key.sign(&data))
                .await
                .ok()
                .flatten()
                .ok_or(HeldError)?;
            // Appended, as an agent's would be.
            let blob = ssh_signature(&der).ok_or(HeldError)?;
            to_sign.extend_from_slice(&(blob.len() as u32).to_be_bytes());
            to_sign.extend_from_slice(&blob);
            Ok(to_sign)
        }
    }
}

/// An ECDSA P-256 signature in DER (`SEQUENCE { INTEGER r, INTEGER s }`) as
/// ssh sends it: `string "ecdsa-sha2-nistp256"`, then `string (mpint r, mpint s)`.
pub(super) fn ssh_signature(der: &[u8]) -> Option<Vec<u8>> {
    use russh::keys::ssh_encoding::Encode;
    let (r, s) = der_integers(der)?;
    let mut data = Vec::new();
    ssh_key::Mpint::from_positive_bytes(r).encode(&mut data).ok()?;
    ssh_key::Mpint::from_positive_bytes(s).encode(&mut data).ok()?;
    let algorithm = ssh_key::Algorithm::Ecdsa { curve: ssh_key::EcdsaCurve::NistP256 };
    ssh_key::Signature::new(algorithm, data).ok()?.encode_vec().ok()
}

/// DER's `SEQUENCE { INTEGER, INTEGER }`: the two integers' bytes.
fn der_integers(der: &[u8]) -> Option<(&[u8], &[u8])> {
    // A length: short form, or one or two bytes of long form.
    fn length(bytes: &[u8]) -> Option<(usize, &[u8])> {
        match *bytes.first()? {
            n if n < 0x80 => Some((n as usize, &bytes[1..])),
            0x81 => Some((*bytes.get(1)? as usize, bytes.get(2..)?)),
            0x82 => {
                Some(((*bytes.get(1)? as usize) << 8 | *bytes.get(2)? as usize, bytes.get(3..)?))
            },
            _ => None,
        }
    }
    fn integer(bytes: &[u8]) -> Option<(&[u8], &[u8])> {
        let (&0x02, rest) = bytes.split_first()? else { return None };
        let (n, rest) = length(rest)?;
        Some((rest.get(..n)?, rest.get(n..)?))
    }
    let (&0x30, rest) = der.split_first()? else { return None };
    let (n, rest) = length(rest)?;
    let body = rest.get(..n)?;
    let (r, body) = integer(body)?;
    let (s, body) = integer(body)?;
    body.is_empty().then_some((r, s))
}

/// A P-256 public key as Keystore gives it (X.509 `SubjectPublicKeyInfo`,
/// DER), for [`Held`]: its point is the last 65 bytes.
pub fn held_public_key(spki: &[u8], comment: &str) -> Result<PublicKey, String> {
    let point = spki.len().checked_sub(65).map(|at| &spki[at..]).ok_or("not a P-256 key")?;
    let ecdsa = ssh_key::public::EcdsaPublicKey::from_sec1_bytes(point)
        .map_err(|err| format!("not a P-256 key: {err}"))?;
    if !matches!(ecdsa, ssh_key::public::EcdsaPublicKey::NistP256(_)) {
        return Err("not a P-256 key".into());
    }
    Ok(PublicKey::new(ssh_key::public::KeyData::Ecdsa(ecdsa), comment))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Signed in DER (as Keystore signs), sent as ssh's: what a server checks passes.
    #[test]
    fn held_keys_sign_as_ssh_wants() {
        use p256::ecdsa::signature::Signer;
        use p256::pkcs8::EncodePublicKey;
        use russh::keys::signature::Verifier;
        use russh::keys::ssh_encoding::Decode;
        let secret = p256::ecdsa::SigningKey::from_slice(&[9; 32]).unwrap();
        let spki = secret.verifying_key().to_public_key_der().unwrap();
        let public = held_public_key(spki.as_bytes(), "phone").unwrap();
        assert!(public.to_openssh().unwrap().starts_with("ecdsa-sha2-nistp256 "));
        for message in [&b"to sign"[..], &[0xff; 300]] {
            let signature: p256::ecdsa::Signature = secret.sign(message);
            let blob = ssh_signature(signature.to_der().as_bytes()).unwrap();
            let signature = ssh_key::Signature::decode(&mut &blob[..]).unwrap();
            Verifier::verify(public.key_data(), message, &signature).unwrap();
            assert!(Verifier::verify(public.key_data(), b"something else", &signature).is_err());
        }
        assert!(held_public_key(&[0; 10], "").is_err());
        assert!(ssh_signature(&[0x30, 0x03, 0x02, 0x01]).is_none());
    }
}
