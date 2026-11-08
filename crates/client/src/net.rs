//! WebSocket connection to the server, exposed to iced as a stream of events.

use std::time::Duration;

use futures::{SinkExt, Stream, StreamExt};
use syncord_proto::{ClientMsg, ServerMsg};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message as WsMessage;

/// Handle used to send requests to the server. Dropping every clone closes the connection.
#[derive(Debug, Clone)]
pub struct Connection(mpsc::UnboundedSender<ClientMsg>);

impl Connection {
    pub fn send(&self, msg: ClientMsg) {
        let _ = self.0.send(msg);
    }
}

#[derive(Debug, Clone)]
pub enum Event {
    Connected(Connection),
    Received(Box<ServerMsg>),
    Disconnected(String),
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

pub fn connect(url: String) -> impl Stream<Item = Event> {
    iced::stream::channel(64, async move |mut output: futures::channel::mpsc::Sender<Event>| {
        let ws = match tokio::time::timeout(Duration::from_secs(10), tokio_tungstenite::connect_async(&url)).await {
            Ok(Ok((ws, _))) => ws,
            Ok(Err(e)) => {
                let _ = output.send(Event::Disconnected(format!("Couldn't connect to {url}: {e}"))).await;
                return;
            }
            Err(_) => {
                let _ = output.send(Event::Disconnected(format!("Timed out connecting to {url}"))).await;
                return;
            }
        };

        let (tx, mut rx) = mpsc::unbounded_channel();
        let _ = output.send(Event::Connected(Connection(tx))).await;

        let (mut sink, mut stream) = ws.split();
        let mut keepalive = tokio::time::interval(Duration::from_secs(30));
        keepalive.tick().await;

        let reason = loop {
            tokio::select! {
                outgoing = rx.recv() => match outgoing {
                    Some(msg) => {
                        let json = serde_json::to_string(&msg).expect("serialize");
                        if let Err(e) = sink.send(WsMessage::Text(json.into())).await {
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

#[cfg(test)]
mod tests {
    use super::server_url;

    #[test]
    fn urls() {
        assert_eq!(server_url("192.168.1.20"), "ws://192.168.1.20:7878/ws");
        assert_eq!(server_url("box:9000"), "ws://box:9000/ws");
        assert_eq!(server_url("http://box:9000/"), "ws://box:9000/ws");
        assert_eq!(server_url("wss://chat.example.com"), "wss://chat.example.com/ws");
        assert_eq!(server_url("ws://h:1/custom"), "ws://h:1/custom");
        assert_eq!(server_url("[::1]:7878"), "ws://[::1]:7878/ws");
    }
}
