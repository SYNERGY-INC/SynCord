//! WebSocket connection to the server, exposed to iced as a stream of events.

use std::time::Duration;

use futures::{SinkExt, Stream, StreamExt};
use celerity_proto::{ClientMsg, ServerMsg};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message as WsMessage;

use crate::voice::VoiceSink;

#[derive(Debug)]
enum Outgoing {
    Json(ClientMsg),
    Binary(Vec<u8>),
}

/// Handle used to send requests to the server. Dropping every clone closes the connection.
#[derive(Debug, Clone)]
pub struct Connection(mpsc::UnboundedSender<Outgoing>);

impl Connection {
    pub fn send(&self, msg: ClientMsg) {
        let _ = self.0.send(Outgoing::Json(msg));
    }

    /// Send a voice packet over the WebSocket (used when UDP can't get through).
    pub fn send_binary(&self, packet: Vec<u8>) {
        let _ = self.0.send(Outgoing::Binary(packet));
    }
}

#[derive(Debug, Clone)]
pub enum Event {
    Connected {
        conn: Connection,
        /// The address actually used (`wss://...` when encrypted).
        url: String,
        /// The server certificate's fingerprint, when the connection is encrypted.
        fingerprint: Option<String>,
    },
    Received(Box<ServerMsg>),
    Disconnected(String),
    /// A server we know showed a different certificate than last time: refused.
    IdentityChanged {
        seen: String,
    },
}

/// Addresses to try for what the person typed: encrypted first, then plain for
/// servers older than 0.5 (unless they typed `ws://`/`wss://` themselves).
pub fn candidates(input: &str) -> Vec<String> {
    let url = server_url(input);
    let lower = input.trim().to_ascii_lowercase();
    if lower.starts_with("ws://")
        || lower.starts_with("http://")
        || lower.starts_with("wss://")
        || lower.starts_with("https://")
    {
        return vec![url];
    }
    vec![url.replacen("ws://", "wss://", 1), url]
}

/// Key under which a server's certificate is remembered: its host and port.
pub fn server_key(input: &str) -> String {
    let url = server_url(input);
    let rest = url.split_once("://").map_or(url.as_str(), |(_, r)| r);
    rest.split('/').next().unwrap_or(rest).to_ascii_lowercase()
}

/// Why connecting failed, and whether trying without encryption makes sense.
enum Failure {
    /// Couldn't reach the server at all (wrong address, server off, timed out).
    Unreachable(String),
    /// Reached it, but the encrypted handshake didn't work (e.g. a server older than 0.5).
    Handshake(String),
    IdentityChanged,
}

fn classify(e: tokio_tungstenite::tungstenite::Error) -> Failure {
    use std::io::ErrorKind::*;
    use tokio_tungstenite::tungstenite::Error;
    let text = e.to_string();
    if text.contains(crate::tls::IDENTITY_CHANGED) {
        return Failure::IdentityChanged;
    }
    match e {
        Error::Io(io)
            if matches!(
                io.kind(),
                ConnectionRefused | TimedOut | HostUnreachable | NetworkUnreachable | AddrNotAvailable
            ) =>
        {
            Failure::Unreachable(text)
        }
        Error::Url(_) => Failure::Unreachable(text),
        _ => Failure::Handshake(text),
    }
}

type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn open(url: &str, pin: Option<String>) -> Result<(Ws, Option<String>), Failure> {
    let tls = crate::tls::config(pin);
    let connector = tokio_tungstenite::Connector::Rustls(tls.config.clone());
    let connecting = tokio_tungstenite::connect_async_tls_with_config(url, None, true, Some(connector));
    match tokio::time::timeout(Duration::from_secs(10), connecting).await {
        Ok(Ok((ws, _))) => {
            let fingerprint = url.starts_with("wss://").then(|| tls.seen.lock().unwrap().clone()).flatten();
            Ok((ws, fingerprint))
        }
        Ok(Err(e)) => Err(classify(e)),
        Err(_) => Err(Failure::Unreachable("timed out".into())),
    }
}

/// Turn what a person types ("192.168.1.20", "myhost:7878", "wss://chat.example.com")
/// into a WebSocket URL.
pub fn server_url(input: &str) -> String {
    let input = input.trim().trim_end_matches('/');
    let (scheme, rest) = if let Some(r) = input.strip_prefix("wss://") {
        ("wss", r)
    } else if let Some(r) = input.strip_prefix("https://") {
        ("wss", r)
    } else if let Some(r) = input.strip_prefix("ws://") {
        ("ws", r)
    } else if let Some(r) = input.strip_prefix("http://") {
        ("ws", r)
    } else {
        ("ws", input)
    };
    let (host, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/ws"),
    };
    // Add the default port when none is given (and it isn't an IPv6 literal without a port).
    let has_port = host.rsplit_once(':').is_some_and(|(h, p)| {
        !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()) && (!h.contains(':') || h.ends_with(']'))
    });
    let host = if has_port || scheme == "wss" { host.to_string() } else { format!("{host}:7878") };
    format!("{scheme}://{host}{path}")
}

/// The host name or IP address in a WebSocket URL from [`server_url`].
pub fn host_of(url: &str) -> Option<String> {
    let rest = url.split_once("://")?.1;
    let authority = rest.split('/').next()?;
    let host = if let Some(stripped) = authority.strip_prefix('[') {
        stripped.split(']').next()?
    } else {
        authority.rsplit_once(':').map_or(authority, |(h, _)| h)
    };
    (!host.is_empty()).then(|| host.to_string())
}

/// Connect to the server the person typed, and deliver voice packets that arrive
/// over the WebSocket to `voice`. `pin` is the certificate fingerprint remembered
/// for this server: with one, the server must show that certificate and plain
/// (unencrypted) connections are never used.
pub fn connect(input: String, pin: Option<String>, voice: VoiceSink) -> impl Stream<Item = Event> {
    iced::stream::channel(64, async move |mut output: futures::channel::mpsc::Sender<Event>| {
        let mut opened = None;
        let mut last_error = String::new();
        for url in candidates(&input) {
            let encrypted = url.starts_with("wss://");
            if !encrypted && pin.is_some() {
                break; // This server encrypts: never fall back to plain.
            }
            match open(&url, pin.clone()).await {
                Ok((ws, fingerprint)) => {
                    opened = Some((ws, url, fingerprint));
                    break;
                }
                Err(Failure::IdentityChanged) => {
                    let seen = probe_fingerprint(&url).await.unwrap_or_default();
                    let _ = output.send(Event::IdentityChanged { seen }).await;
                    return;
                }
                Err(Failure::Unreachable(e)) => {
                    last_error = format!("Couldn't connect to {}: {e}", display_address(&url));
                    break;
                }
                Err(Failure::Handshake(e)) => {
                    last_error = format!("Couldn't connect to {}: {e}", display_address(&url));
                }
            }
        }
        let Some((ws, url, fingerprint)) = opened else {
            let _ = output.send(Event::Disconnected(last_error)).await;
            return;
        };

        let (tx, mut rx) = mpsc::unbounded_channel();
        let _ = output.send(Event::Connected { conn: Connection(tx), url, fingerprint }).await;

        let (mut sink, mut stream) = ws.split();
        let mut keepalive = tokio::time::interval(Duration::from_secs(30));
        keepalive.tick().await;

        let reason = loop {
            tokio::select! {
                outgoing = rx.recv() => match outgoing {
                    Some(msg) => {
                        let frame = match msg {
                            Outgoing::Json(m) => WsMessage::Text(serde_json::to_string(&m).expect("serialize").into()),
                            Outgoing::Binary(b) => WsMessage::Binary(b.into()),
                        };
                        if let Err(e) = sink.send(frame).await {
                            break format!("Connection lost: {e}");
                        }
                    }
                    // Every `Connection` handle was dropped: we're done.
                    None => {
                        let _ = sink.send(WsMessage::Close(None)).await;
                        return;
                    }
                },
                incoming = stream.next() => match incoming {
                    Some(Ok(WsMessage::Text(text))) => match serde_json::from_str::<ServerMsg>(&text) {
                        Ok(msg) => {
                            if output.send(Event::Received(Box::new(msg))).await.is_err() {
                                return;
                            }
                        }
                        Err(e) => eprintln!("ignoring unknown server message: {e}"),
                    },
                    Some(Ok(WsMessage::Binary(data))) => {
                        // Straight to the video decoders or the audio mixer: neither goes through the UI thread.
                        if crate::live::deliver(&data) {
                        } else if let Some(mixer) = voice.lock().unwrap().as_ref() {
                            mixer.push_packet(&data);
                        }
                    }
                    Some(Ok(WsMessage::Close(_))) | None => break "The server closed the connection".to_string(),
                    Some(Ok(_)) => {}
                    Some(Err(e)) => break format!("Connection lost: {e}"),
                },
                _ = keepalive.tick() => {
                    let json = serde_json::to_string(&ClientMsg::Ping).unwrap();
                    if sink.send(WsMessage::Text(json.into())).await.is_err() {
                        break "Connection lost".to_string();
                    }
                }
            }
        };
        let _ = output.send(Event::Disconnected(reason)).await;
    })
}

/// "host:port" for messages (no scheme or path).
fn display_address(url: &str) -> String {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    rest.split('/').next().unwrap_or(rest).to_string()
}

/// Fingerprint of the certificate a server shows, without trusting it (for the "identity changed" screen).
pub async fn probe_fingerprint(url: &str) -> Option<String> {
    let tls = crate::tls::config(None);
    let connector = tokio_tungstenite::Connector::Rustls(tls.config.clone());
    let attempt = tokio_tungstenite::connect_async_tls_with_config(url, None, true, Some(connector));
    let _ = tokio::time::timeout(Duration::from_secs(10), attempt).await;
    tls.seen.lock().unwrap().clone()
}

#[cfg(test)]
mod tests {
    use super::server_url;

    async fn start(encrypted: bool) -> (String, Option<String>, std::path::PathBuf) {
        use celerity_server::{AppState, Config, db::Db, router};
        let _ = rustls::crypto::ring::default_provider().install_default();
        let state = AppState::new(Config::default(), Db::open_in_memory().unwrap());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let dir =
            std::env::temp_dir().join(format!("celerity-net-test-{}-{}", std::process::id(), address.replace(':', "-")));
        std::fs::create_dir_all(&dir).unwrap();
        if encrypted {
            let (tls, print) = celerity_server::tls::load_or_create(&dir).unwrap();
            let dual = celerity_server::tls::DualListener::new(listener, tls).unwrap();
            tokio::spawn(async move { axum::serve(dual, router(state)).await.unwrap() });
            (address, Some(print), dir)
        } else {
            tokio::spawn(async move { axum::serve(listener, router(state)).await.unwrap() });
            (address, None, dir)
        }
    }

    async fn first_event(address: &str, pin: Option<String>) -> super::Event {
        use futures::StreamExt;
        let mut events = Box::pin(super::connect(address.to_string(), pin, Default::default()));
        events.next().await.unwrap()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn trust_on_first_use_and_pinning() {
        use super::Event;
        let (address, print, dir) = start(true).await;
        let print = print.unwrap();

        // First time: encrypted, and we learn the fingerprint the server logs.
        let Event::Connected { url, fingerprint, .. } = first_event(&address, None).await else { panic!() };
        assert!(url.starts_with("wss://"));
        assert_eq!(fingerprint.as_deref(), Some(print.as_str()));

        // Remembered fingerprint: connects.
        assert!(matches!(first_event(&address, Some(print.clone())).await, Event::Connected { .. }));

        // A different remembered fingerprint: refused, showing what the server presented.
        let Event::IdentityChanged { seen } = first_event(&address, Some("AA:BB".into())).await else {
            panic!("should refuse")
        };
        assert_eq!(seen, print);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn old_servers_fall_back_to_plain_but_known_ones_never_do() {
        use super::Event;
        let (address, _, dir) = start(false).await;
        // A server older than 0.5: plain connection, no fingerprint.
        let Event::Connected { url, fingerprint, .. } = first_event(&address, None).await else { panic!() };
        assert!(url.starts_with("ws://"), "{url}");
        assert_eq!(fingerprint, None);
        // A server we know encrypts must never be reached without encryption.
        assert!(matches!(first_event(&address, Some("AA:BB".into())).await, Event::Disconnected(_)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn candidates_and_keys() {
        use super::{candidates, server_key};
        assert_eq!(candidates("192.168.1.20"), ["wss://192.168.1.20:7878/ws", "ws://192.168.1.20:7878/ws"]);
        assert_eq!(candidates("ws://box:9000"), ["ws://box:9000/ws"]);
        assert_eq!(candidates("https://chat.example.com"), ["wss://chat.example.com/ws"]);
        assert_eq!(server_key("Box"), "box:7878");
        assert_eq!(server_key("wss://Chat.Example.com"), "chat.example.com");
    }

    #[test]
    fn urls() {
        assert_eq!(server_url("192.168.1.20"), "ws://192.168.1.20:7878/ws");
        assert_eq!(server_url("box:9000"), "ws://box:9000/ws");
        assert_eq!(server_url("http://box:9000/"), "ws://box:9000/ws");
        assert_eq!(server_url("wss://chat.example.com"), "wss://chat.example.com/ws");
        assert_eq!(server_url("ws://h:1/custom"), "ws://h:1/custom");
        assert_eq!(server_url("[::1]:7878"), "ws://[::1]:7878/ws");
    }

    #[test]
    fn hosts() {
        use super::host_of;
        assert_eq!(host_of("ws://192.168.1.20:7878/ws").as_deref(), Some("192.168.1.20"));
        assert_eq!(host_of("wss://chat.example.com/ws").as_deref(), Some("chat.example.com"));
        assert_eq!(host_of("ws://[::1]:7878/ws").as_deref(), Some("::1"));
    }
}
