//! End-to-end test: start a real server on a random port and drive it with
//! two WebSocket clients, the same way the desktop app does.

use std::time::Duration;

use futures::{SinkExt, StreamExt};
use syncord_proto::voice::{Packet, token_from_hex};
use syncord_proto::{ChannelKind, ClientMsg, Profile, ServerMsg};
use syncord_server::{AppState, Config, db::Db, router, start_udp};
use tokio::net::TcpStream;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message as Ws};

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

async fn start_server() -> String {
    start_server_with_udp().await.0
}

/// Returns the WebSocket URL and the UDP voice port.
async fn start_server_with_udp() -> (String, u16) {
    let uploads = std::env::temp_dir().join(format!("syncord-test-{}", rand_suffix()));
    let config =
        Config { invite_code: "letmein".into(), community_name: "Test Crew".into(), uploads, ..Config::default() };
    let state = AppState::new(config, Db::open_in_memory().unwrap());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let udp_port = start_udp(&state, "127.0.0.1:0".parse().unwrap()).await.unwrap();
    tokio::spawn(async move { axum::serve(listener, router(state)).await.unwrap() });
    (format!("ws://{addr}/ws"), udp_port)
}

fn rand_suffix() -> u64 {
    use std::hash::{BuildHasher, RandomState};
    RandomState::new().hash_one(std::time::SystemTime::now())
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
    let general = ready.channels.iter().find(|c| c.kind == ChannelKind::Text).unwrap().id;
    assert!(ready.channels.iter().any(|c| c.kind == ChannelKind::Voice), "fresh servers get a voice channel");
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
    send(
        &mut bob,
        ClientMsg::SendMessage { channel_id: general, content: "  hello alice  ".into(), attachments: vec![] },
    )
    .await;
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
    send(
        &mut bob,
        ClientMsg::CreateChannel { name: "bob's room".into(), topic: String::new(), kind: ChannelKind::Text },
    )
    .await;
    expect(&mut bob, |m| matches!(m, ServerMsg::Error { .. })).await;
    send(
        &mut alice,
        ClientMsg::CreateChannel { name: "Gaming Night".into(), topic: "gg".into(), kind: ChannelKind::Text },
    )
    .await;
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
    send(&mut after, ClientMsg::SendMessage { channel_id: general, content: "sneaky".into(), attachments: vec![] })
        .await;
    expect(&mut after, |m| matches!(m, ServerMsg::AuthFailed { .. })).await;
}

async fn next_binary(ws: &mut Socket) -> Vec<u8> {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ws::Binary(b) = ws.next().await.expect("socket closed").unwrap() {
                return b.to_vec();
            }
        }
    })
    .await
    .expect("timed out waiting for voice packet")
}

#[tokio::test]
async fn voice_relay_udp_and_websocket() {
    let (url, udp_port) = start_server_with_udp().await;
    let (mut alice, ready) = register(&url, "alice", "letmein").await;
    let ServerMsg::Ready(ready) = ready else { panic!() };
    let voice = ready.channels.iter().find(|c| c.kind == ChannelKind::Voice).unwrap().id;
    let text = ready.channels.iter().find(|c| c.kind == ChannelKind::Text).unwrap().id;
    let alice_id = ready.me;
    let (mut bob, _) = register(&url, "bob", "letmein").await;

    // Text messages can't go to a voice channel, and you can't "join" a text channel.
    send(&mut alice, ClientMsg::SendMessage { channel_id: voice, content: "hi".into(), attachments: vec![] }).await;
    expect(&mut alice, |m| matches!(m, ServerMsg::Error { .. })).await;
    send(&mut alice, ClientMsg::JoinVoice { channel_id: text, encrypted: false }).await;
    expect(&mut alice, |m| matches!(m, ServerMsg::Error { .. })).await;

    // Both join.
    send(&mut alice, ClientMsg::JoinVoice { channel_id: voice, encrypted: false }).await;
    let ServerMsg::VoiceReady { token: alice_token, udp_port: port, ssrc: alice_ssrc, .. } =
        expect(&mut alice, |m| matches!(m, ServerMsg::VoiceReady { .. })).await
    else {
        unreachable!()
    };
    assert_eq!(port, udp_port);
    let alice_token = token_from_hex(&alice_token).unwrap();
    expect(&mut bob, |m| matches!(m, ServerMsg::VoiceStateUpdate { user_id, state: Some(_) } if *user_id == alice_id))
        .await;

    send(&mut bob, ClientMsg::JoinVoice { channel_id: voice, encrypted: false }).await;
    let ServerMsg::VoiceReady { token: bob_token, ssrc: bob_ssrc, .. } =
        expect(&mut bob, |m| matches!(m, ServerMsg::VoiceReady { .. })).await
    else {
        unreachable!()
    };
    let bob_token = token_from_hex(&bob_token).unwrap();

    // Bob opens a UDP path; Alice stays WebSocket-only.
    let udp = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    udp.connect(("127.0.0.1", udp_port)).await.unwrap();
    udp.send(&Packet::Hello { token: bob_token }.encode()).await.unwrap();
    let mut buf = [0u8; 1500];
    let n = tokio::time::timeout(Duration::from_secs(5), udp.recv(&mut buf)).await.unwrap().unwrap();
    assert_eq!(Packet::parse(&buf[..n]), Some(Packet::Ack));

    // Alice talks over the WebSocket -> Bob hears it over UDP.
    let up = Packet::AudioUp { token: alice_token, seq: 7, opus: b"alice-audio" }.encode();
    alice.send(Ws::Binary(up.into())).await.unwrap();
    let n = tokio::time::timeout(Duration::from_secs(5), udp.recv(&mut buf)).await.unwrap().unwrap();
    assert_eq!(Packet::parse(&buf[..n]), Some(Packet::AudioDown { ssrc: alice_ssrc, seq: 7, opus: b"alice-audio" }));

    // Bob talks over UDP -> Alice hears it over her WebSocket.
    udp.send(&Packet::AudioUp { token: bob_token, seq: 1, opus: b"bob-audio" }.encode()).await.unwrap();
    let got = next_binary(&mut alice).await;
    assert_eq!(Packet::parse(&got), Some(Packet::AudioDown { ssrc: bob_ssrc, seq: 1, opus: b"bob-audio" }));

    // Bob mutes: his audio is no longer relayed, and everyone sees the state.
    send(&mut bob, ClientMsg::SetVoiceState { muted: true, deafened: false }).await;
    expect(&mut alice, |m| matches!(m, ServerMsg::VoiceStateUpdate { state: Some(s), .. } if s.muted)).await;
    udp.send(&Packet::AudioUp { token: bob_token, seq: 2, opus: b"secret" }.encode()).await.unwrap();
    send(&mut alice, ClientMsg::Ping).await;
    expect(&mut alice, |m| matches!(m, ServerMsg::Pong)).await;
    let leaked = tokio::time::timeout(Duration::from_millis(300), next_binary(&mut alice)).await;
    assert!(leaked.is_err(), "muted audio must not be relayed");

    // Bob disconnecting removes him from voice.
    drop(bob);
    expect(&mut alice, |m| matches!(m, ServerMsg::VoiceStateUpdate { state: None, .. })).await;

    // Late joiners see who's already in voice.
    let (_carol, ready) = register(&url, "carol", "letmein").await;
    let ServerMsg::Ready(ready) = ready else { panic!() };
    assert_eq!(ready.voice.len(), 1);
    assert_eq!(ready.voice[0].user_id, alice_id);
}

fn http_base(ws_url: &str) -> String {
    ws_url.replace("ws://", "http://").trim_end_matches("/ws").to_string()
}

fn png_bytes(w: u32, h: u32) -> Vec<u8> {
    let img = image::RgbaImage::from_pixel(w, h, image::Rgba([200, 100, 50, 255]));
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png).unwrap();
    out.into_inner()
}

/// Read events until one matches, failing if any event matches `forbidden` first.
async fn expect_not_before(
    ws: &mut Socket,
    forbidden: impl Fn(&ServerMsg) -> bool,
    pred: impl Fn(&ServerMsg) -> bool,
) -> ServerMsg {
    let msg = expect(ws, |m| forbidden(m) || pred(m)).await;
    assert!(!forbidden(&msg), "received something private: {msg:?}");
    msg
}

#[tokio::test]
async fn direct_messages_files_and_reactions() {
    let url = start_server().await;
    let base = http_base(&url);
    let _ = rustls::crypto::ring::default_provider().install_default();
    let http = reqwest::Client::new();

    let (mut alice, r) = register(&url, "alice", "letmein").await;
    let ServerMsg::Ready(ready) = r else { panic!() };
    let (alice_id, alice_token) = (ready.me, ready.token.clone());
    let general = ready.channels.iter().find(|c| c.kind == ChannelKind::Text).unwrap().id;
    let voice = ready.channels.iter().find(|c| c.kind == ChannelKind::Voice).unwrap().id;
    let (mut bob, r) = register(&url, "bob", "letmein").await;
    let ServerMsg::Ready(ready) = r else { panic!() };
    let (bob_id, bob_token) = (ready.me, ready.token.clone());
    let (mut eve, r) = register(&url, "eve", "letmein").await;
    let ServerMsg::Ready(ready) = r else { panic!() };
    let eve_token = ready.token.clone();

    // Alice opens a DM with Bob: both learn about it, from their own point of view.
    send(&mut alice, ClientMsg::OpenDirect { user_id: bob_id }).await;
    let ServerMsg::DirectOpened(dm) = expect(&mut alice, |m| matches!(m, ServerMsg::DirectOpened(_))).await else {
        unreachable!()
    };
    assert_eq!(dm.user_id, bob_id);
    let ServerMsg::DirectOpened(theirs) = expect(&mut bob, |m| matches!(m, ServerMsg::DirectOpened(_))).await else {
        unreachable!()
    };
    assert_eq!((theirs.id, theirs.user_id), (dm.id, alice_id));

    // Eve can't read it.
    send(&mut eve, ClientMsg::FetchHistory { channel_id: dm.id, before: None }).await;
    expect(&mut eve, |m| matches!(m, ServerMsg::Error { .. })).await;

    // Upload rules.
    let upload = |token: &str, channel: i64, name: &str, body: Vec<u8>| {
        http.post(format!("{base}{}?channel_id={channel}&filename={name}", syncord_proto::UPLOAD_PATH))
            .header("Authorization", format!("Bearer {token}"))
            .body(body)
            .send()
    };
    let r = http
        .post(format!("{base}{}?channel_id={}&filename=x.png", syncord_proto::UPLOAD_PATH, dm.id))
        .body(png_bytes(4, 4))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401, "no token");
    assert_eq!(upload(&eve_token, dm.id, "x.png", png_bytes(4, 4)).await.unwrap().status(), 403, "not a member");
    assert_eq!(upload(&alice_token, voice, "x.png", png_bytes(4, 4)).await.unwrap().status(), 403, "voice channel");

    let r = upload(&alice_token, dm.id, "../../cat.png", png_bytes(1200, 900)).await.unwrap();
    assert_eq!(r.status(), 200);
    let att: syncord_proto::Attachment = serde_json::from_slice(&r.bytes().await.unwrap()).unwrap();
    assert_eq!(att.filename, "cat.png");
    assert_eq!((att.width, att.height), (Some(1200), Some(900)));

    let download = |token: &str, id: &str, thumb: bool| {
        http.get(format!("{base}{}/{id}{}", syncord_proto::FILES_PATH, if thumb { "?thumb=1" } else { "" }))
            .header("Authorization", format!("Bearer {token}"))
            .send()
    };
    assert_eq!(download(&bob_token, &att.id, false).await.unwrap().status(), 404, "not sent yet");
    assert_eq!(download(&alice_token, &att.id, false).await.unwrap().status(), 200);
    assert_eq!(download(&alice_token, "../../etc/passwd", false).await.unwrap().status(), 404);

    // Alice sends it in the DM. Bob gets it; Eve never sees it.
    let msg = ClientMsg::SendMessage { channel_id: dm.id, content: "look".into(), attachments: vec![att.id.clone()] };
    send(&mut alice, msg).await;
    let ServerMsg::MessageCreated(m) = expect(&mut bob, |m| matches!(m, ServerMsg::MessageCreated(_))).await else {
        unreachable!()
    };
    assert_eq!(m.attachments.len(), 1);
    send(&mut alice, ClientMsg::SendMessage { channel_id: general, content: "public".into(), attachments: vec![] })
        .await;
    let is_private = |x: &ServerMsg| matches!(x, ServerMsg::MessageCreated(m) if m.channel_id == dm.id);
    expect_not_before(&mut eve, is_private, |x| matches!(x, ServerMsg::MessageCreated(m) if m.content == "public"))
        .await;

    // Files: members can download (preview is a small image), others can't.
    let r = download(&bob_token, &att.id, true).await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["content-type"], "image/jpeg");
    let thumb = image::load_from_memory(&r.bytes().await.unwrap()).unwrap();
    assert_eq!((thumb.width(), thumb.height()), (480, 360));
    assert_eq!(download(&eve_token, &att.id, false).await.unwrap().status(), 404);

    // Reactions reach the members only.
    send(&mut bob, ClientMsg::AddReaction { message_id: m.id, emoji: "👍".into() }).await;
    expect(&mut alice, |x| matches!(x, ServerMsg::ReactionUpdate { added: true, .. })).await;
    send(&mut alice, ClientMsg::SendMessage { channel_id: general, content: "public2".into(), attachments: vec![] })
        .await;
    let private_reaction = |x: &ServerMsg| matches!(x, ServerMsg::ReactionUpdate { .. });
    expect_not_before(
        &mut eve,
        private_reaction,
        |x| matches!(x, ServerMsg::MessageCreated(m) if m.content == "public2"),
    )
    .await;
    send(&mut alice, ClientMsg::AddReaction { message_id: m.id, emoji: "not valid".into() }).await;
    expect(&mut alice, |x| matches!(x, ServerMsg::Error { .. })).await;

    // Eve can't react to or delete DM messages; the server owner (Alice) can't delete Bob's DM messages either.
    send(&mut eve, ClientMsg::AddReaction { message_id: m.id, emoji: "😈".into() }).await;
    send(&mut eve, ClientMsg::DeleteMessage { message_id: m.id }).await;
    expect(&mut eve, |x| matches!(x, ServerMsg::Error { .. })).await;

    // Read markers come back after reconnecting, with the DM list.
    send(&mut bob, ClientMsg::MarkRead { channel_id: dm.id, message_id: m.id }).await;
    send(&mut bob, ClientMsg::Ping).await;
    expect(&mut bob, |x| matches!(x, ServerMsg::Pong)).await;
    let (mut again, _) = connect_async(&url).await.unwrap();
    send(&mut again, ClientMsg::Resume { token: bob_token.clone() }).await;
    let ServerMsg::Ready(r) = expect(&mut again, |x| matches!(x, ServerMsg::Ready(_))).await else { unreachable!() };
    assert_eq!(r.directs.len(), 1);
    assert_eq!(r.directs[0].user_id, alice_id);
    assert_eq!(r.directs[0].last_message_id, Some(m.id));
    assert!(r.read_states.iter().any(|s| s.channel_id == dm.id && s.last_read == m.id));
    assert!(r.channels.iter().all(|c| c.id != dm.id), "DMs aren't listed as server channels");

    // Deleting the message removes the file.
    send(&mut alice, ClientMsg::DeleteMessage { message_id: m.id }).await;
    expect(&mut bob, |x| matches!(x, ServerMsg::MessageDeleted { .. })).await;
    assert_eq!(download(&alice_token, &att.id, false).await.unwrap().status(), 404);
}

fn gif_bytes() -> Vec<u8> {
    use image::codecs::gif::GifEncoder;
    use image::{Delay, Frame, Rgba, RgbaImage};
    let frames: Vec<Frame> = (0..3)
        .map(|i| {
            Frame::from_parts(
                RgbaImage::from_pixel(40, 30, Rgba([i * 80, 0, 0, 255])),
                0,
                0,
                Delay::from_numer_denom_ms(100, 1),
            )
        })
        .collect();
    let mut out = Vec::new();
    {
        let mut enc = GifEncoder::new(&mut out);
        enc.encode_frames(frames).unwrap();
    }
    out
}

/// A stand-in for GIPHY: search/trending answer in GIPHY's format, pointing at a GIF it serves.
async fn start_fake_giphy() -> String {
    use axum::routing::get;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let b = base.clone();
    let results = move |q: axum::extract::Query<std::collections::HashMap<String, String>>| {
        let b = b.clone();
        async move {
            assert_eq!(q.get("api_key").map(String::as_str), Some("test-key"), "the server sends its key");
            let title = q.get("q").cloned().unwrap_or_else(|| "trending".into());
            axum::Json(serde_json::json!({"data": [{
                "id": "gif1", "title": title,
                "images": {
                    "fixed_width_small": {"url": format!("{b}/small.gif"), "width": "40", "height": "30"},
                    "fixed_width_small_still": {"url": format!("{b}/still.gif")},
                    "downsized_medium": {"url": format!("{b}/full.gif")}
                }
            }]}))
        }
    };
    let app = axum::Router::new()
        .route("/search", get(results.clone()))
        .route("/trending", get(results))
        .route("/full.gif", get(|| async { gif_bytes() }));
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    base
}

#[tokio::test]
async fn gif_search_and_sending() {
    let gif_base = start_fake_giphy().await;
    let uploads = std::env::temp_dir().join(format!("syncord-test-{}", rand_suffix()));
    let config = Config {
        invite_code: "x".into(),
        uploads,
        giphy_api_key: "test-key".into(),
        gif_api_base: gif_base,
        ..Config::default()
    };
    let state = AppState::new(config, Db::open_in_memory().unwrap());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/ws", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router(state)).await.unwrap() });
    let _ = rustls::crypto::ring::default_provider().install_default();

    let (mut alice, r) = register(&url, "alice", "x").await;
    let ServerMsg::Ready(ready) = r else { panic!() };
    assert_eq!(ready.gif_provider.as_deref(), Some("GIPHY"));
    let general = ready.channels.iter().find(|c| c.kind == ChannelKind::Text).unwrap().id;

    // Trending (empty query) and search both come back in our own format.
    send(&mut alice, ClientMsg::SearchGifs { query: String::new() }).await;
    let ServerMsg::GifResults { results, .. } = expect(&mut alice, |m| matches!(m, ServerMsg::GifResults { .. })).await
    else {
        unreachable!()
    };
    assert_eq!(results[0].title, "trending");
    send(&mut alice, ClientMsg::SearchGifs { query: "cat".into() }).await;
    let ServerMsg::GifResults { query, results } =
        expect(&mut alice, |m| matches!(m, ServerMsg::GifResults { .. })).await
    else {
        unreachable!()
    };
    assert_eq!(query, "cat");
    assert_eq!((results[0].id.as_str(), results[0].width), ("gif1", 40));
    assert!(results[0].still_url.ends_with("/still.gif"));

    // Sending: the server fetches the GIF and posts it as an animated attachment.
    send(&mut alice, ClientMsg::SendGif { channel_id: general, gif_id: "gif1".into() }).await;
    let ServerMsg::MessageCreated(m) = expect(&mut alice, |m| matches!(m, ServerMsg::MessageCreated(_))).await else {
        unreachable!()
    };
    assert_eq!(m.attachments.len(), 1);
    assert!(m.attachments[0].animated);
    assert_eq!(m.attachments[0].filename, "cat.gif");

    // Only GIFs from recent results can be sent: no fetching arbitrary addresses.
    send(&mut alice, ClientMsg::SendGif { channel_id: general, gif_id: "http://evil/x.gif".into() }).await;
    expect(&mut alice, |m| matches!(m, ServerMsg::Error { message } if message.contains("expired"))).await;
}

#[tokio::test]
async fn upload_size_limit_and_streamed_download() {
    let uploads = std::env::temp_dir().join(format!("syncord-test-{}", rand_suffix()));
    let config = Config { invite_code: "x".into(), uploads, max_upload_mb: 1, ..Config::default() };
    let state = AppState::new(config, Db::open_in_memory().unwrap());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/ws", listener.local_addr().unwrap());
    let base = http_base(&url);
    tokio::spawn(async move { axum::serve(listener, router(state)).await.unwrap() });
    let _ = rustls::crypto::ring::default_provider().install_default();
    let http = reqwest::Client::new();

    let (_alice, r) = register(&url, "alice", "x").await;
    let ServerMsg::Ready(ready) = r else { panic!() };
    let general = ready.channels.iter().find(|c| c.kind == ChannelKind::Text).unwrap().id;
    let upload = |body: Vec<u8>| {
        http.post(format!("{base}{}?channel_id={general}&filename=data.bin", syncord_proto::UPLOAD_PATH))
            .header("Authorization", format!("Bearer {}", ready.token))
            .body(body)
            .send()
    };

    // Over the limit: refused while streaming, and nothing is left behind.
    assert_eq!(upload(vec![7u8; 1_100_000]).await.unwrap().status(), 413);

    // Under the limit: stored, and downloads come back whole with a length.
    let body: Vec<u8> = (0..900_000u32).map(|i| (i % 251) as u8).collect();
    let r = upload(body.clone()).await.unwrap();
    assert_eq!(r.status(), 200);
    let att: syncord_proto::Attachment = serde_json::from_slice(&r.bytes().await.unwrap()).unwrap();
    assert_eq!(att.size, 900_000);
    assert_eq!(att.mime, "application/octet-stream");
    let r = http
        .get(format!("{base}{}/{}", syncord_proto::FILES_PATH, att.id))
        .header("Authorization", format!("Bearer {}", ready.token))
        .send()
        .await
        .unwrap();
    assert_eq!(r.headers()["content-length"], "900000");
    assert_eq!(r.bytes().await.unwrap().to_vec(), body);
}
