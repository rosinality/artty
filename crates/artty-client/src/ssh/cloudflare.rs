//! Hosts behind a Cloudflare Tunnel, reached as `cloudflared access ssh
//! --hostname H` reaches them (the `ProxyCommand` the tunnel's docs give):
//! ssh carried in a WebSocket to `https://H`, over TLS, with Cloudflare
//! Access's service token if the command gives one. Done here, so a phone
//! needs no `cloudflared`.

use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

use crate::ssh_config::{Local, Resolved};

/// Where a `cloudflared access ssh` command goes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tunnel {
    /// The tunnel's public hostname for the machine's ssh.
    pub hostname: String,
    /// Cloudflare Access's service token (client id and secret), if the
    /// command gives one (`--service-token-id`, `--service-token-secret`).
    pub service_token: Option<(String, String)>,
}

/// The tunnel `resolved`'s `ProxyCommand` reaches, if it is cloudflared's
/// `access ssh` (its `%` tokens as ssh expands them).
pub fn tunnel(resolved: &Resolved) -> Option<Tunnel> {
    let command = resolved.proxy_command.as_deref()?;
    let words: Vec<&str> =
        command.split_whitespace().map(|w| w.trim_matches(['"', '\''])).collect();
    let (program, args) = words.split_first()?;
    if program.rsplit('/').next() != Some("cloudflared") {
        return None;
    }
    // `access ssh`, maybe after global flags (`--loglevel x`).
    let at = args.windows(2).position(|w| w == ["access", "ssh"])?;
    let args = &args[at + 2..];
    // Flags as Go reads them: `--name value`, `--name=value`, one dash or two.
    let flag = |names: &[&str]| -> Option<String> {
        args.iter().enumerate().find_map(|(i, arg)| {
            let bare = arg.trim_start_matches('-');
            if !arg.starts_with('-') {
                return None;
            }
            names.iter().find_map(|name| match bare.split_once('=') {
                Some((n, value)) if n == *name => Some(value.to_owned()),
                None if bare == *name => args.get(i + 1).map(|v| (*v).to_owned()),
                _ => None,
            })
        })
    };
    let local = Local::default();
    let hostname = resolved.expand(&flag(&["hostname"])?, &local);
    let hostname = hostname
        .trim_start_matches("https://")
        .trim_start_matches("ssh://")
        .trim_end_matches('/')
        .to_owned();
    let id = flag(&["service-token-id", "id"]);
    let secret = flag(&["service-token-secret", "secret"]);
    let service_token = id.zip(secret);
    (!hostname.is_empty()).then_some(Tunnel { hostname, service_token })
}

/// Open the tunnel: a stream that is the machine's ssh.
pub(super) async fn open(tunnel: &Tunnel) -> Result<DuplexStream, String> {
    let host = tunnel.hostname.as_str();
    let tcp = TcpStream::connect((host, 443)).await.map_err(|err| format!("{host}: {err}"))?;
    let _ = tcp.set_nodelay(true);
    let roots =
        tokio_rustls::rustls::RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
    let provider = Arc::new(tokio_rustls::rustls::crypto::ring::default_provider());
    let mut config = tokio_rustls::rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|err| err.to_string())?
        .with_root_certificates(roots)
        .with_no_client_auth();
    // A WebSocket is HTTP/1.1's.
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    let name = tokio_rustls::rustls::pki_types::ServerName::try_from(host.to_owned())
        .map_err(|err| format!("{host}: {err}"))?;
    let tls = tokio_rustls::TlsConnector::from(Arc::new(config))
        .connect(name, tcp)
        .await
        .map_err(|err| format!("{host}: TLS: {err}"))?;
    let mut request =
        format!("wss://{host}/").into_client_request().map_err(|err| format!("{host}: {err}"))?;
    let headers = request.headers_mut();
    if let Some((id, secret)) = &tunnel.service_token {
        for (name, value) in [("cf-access-client-id", id), ("cf-access-client-secret", secret)] {
            if let Ok(value) = value.parse() {
                headers.insert(name, value);
            }
        }
    }
    headers.insert(
        "user-agent",
        concat!("artty/", env!("CARGO_PKG_VERSION")).parse().expect("a header"),
    );
    let (socket, _) = tokio_tungstenite::client_async(request, tls).await.map_err(|err| match err {
        tokio_tungstenite::tungstenite::Error::Http(response) => {
            let status = response.status();
            let access = response
                .headers()
                .get("location")
                .and_then(|l| l.to_str().ok())
                .is_some_and(|l| l.contains("cloudflareaccess.com"));
            if access || status == 403 {
                format!(
                    "{host}: Cloudflare Access wants a login ({status}): give cloudflared's command a service token (--service-token-id, --service-token-secret)"
                )
            } else {
                format!("{host}: the tunnel answered {status}")
            }
        },
        err => format!("{host}: {err}"),
    })?;

    // The socket's binary messages, as a stream (what russh reads and writes).
    let (ours, theirs) = tokio::io::duplex(256 * 1024);
    tokio::spawn(async move {
        let (mut sink, mut source) = socket.split();
        let (mut from_ssh, mut to_ssh) = tokio::io::split(theirs);
        let mut buffer = vec![0; 32 * 1024];
        loop {
            tokio::select! {
                message = source.next() => match message {
                    Some(Ok(Message::Binary(data))) => {
                        if to_ssh.write_all(&data).await.is_err() {
                            break;
                        }
                    },
                    Some(Ok(Message::Text(text))) => {
                        if to_ssh.write_all(text.as_bytes()).await.is_err() {
                            break;
                        }
                    },
                    // Pings are answered as the socket is next written or flushed.
                    Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_))) => {
                        let _ = sink.flush().await;
                    },
                    Some(Ok(Message::Close(_)) | Err(_)) | None => break,
                },
                read = from_ssh.read(&mut buffer) => match read {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if sink.send(Message::Binary(buffer[..n].to_vec().into())).await.is_err() {
                            break;
                        }
                    },
                },
            }
        }
        let _ = sink.close().await;
    });
    Ok(ours)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ssh_config::Config;

    fn tunnel_of(config: &str, host: &str) -> Option<Tunnel> {
        tunnel(&Config::parse(config, &|_| Vec::new()).resolve(host, &Local::default()))
    }

    #[test]
    fn cloudflared_commands() {
        let t = tunnel_of(
            "Host ssh.example.com\n  ProxyCommand cloudflared access ssh --hostname %h\n",
            "ssh.example.com",
        );
        assert_eq!(t, Some(Tunnel { hostname: "ssh.example.com".into(), service_token: None }));
        // A path, flags as Go takes them, a service token, a HostName.
        let t = tunnel_of(
            "Host box\n  HostName box.example.com\n  ProxyCommand /opt/homebrew/bin/cloudflared --loglevel warn access ssh -hostname=%h --service-token-id ID --service-token-secret=SECRET\n",
            "box",
        )
        .unwrap();
        assert_eq!(t.hostname, "box.example.com");
        assert_eq!(t.service_token, Some(("ID".into(), "SECRET".into())));
        assert_eq!(
            tunnel_of(
                "Host x\n  ProxyCommand cloudflared access ssh --hostname https://ssh.x.io/\n",
                "x"
            )
            .unwrap()
            .hostname,
            "ssh.x.io"
        );
        // Not cloudflared's ssh: not a tunnel.
        assert_eq!(tunnel_of("Host x\n  ProxyCommand nc %h %p\n", "x"), None);
        assert_eq!(
            tunnel_of("Host x\n  ProxyCommand cloudflared access tcp --hostname %h\n", "x"),
            None
        );
        assert_eq!(tunnel_of("Host x\n  ProxyCommand cloudflared access ssh\n", "x"), None);
    }
}
