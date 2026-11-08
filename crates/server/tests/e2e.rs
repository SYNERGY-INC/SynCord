//! End-to-end test: start a real server on a random port and drive it with
//! two WebSocket clients, the same way the desktop app does.

use std::time::Duration;

use futures::{SinkExt, StreamExt};
use syncord_proto::{ClientMsg, Profile, ServerMsg};
use syncord_server::{AppState, Config, db::Db, router};
use tokio::net::TcpStream;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message as Ws};

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

async fn start_server() -> String {
    let config = Config { invite_code: "letmein".into(), community_name: "Test Crew".into(), ..Config::default() };
    let state = AppState::new(config, Db::open_in_memory().unwrap());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router(state)).await.unwrap() });
    format!("ws://{addr}/ws")
}

async fn send(ws: &mut Socket, msg: ClientMsg) {
    ws.send(Ws::Text(serde_json::to_string(&msg).unwrap().into())).await.unwrap();
}

/// Read frames until one matches `pred`, skipping unrelated broadcasts.
async fn expect(ws: &mut Socket, pred: impl Fn(&ServerMsg) -> bool) -> ServerMsg {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let frame = ws.next().await.expect("socket closed").unwrap();
            if let Ws::Text(t) = frame {
                let msg: ServerMsg = serde_json::from_str(&t).unwrap();
                if pred(&msg) {
                    return msg;
                }
            }
        }
    })
    .await
    .expect("timed out waiting for server message")
}

async fn register(url: &str, name: &str, invite: &str) -> (Socket, ServerMsg) {
    let (mut ws, _) = connect_async(url).await.unwrap();
    send(
        &mut ws,
        ClientMsg::Register { username: name.into(), password: "correct horse".into(), invite: invite.into() },
    )
    .await;
    let reply = expect(&mut ws, |m| matches!(m, ServerMsg::Ready(_) | ServerMsg::AuthFailed { .. })).await;
    (ws, reply)
}

#[tokio::test]
async fn full_flow() {
    let url = start_server().await;

    // Wrong invite code is rejected.
    let (_, reply) = register(&url, "mallory", "nope").await;
    assert!(matches!(reply, ServerMsg::AuthFailed { .. }));

    // First account becomes owner.
    let (mut alice, reply) = register(&url, "alice", "letmein").await;
    let ServerMsg::Ready(ready) = reply else { panic!("expected Ready, got {reply:?}") };
    assert_eq!(ready.community_name, "Test Crew");
    let alice_id = ready.me;
    let general = ready.channels[0].id;
    assert!(ready.users.iter().any(|u| u.id == alice_id && u.is_owner));
    let alice_token = ready.token.clone();

    // Second account: not owner, sees alice online.
    let (mut bob, reply) = register(&url, "bob", "letmein").await;
    let ServerMsg::Ready(ready) = reply else { panic!() };
    let bob_id = ready.me;
    assert!(ready.online.contains(&alice_id));
    assert!(!ready.users.iter().find(|u| u.id == bob_id).unwrap().is_owner);
    expect(&mut alice, |m| matches!(m, ServerMsg::Presence { user_id, online: true } if *user_id == bob_id)).await;

    // Duplicate usernames (any case) are rejected.
    let (_, reply) = register(&url, "ALICE", "letmein").await;
    assert!(matches!(reply, ServerMsg::AuthFailed { .. }));

    // Bob sends a message; both see it.
    send(&mut bob, ClientMsg::SendMessage { channel_id: general, content: "  hello alice  ".into() }).await;
    let created = expect(&mut alice, |m| matches!(m, ServerMsg::MessageCreated(_))).await;
    let ServerMsg::MessageCreated(msg) = created else { unreachable!() };
    assert_eq!(msg.content, "hello alice");
    assert_eq!(msg.author_id, bob_id);
    expect(&mut bob, |m| matches!(m, ServerMsg::MessageCreated(_))).await;

    // Alice can't edit Bob's message, Bob can.
    send(&mut alice, ClientMsg::EditMessage { message_id: msg.id, content: "hacked".into() }).await;
    expect(&mut alice, |m| matches!(m, ServerMsg::Error { .. })).await;
    send(&mut bob, ClientMsg::EditMessage { message_id: msg.id, content: "hello there".into() }).await;
    let updated = expect(&mut alice, |m| matches!(m, ServerMsg::MessageUpdated(_))).await;
    let ServerMsg::MessageUpdated(m) = updated else { unreachable!() };
    assert_eq!(m.content, "hello there");
    assert!(m.edited_at.is_some());

    // Only the owner can create channels.
    send(&mut bob, ClientMsg::CreateChannel { name: "bob's room".into(), topic: String::new() }).await;
    expect(&mut bob, |m| matches!(m, ServerMsg::Error { .. })).await;
    send(&mut alice, ClientMsg::CreateChannel { name: "Gaming Night".into(), topic: "gg".into() }).await;
    let created = expect(&mut bob, |m| matches!(m, ServerMsg::ChannelCreated(_))).await;
    let ServerMsg::ChannelCreated(ch) = created else { unreachable!() };
    assert_eq!(ch.name, "gaming-night");

    // Profile changes are broadcast (and sanitized).
    let profile = Profile { display_name: "Bobby".into(), accent_color: 0xFF00FF00, ..Profile::default() };
    send(&mut bob, ClientMsg::UpdateProfile { profile }).await;
    let updated = expect(&mut alice, |m| matches!(m, ServerMsg::UserUpdated(_))).await;
    let ServerMsg::UserUpdated(u) = updated else { unreachable!() };
    assert_eq!(u.name(), "Bobby");
    assert_eq!(u.profile.accent_color, 0x00FF00);

    // History contains the message.
    send(&mut alice, ClientMsg::FetchHistory { channel_id: general, before: None }).await;
    let hist = expect(&mut alice, |m| matches!(m, ServerMsg::History { .. })).await;
    let ServerMsg::History { messages, reached_start, .. } = hist else { unreachable!() };
    assert_eq!(messages.len(), 1);
    assert!(reached_start);

    // Owner deletes Bob's message.
    send(&mut alice, ClientMsg::DeleteMessage { message_id: msg.id }).await;
    expect(&mut bob, |m| matches!(m, ServerMsg::MessageDeleted { message_id, .. } if *message_id == msg.id)).await;

    // Bob disconnects -> alice sees him go offline.
    drop(bob);
    expect(&mut alice, |m| matches!(m, ServerMsg::Presence { user_id, online: false } if *user_id == bob_id)).await;

    // A saved token logs back in; after logout it no longer works.
    let (mut again, _) = connect_async(&url).await.unwrap();
    send(&mut again, ClientMsg::Resume { token: alice_token.clone() }).await;
    expect(&mut again, |m| matches!(m, ServerMsg::Ready(_))).await;
    send(&mut again, ClientMsg::Logout).await;
    let (mut after, _) = connect_async(&url).await.unwrap();
    // Give the server a moment to process the logout.
    tokio::time::sleep(Duration::from_millis(200)).await;
    send(&mut after, ClientMsg::Resume { token: alice_token }).await;
    expect(&mut after, |m| matches!(m, ServerMsg::AuthFailed { .. })).await;

    // Requests before logging in are refused.
    send(&mut after, ClientMsg::SendMessage { channel_id: general, content: "sneaky".into() }).await;
    expect(&mut after, |m| matches!(m, ServerMsg::AuthFailed { .. })).await;
}
