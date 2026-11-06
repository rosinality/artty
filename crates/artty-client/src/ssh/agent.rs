//! Agent forwarding (`ForwardAgent`): the machine's programs (`git`, an
//! onward `ssh`) use this app's keys — the held one and those added —
//! through the agent protocol over `auth-agent@openssh.com` channels. The
//! keys never leave: each signature is made here, and only once the user
//! agrees ([`Login::agent_sign`]).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use russh::keys::ssh_encoding::{Decode, Encode};
use russh::keys::{HashAlg, PublicKey, signature, ssh_key};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use super::held::ssh_signature;
use super::{Credentials, Login};

const FAILURE: u8 = 5;
const REQUEST_IDENTITIES: u8 = 11;
const IDENTITIES_ANSWER: u8 = 12;
const SIGN_REQUEST: u8 = 13;
const SIGN_RESPONSE: u8 = 14;
const RSA_SHA2_256: u32 = 2;
const RSA_SHA2_512: u32 = 4;
/// Longer than any request an agent takes.
const MAX_MESSAGE: usize = 256 * 1024;

pub(super) struct Agent {
    credentials: Credentials,
    login: Arc<dyn Login>,
    /// Who asks (for the user): `user@host`.
    host: String,
    passphrases: Arc<Mutex<HashMap<String, String>>>,
}

/// A key the agent offers.
enum Offered {
    Held(usize),
    Key(usize),
}

impl Agent {
    pub(super) fn new(
        credentials: Credentials,
        login: Arc<dyn Login>,
        host: String,
        passphrases: Arc<Mutex<HashMap<String, String>>>,
    ) -> Agent {
        Agent { credentials, login, host, passphrases }
    }

    /// The keys offered: public halves, names, and which they are.
    fn offered(&self) -> Vec<(PublicKey, String, Offered)> {
        let mut out = Vec::new();
        for (i, held) in self.credentials.held.iter().enumerate() {
            out.push((held.public.clone(), held.name.clone(), Offered::Held(i)));
        }
        for (i, key) in self.credentials.keys.iter().enumerate() {
            // The public half is in the clear, a passphrase or not.
            let public = ssh_key::PrivateKey::from_openssh(key.text.trim())
                .map(|k| k.public_key().clone())
                .or_else(|_| {
                    russh::keys::decode_secret_key(&key.text, key.passphrase.as_deref())
                        .map(|k| k.public_key().clone())
                });
            if let Ok(public) = public {
                out.push((public, key.name.clone(), Offered::Key(i)));
            }
        }
        out
    }

    /// Answer the machine's requests on `stream` until it closes.
    pub(super) async fn serve(self: Arc<Self>, mut stream: impl AsyncRead + AsyncWrite + Unpin) {
        loop {
            let Ok(length) = stream.read_u32().await else { return };
            let length = length as usize;
            if length == 0 || length > MAX_MESSAGE {
                return;
            }
            let mut message = vec![0; length];
            if stream.read_exact(&mut message).await.is_err() {
                return;
            }
            let answer = self.clone().answer(&message).await.unwrap_or_else(|| vec![FAILURE]);
            let mut framed = (answer.len() as u32).to_be_bytes().to_vec();
            framed.extend(answer);
            if stream.write_all(&framed).await.is_err() {
                return;
            }
        }
    }

    async fn answer(self: Arc<Self>, message: &[u8]) -> Option<Vec<u8>> {
        let (&kind, mut body) = message.split_first()?;
        match kind {
            REQUEST_IDENTITIES => {
                let offered = self.offered();
                let mut out = vec![IDENTITIES_ANSWER];
                (offered.len() as u32).encode(&mut out).ok()?;
                for (public, name, _) in offered {
                    public.key_data().encode_vec().ok()?.encode(&mut out).ok()?;
                    name.encode(&mut out).ok()?;
                }
                Some(out)
            },
            SIGN_REQUEST => {
                let blob = Vec::<u8>::decode(&mut body).ok()?;
                let data = Vec::<u8>::decode(&mut body).ok()?;
                let flags = u32::decode(&mut body).ok()?;
                let (_, name, which) = self.offered().into_iter().find(|(public, _, _)| {
                    public.key_data().encode_vec().ok().as_deref() == Some(&blob[..])
                })?;
                // Asked of the user: off the runtime's threads (they wait).
                let signature = tokio::task::spawn_blocking(move || {
                    if !self.login.agent_sign(&name, &self.host) {
                        return None;
                    }
                    self.sign(&which, &data, flags)
                })
                .await
                .ok()??;
                let mut out = vec![SIGN_RESPONSE];
                signature.encode(&mut out).ok()?;
                Some(out)
            },
            // Adding keys, locking, extensions: not this agent's.
            _ => None,
        }
    }

    /// `data` signed: the signature as ssh sends it.
    fn sign(&self, which: &Offered, data: &[u8], flags: u32) -> Option<Vec<u8>> {
        match *which {
            Offered::Held(i) => ssh_signature(&self.credentials.held.get(i)?.key.sign(data)?),
            Offered::Key(i) => {
                let key = self.credentials.keys.get(i)?;
                let private = super::login::decode(key, &*self.login, &self.passphrases)?;
                let signature = match private.key_data() {
                    ssh_key::private::KeypairData::Rsa(rsa) => {
                        let hash = if flags & RSA_SHA2_512 != 0 {
                            Some(HashAlg::Sha512)
                        } else if flags & RSA_SHA2_256 != 0 {
                            Some(HashAlg::Sha256)
                        } else {
                            None
                        };
                        signature::Signer::try_sign(&(rsa, hash), data).ok()?
                    },
                    _ => signature::Signer::<ssh_key::Signature>::try_sign(&private, data).ok()?,
                };
                signature.encode_vec().ok()
            },
        }
    }
}
