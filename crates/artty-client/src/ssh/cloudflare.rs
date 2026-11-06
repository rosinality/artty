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

/// The most sent in one WebSocket message.
const MESSAGE: usize = 4 * 1024;

/// Where a `cloudflared access ssh` command goes (read by `proxy`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tunnel {
    /// The tunnel's public hostname for the machine's ssh.
    pub hostname: String,
    /// Cloudflare Access's service token (client id and secret), if the
    /// command gives one (`--service-token-id`, `--service-token-secret`).
    pub service_token: Option<(String, String)>,
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
        // Messages of cloudflared's size (its WebSocket's 4 KiB frames): the
        // tunnel stalls on bigger ones once a few hundred KiB are in flight
        // (an upload stopped after its first 8 KiB arrived).
        let mut buffer = vec![0; MESSAGE];
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
