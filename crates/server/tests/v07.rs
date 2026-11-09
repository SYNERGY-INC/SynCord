//! End-to-end tests for what 0.7 added: invite links, kicks and bans, private channels
//! and categories, threads, groups, polls, custom emoji, stickers, the soundboard,
//! end-to-end encryption relays, rich link previews and the admin page.

use std::sync::Arc;
use std::time::Duration;

use celerity_proto::e2ee::b64_encode;
use celerity_proto::{
    ChannelKind, ChannelPosition, ClientMsg, EVERYONE_ROLE, EncryptedPayload, PROTOCOL_VERSION, Ready, SearchQuery,
    ServerMsg, perms,
};
use celerity_server::{AppState, Config, db::Db, router, start_udp};
use futures::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message as Ws};

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

struct Server {
    /// WebSocket address for an up-to-date app.
    url: String,
    /// The same, for an app older than 0.7 (no protocol in the address).
    old_url: String,
    base: String,
    state: Arc<AppState>,
}

fn rand_suffix() -> u64 {
    use std::hash::{BuildHasher, RandomState};
    RandomState::new().hash_one(std::time::SystemTime::now())
}

async fn start_with(edit: impl FnOnce(&mut Config)) -> Server {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let uploads = std::env::temp_dir().join(format!("celerity-v07-{}", rand_suffix()));
    let mut config =
        Config { invite_code: "letmein".into(), community_name: "Test Crew".into(), uploads, ..Config::default() };
    edit(&mut config);
    let state = AppState::new(config, Db::open_in_memory().unwrap());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    start_udp(&state, "127.0.0.1:0".parse().unwrap()).await.unwrap();
    let app = router(state.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Server {
        url: format!("ws://{addr}/ws?protocol={PROTOCOL_VERSION}"),
        old_url: format!("ws://{addr}/ws"),
        base: format!("http://{addr}"),
        state,
    }
}

async fn start() -> Server {
    start_with(|_| {}).await
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

/// Read events until one matches, failing if any event matches `forbidden` first.
async fn expect_not_before(
    ws: &mut Socket,
    forbidden: impl Fn(&ServerMsg) -> bool,
    pred: impl Fn(&ServerMsg) -> bool,
) -> ServerMsg {
    let msg = expect(ws, |m| forbidden(m) || pred(m)).await;
    assert!(!forbidden(&msg), "received something it shouldn't have: {msg:?}");
    msg
}

fn is_error(m: &ServerMsg, has: &str) -> bool {
    matches!(m, ServerMsg::Error { message } if message.contains(has))
}

async fn register_at(url: &str, name: &str, invite: &str) -> (Socket, ServerMsg) {
    let (mut ws, _) = connect_async(url).await.unwrap();
    send(
        &mut ws,
        ClientMsg::Register { username: name.into(), password: "correct horse".into(), invite: invite.into() },
    )
    .await;
    let reply = expect(&mut ws, |m| matches!(m, ServerMsg::Ready(_) | ServerMsg::AuthFailed { .. })).await;
    (ws, reply)
}

/// Register and expect to get in.
async fn join(url: &str, name: &str) -> (Socket, Ready) {
    let (ws, reply) = register_at(url, name, "letmein").await;
    let ServerMsg::Ready(ready) = reply else { panic!("expected Ready, got {reply:?}") };
    (ws, ready)
}

async fn login(url: &str, name: &str) -> ServerMsg {
    let (mut ws, _) = connect_async(url).await.unwrap();
    send(&mut ws, ClientMsg::Login { username: name.into(), password: "correct horse".into() }).await;
    expect(&mut ws, |m| matches!(m, ServerMsg::Ready(_) | ServerMsg::AuthFailed { .. })).await
}

fn general(ready: &Ready) -> i64 {
    ready.channels.iter().find(|c| c.kind == ChannelKind::Text).unwrap().id
}

fn voice(ready: &Ready) -> i64 {
    ready.channels.iter().find(|c| c.kind == ChannelKind::Voice).unwrap().id
}

fn say(channel_id: i64, text: &str) -> ClientMsg {
    ClientMsg::SendMessage { channel_id, content: text.into(), attachments: vec![], reply_to: None }
}

fn png(w: u32, h: u32) -> Vec<u8> {
    let img = image::RgbaImage::from_pixel(w, h, image::Rgba([10, 200, 90, 255]));
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png).unwrap();
    out.into_inner()
}

/// A silent mono 16-bit WAV of `ms` milliseconds at 8 kHz.
fn wav(ms: u32) -> Vec<u8> {
    let data = vec![0u8; (16 * ms) as usize];
    let mut out = b"RIFF".to_vec();
    out.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&[1, 0, 1, 0]);
    out.extend_from_slice(&8_000u32.to_le_bytes());
    out.extend_from_slice(&16_000u32.to_le_bytes());
    out.extend_from_slice(&[2, 0, 16, 0]);
    out.extend_from_slice(b"data");
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(&data);
    out
}

#[tokio::test]
async fn invite_links_kicks_and_bans() {
    let s = start().await;
    let http = reqwest::Client::new();
    let (mut alice, ready) = join(&s.url, "alice").await;
    assert!(ready.invite_links, "the server says it can make invite links");
    let (mut bob, ready) = join(&s.url, "bob").await;
    let bob_id = ready.me;

    // Only people with Create invites can make them.
    send(&mut bob, ClientMsg::CreateInvite { max_age_secs: None, max_uses: None }).await;
    expect(&mut bob, |m| is_error(m, "Create invites")).await;
    send(&mut alice, ClientMsg::CreateInvite { max_age_secs: Some(3600), max_uses: Some(1) }).await;
    let ServerMsg::InviteCreated(invite) = expect(&mut alice, |m| matches!(m, ServerMsg::InviteCreated(_))).await
    else {
        unreachable!()
    };
    assert_eq!((invite.max_uses, invite.uses), (Some(1), 0));
    assert!(invite.expires_at.is_some());

    // The web page shows the server and an "Open in Celerity" link.
    let page = http.get(format!("{}/invite/{}", s.base, invite.token)).send().await.unwrap();
    assert_eq!(page.status(), 200);
    let html = page.text().await.unwrap();
    assert!(html.contains("Test Crew") && html.contains("Open in Celerity"), "{html}");
    assert!(html.contains(&format!("celerity://join/{}/{}", s.base.trim_start_matches("http://"), invite.token)));
    assert!(html.contains("github.com/bxpo/SynCord/releases/latest"));

    // The token works instead of the invite code, once.
    let (_carol, reply) = register_at(&s.url, "carol", &invite.token).await;
    assert!(matches!(reply, ServerMsg::Ready(_)), "{reply:?}");
    let (_, reply) = register_at(&s.url, "dave", &invite.token).await;
    assert!(matches!(reply, ServerMsg::AuthFailed { .. }), "used up");
    let (_, reply) = register_at(&s.url, "dave", "made-up").await;
    assert!(matches!(reply, ServerMsg::AuthFailed { .. }));
    // The server-wide invite code keeps working.
    let (_dave, reply) = register_at(&s.url, "dave", "letmein").await;
    assert!(matches!(reply, ServerMsg::Ready(_)));

    // Used-up invites drop off the list; a revoked one stops working and its page says so.
    send(&mut alice, ClientMsg::CreateInvite { max_age_secs: None, max_uses: None }).await;
    let ServerMsg::InviteCreated(second) = expect(&mut alice, |m| matches!(m, ServerMsg::InviteCreated(_))).await
    else {
        unreachable!()
    };
    send(&mut alice, ClientMsg::ListInvites).await;
    let ServerMsg::Invites { invites } = expect(&mut alice, |m| matches!(m, ServerMsg::Invites { .. })).await else {
        unreachable!()
    };
    assert_eq!(invites.iter().map(|i| &i.token).collect::<Vec<_>>(), vec![&second.token]);
    send(&mut alice, ClientMsg::RevokeInvite { token: second.token.clone() }).await;
    expect(&mut alice, |m| matches!(m, ServerMsg::InviteRevoked { token } if *token == second.token)).await;
    let gone = http.get(format!("{}/invite/{}", s.base, second.token)).send().await.unwrap();
    assert_eq!(gone.status(), 404);
    let (_, reply) = register_at(&s.url, "erin", &second.token).await;
    assert!(matches!(reply, ServerMsg::AuthFailed { .. }));

    // Kicking needs the permission and a higher rank.
    send(&mut bob, ClientMsg::KickMember { user_id: ready.users.iter().find(|u| u.is_owner).unwrap().id }).await;
    expect(&mut bob, |m| is_error(m, "Kick members")).await;
    send(&mut alice, ClientMsg::KickMember { user_id: bob_id }).await;
    expect(&mut alice, |m| matches!(m, ServerMsg::MemberRemoved { user_id, banned: false } if *user_id == bob_id))
        .await;
    expect(&mut alice, |m| matches!(m, ServerMsg::UserUpdated(u) if u.id == bob_id && u.removed && !u.banned)).await;
    // Bob is told, then disconnected.
    expect(&mut bob, |m| matches!(m, ServerMsg::MemberRemoved { .. })).await;
    let closed = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match bob.next().await {
                None | Some(Err(_)) | Some(Ok(Ws::Close(_))) => break,
                _ => {}
            }
        }
    })
    .await;
    assert!(closed.is_ok(), "kicked people are disconnected");
    let r = login(&s.url, "bob").await;
    assert!(matches!(&r, ServerMsg::AuthFailed { reason } if reason.contains("removed")), "{r:?}");

    // Coming back: register again with the same password and a valid invite.
    let (_, reply) = register_at(&s.url, "bob", "wrong").await;
    assert!(matches!(reply, ServerMsg::AuthFailed { .. }));
    let (_bob, reply) = register_at(&s.url, "bob", "letmein").await;
    let ServerMsg::Ready(back) = reply else { panic!("{reply:?}") };
    assert_eq!(back.me, bob_id, "the same account");

    // Banned people can't come back until unbanned.
    send(&mut alice, ClientMsg::BanMember { user_id: bob_id }).await;
    expect(&mut alice, |m| matches!(m, ServerMsg::MemberRemoved { banned: true, .. })).await;
    let r = login(&s.url, "bob").await;
    assert!(matches!(&r, ServerMsg::AuthFailed { reason } if reason.contains("banned")), "{r:?}");
    let (_, reply) = register_at(&s.url, "bob", "letmein").await;
    assert!(matches!(&reply, ServerMsg::AuthFailed { reason } if reason.contains("banned")));
    send(&mut alice, ClientMsg::UnbanMember { user_id: bob_id }).await;
    expect(&mut alice, |m| matches!(m, ServerMsg::UserUpdated(u) if u.id == bob_id && !u.banned)).await;
    let (_bob, reply) = register_at(&s.url, "bob", "letmein").await;
    assert!(matches!(reply, ServerMsg::Ready(_)), "back after the unban, with an invite");
}

#[tokio::test]
async fn private_channels_and_categories() {
    let s = start().await;
    let (mut alice, ready) = join(&s.url, "alice").await;
    let general = general(&ready);
    let (mut bob, ready) = join(&s.url, "bob").await;
    let bob_id = ready.me;

    send(&mut alice, ClientMsg::CreateRole { name: "Staff".into() }).await;
    let ServerMsg::RoleUpdated(staff) =
        expect(&mut alice, |m| matches!(m, ServerMsg::RoleUpdated(r) if r.name == "Staff")).await
    else {
        unreachable!()
    };
    send(&mut alice, ClientMsg::CreateCategory { name: "Back room".into() }).await;
    let ServerMsg::CategoryUpdated(cat) = expect(&mut bob, |m| matches!(m, ServerMsg::CategoryUpdated(_))).await else {
        unreachable!()
    };
    // Bob can't make categories.
    send(&mut bob, ClientMsg::CreateCategory { name: "mine".into() }).await;
    expect(&mut bob, |m| is_error(m, "Manage channels")).await;

    // Limiting the category hides it from Bob.
    send(
        &mut alice,
        ClientMsg::UpdateCategory { category_id: cat.id, name: None, allowed_roles: Some(vec![staff.id]) },
    )
    .await;
    expect(&mut bob, |m| matches!(m, ServerMsg::CategoryDeleted { category_id } if *category_id == cat.id)).await;
    send(
        &mut alice,
        ClientMsg::CreateChannel {
            name: "plans".into(),
            topic: String::new(),
            kind: ChannelKind::Text,
            category_id: Some(cat.id),
        },
    )
    .await;
    let ServerMsg::ChannelCreated(plans) =
        expect(&mut alice, |m| matches!(m, ServerMsg::ChannelCreated(c) if c.name == "plans")).await
    else {
        unreachable!()
    };
    assert_eq!(plans.category_id, Some(cat.id));
    send(&mut alice, say(plans.id, "secret @everyone")).await;
    let ServerMsg::MessageCreated(secret) = expect(&mut alice, |m| matches!(m, ServerMsg::MessageCreated(_))).await
    else {
        unreachable!()
    };
    assert!(secret.mentions.is_empty(), "@everyone in a private channel only pings who can see it: {secret:?}");
    send(&mut alice, say(general, "public")).await;
    // Bob never hears about the channel or its messages.
    expect_not_before(
        &mut bob,
        |m| {
            matches!(m, ServerMsg::ChannelCreated(c) if c.id == plans.id)
                || matches!(m, ServerMsg::MessageCreated(x) if x.channel_id == plans.id)
        },
        |m| matches!(m, ServerMsg::MessageCreated(x) if x.content == "public"),
    )
    .await;
    send(&mut bob, ClientMsg::FetchHistory { channel_id: plans.id, before: None }).await;
    expect(&mut bob, |m| is_error(m, "can't see")).await;
    send(&mut bob, say(plans.id, "let me in")).await;
    expect(&mut bob, |m| is_error(m, "no longer exists")).await;
    send(&mut bob, ClientMsg::FetchPins { channel_id: plans.id }).await;
    expect(&mut bob, |m| is_error(m, "can't see")).await;
    send(&mut bob, ClientMsg::Search(SearchQuery { words: vec!["secret".into()], ..Default::default() })).await;
    let ServerMsg::SearchResults { results, .. } =
        expect(&mut bob, |m| matches!(m, ServerMsg::SearchResults { .. })).await
    else {
        unreachable!()
    };
    assert!(results.is_empty(), "search skips private channels");
    let (_bob2, again) = {
        let (mut ws, _) = connect_async(&s.url).await.unwrap();
        send(&mut ws, ClientMsg::Login { username: "bob".into(), password: "correct horse".into() }).await;
        let ServerMsg::Ready(r) = expect(&mut ws, |m| matches!(m, ServerMsg::Ready(_))).await else { unreachable!() };
        (ws, r)
    };
    assert!(again.channels.iter().all(|c| c.id != plans.id));
    assert!(again.categories.iter().all(|c| c.id != cat.id));

    // Giving Bob the role shows him the category and channel straight away.
    send(&mut alice, ClientMsg::SetUserRoles { user_id: bob_id, roles: vec![staff.id] }).await;
    expect(&mut bob, |m| matches!(m, ServerMsg::CategoryUpdated(c) if c.id == cat.id)).await;
    expect(&mut bob, |m| matches!(m, ServerMsg::ChannelCreated(c) if c.id == plans.id)).await;
    send(&mut bob, ClientMsg::FetchHistory { channel_id: plans.id, before: None }).await;
    let ServerMsg::History { messages, .. } = expect(&mut bob, |m| matches!(m, ServerMsg::History { .. })).await else {
        unreachable!()
    };
    assert_eq!(messages.len(), 1);

    // A channel's own roles win over its category's; moving it out makes it public again.
    send(&mut alice, ClientMsg::SetUserRoles { user_id: bob_id, roles: vec![] }).await;
    expect(&mut bob, |m| matches!(m, ServerMsg::ChannelDeleted { channel_id } if *channel_id == plans.id)).await;
    send(
        &mut alice,
        ClientMsg::ReorderChannels {
            categories: vec![],
            channels: vec![
                ChannelPosition { id: plans.id, position: 0, category_id: None },
                ChannelPosition { id: general, position: 1, category_id: Some(cat.id) },
            ],
        },
    )
    .await;
    expect(&mut bob, |m| matches!(m, ServerMsg::ChannelCreated(c) if c.id == plans.id)).await;
    expect(&mut bob, |m| matches!(m, ServerMsg::ChannelDeleted { channel_id } if *channel_id == general)).await;
    // ...and back, all at once; a stale list is refused whole.
    send(
        &mut alice,
        ClientMsg::ReorderChannels {
            categories: vec![],
            channels: vec![
                ChannelPosition { id: general, position: 0, category_id: None },
                ChannelPosition { id: 99_999, position: 1, category_id: None },
            ],
        },
    )
    .await;
    expect(&mut alice, |m| is_error(m, "changed")).await;
    send(
        &mut alice,
        ClientMsg::UpdateChannel { channel_id: general, name: Some("Lobby".into()), topic: None, allowed_roles: None },
    )
    .await;
    expect(&mut alice, |m| matches!(m, ServerMsg::ChannelUpdated(c) if c.id == general && c.name == "lobby")).await;
    send(&mut alice, ClientMsg::DeleteCategory { category_id: cat.id }).await;
    expect(&mut alice, |m| matches!(m, ServerMsg::ChannelUpdated(c) if c.id == general && c.category_id.is_none()))
        .await;
    expect(&mut bob, |m| matches!(m, ServerMsg::ChannelCreated(c) if c.id == general)).await;

    // Private voice channels can't be joined by people who can't see them.
    send(
        &mut alice,
        ClientMsg::UpdateChannel {
            channel_id: voice(&ready),
            name: None,
            topic: None,
            allowed_roles: Some(vec![staff.id]),
        },
    )
    .await;
    expect(&mut bob, |m| matches!(m, ServerMsg::ChannelDeleted { .. })).await;
    send(&mut bob, ClientMsg::JoinVoice { channel_id: voice(&ready), encrypted: true }).await;
    expect(&mut bob, |m| is_error(m, "isn't a voice channel")).await;
}

#[tokio::test]
async fn threads() {
    let s = start().await;
    let (mut alice, ready) = join(&s.url, "alice").await;
    let general = general(&ready);
    let (mut bob, _) = join(&s.url, "bob").await;
    let (mut old, _) = register_at(&s.old_url, "olivia", "letmein").await;

    send(&mut alice, say(general, "Movie night?")).await;
    let ServerMsg::MessageCreated(start) = expect(&mut bob, |m| matches!(m, ServerMsg::MessageCreated(_))).await else {
        unreachable!()
    };
    send(&mut bob, ClientMsg::CreateThread { channel_id: general, message_id: Some(start.id), name: String::new() })
        .await;
    let ServerMsg::ThreadCreated(thread) = expect(&mut alice, |m| matches!(m, ServerMsg::ThreadCreated(_))).await
    else {
        unreachable!()
    };
    assert_eq!((thread.parent_id, thread.parent_message_id), (general, Some(start.id)));
    assert_eq!(thread.name, "Movie night?", "named after the message when no name is given");
    expect(&mut alice, |m| {
        matches!(m, ServerMsg::MessageUpdated(x) if x.id == start.id && x.thread.as_ref().is_some_and(|t| t.id == thread.id))
    })
    .await;
    // One thread per message.
    send(&mut alice, ClientMsg::CreateThread { channel_id: general, message_id: Some(start.id), name: "again".into() })
        .await;
    expect(&mut alice, |m| is_error(m, "already has a thread")).await;

    // Messages in a thread use the thread's id; the count goes up.
    send(&mut alice, say(thread.id, "I'm in")).await;
    expect(&mut bob, |m| matches!(m, ServerMsg::MessageCreated(x) if x.channel_id == thread.id)).await;
    expect(&mut bob, |m| matches!(m, ServerMsg::ThreadUpdated(t) if t.id == thread.id && t.message_count == 1)).await;
    send(&mut alice, say(general, "back in general")).await;
    // Apps older than 0.7 don't get thread messages (they'd show them as stray direct messages).
    expect_not_before(
        &mut old,
        |m| matches!(m, ServerMsg::MessageCreated(x) if x.channel_id == thread.id),
        |m| matches!(m, ServerMsg::MessageCreated(x) if x.content == "back in general"),
    )
    .await;

    send(&mut bob, ClientMsg::FetchHistory { channel_id: thread.id, before: None }).await;
    let ServerMsg::History { messages, .. } = expect(&mut bob, |m| matches!(m, ServerMsg::History { .. })).await else {
        unreachable!()
    };
    assert_eq!(messages[0].content, "I'm in");

    // Archiving: only the starter (or Manage messages); posting brings it back.

    send(&mut bob, ClientMsg::UpdateThread { thread_id: thread.id, name: None, archived: Some(true) }).await;
    expect(&mut alice, |m| matches!(m, ServerMsg::ThreadUpdated(t) if t.archived)).await;
    send(&mut alice, ClientMsg::ListThreads { channel_id: general }).await;
    let ServerMsg::Threads { threads, .. } = expect(&mut alice, |m| matches!(m, ServerMsg::Threads { .. })).await
    else {
        unreachable!()
    };
    assert!(threads.len() == 1 && threads[0].archived);
    let (_carol, ready) = join(&s.url, "carol").await;
    assert!(ready.threads.is_empty(), "archived threads aren't in Ready");
    send(&mut alice, say(thread.id, "unarchive")).await;
    expect(&mut bob, |m| matches!(m, ServerMsg::ThreadUpdated(t) if !t.archived && t.message_count == 2)).await;

    // Bob can't delete it; Alice (owner) can.
    send(&mut bob, ClientMsg::DeleteThread { thread_id: thread.id }).await;
    expect(&mut bob, |m| is_error(m, "Manage messages")).await;
    send(&mut alice, ClientMsg::DeleteThread { thread_id: thread.id }).await;
    expect(&mut bob, |m| matches!(m, ServerMsg::ThreadDeleted { thread_id, parent_id } if *thread_id == thread.id && *parent_id == general))
        .await;
    expect(&mut bob, |m| matches!(m, ServerMsg::MessageUpdated(x) if x.id == start.id && x.thread.is_none())).await;
}

#[tokio::test]
async fn group_direct_messages_and_calls() {
    use celerity_proto::MessageKind;
    let s = start().await;
    let (mut alice, ready) = join(&s.url, "alice").await;
    let alice_id = ready.me;
    let (mut bob, ready) = join(&s.url, "bob").await;
    let bob_id = ready.me;
    let (mut carol, ready) = join(&s.url, "carol").await;
    let carol_id = ready.me;
    let (mut dave, ready) = join(&s.url, "dave").await;
    let dave_id = ready.me;

    send(&mut alice, ClientMsg::CreateGroup { members: vec![bob_id], name: String::new() }).await;
    expect(&mut alice, |m| is_error(m, "3 to 10")).await;
    send(&mut alice, ClientMsg::CreateGroup { members: vec![bob_id, carol_id, alice_id], name: "Trip".into() }).await;
    let ServerMsg::GroupUpdated(group) = expect(&mut bob, |m| matches!(m, ServerMsg::GroupUpdated(_))).await else {
        unreachable!()
    };
    assert_eq!((group.owner_id, group.name.as_str(), group.members.len()), (alice_id, "Trip", 3));

    send(&mut bob, say(group.id, "hi all")).await;
    expect(&mut carol, |m| matches!(m, ServerMsg::MessageCreated(x) if x.channel_id == group.id)).await;
    let ServerMsg::MessageCreated(hi) =
        expect(&mut alice, |m| matches!(m, ServerMsg::MessageCreated(x) if x.channel_id == group.id)).await
    else {
        unreachable!()
    };
    send(&mut carol, ClientMsg::AddReaction { message_id: hi.id, emoji: "👋".into() }).await;
    expect(&mut bob, |m| matches!(m, ServerMsg::ReactionUpdate { added: true, .. })).await;
    send(&mut carol, ClientMsg::SetPinned { message_id: hi.id, pinned: true }).await;
    expect(&mut bob, |m| matches!(m, ServerMsg::MessageUpdated(x) if x.id == hi.id && x.pinned)).await;
    send(&mut dave, ClientMsg::FetchHistory { channel_id: group.id, before: None }).await;
    expect(&mut dave, |m| is_error(m, "can't see")).await;

    // Only the owner adds, removes and renames.
    send(&mut bob, ClientMsg::AddGroupMember { channel_id: group.id, user_id: dave_id }).await;
    expect(&mut bob, |m| is_error(m, "owner")).await;
    send(&mut alice, ClientMsg::AddGroupMember { channel_id: group.id, user_id: dave_id }).await;
    expect(&mut dave, |m| matches!(m, ServerMsg::GroupUpdated(g) if g.members.contains(&dave_id))).await;
    send(&mut alice, ClientMsg::RenameGroup { channel_id: group.id, name: "Road trip".into() }).await;
    expect(&mut dave, |m| matches!(m, ServerMsg::GroupUpdated(g) if g.name == "Road trip")).await;
    send(&mut alice, ClientMsg::RemoveGroupMember { channel_id: group.id, user_id: carol_id }).await;
    expect(&mut carol, |m| matches!(m, ServerMsg::GroupRemoved { channel_id } if *channel_id == group.id)).await;

    // A group call rings everyone; anyone in the group can join.
    send(&mut alice, ClientMsg::JoinVoice { channel_id: group.id, encrypted: true }).await;
    let ServerMsg::MessageCreated(note) =
        expect(&mut bob, |m| matches!(m, ServerMsg::MessageCreated(x) if matches!(x.kind, MessageKind::Call { .. })))
            .await
    else {
        unreachable!()
    };
    let mut pinged = note.mentions.clone();
    pinged.sort();
    assert_eq!(pinged, vec![bob_id, dave_id]);
    send(&mut dave, ClientMsg::JoinVoice { channel_id: group.id, encrypted: true }).await;
    expect(&mut bob, |m| {
        matches!(m, ServerMsg::MessageUpdated(x) if x.id == note.id
            && matches!(&x.kind, MessageKind::Call { participants, .. } if participants.len() == 2))
    })
    .await;
    send(&mut carol, ClientMsg::JoinVoice { channel_id: group.id, encrypted: true }).await;
    expect(&mut carol, |m| is_error(m, "isn't a voice channel")).await;

    // The owner leaves: the longest-standing member takes over.
    send(&mut alice, ClientMsg::LeaveGroup { channel_id: group.id }).await;
    expect(&mut alice, |m| matches!(m, ServerMsg::GroupRemoved { .. })).await;
    expect(&mut bob, |m| matches!(m, ServerMsg::GroupUpdated(g) if g.owner_id == bob_id && g.members.len() == 2)).await;
    expect(&mut bob, |m| matches!(m, ServerMsg::VoiceStateUpdate { user_id, state: None } if *user_id == alice_id))
        .await;
    let (_again, ready) = {
        let (mut ws, _) = connect_async(&s.url).await.unwrap();
        send(&mut ws, ClientMsg::Login { username: "dave".into(), password: "correct horse".into() }).await;
        let ServerMsg::Ready(r) = expect(&mut ws, |m| matches!(m, ServerMsg::Ready(_))).await else { unreachable!() };
        (ws, r)
    };
    assert!(ready.groups.iter().any(|g| g.id == group.id));
    assert!(ready.directs.is_empty(), "groups aren't listed as direct messages");
}

#[tokio::test]
async fn polls() {
    let s = start().await;
    celerity_server::start_polls(s.state.clone());
    let (mut alice, ready) = join(&s.url, "alice").await;
    let general = general(&ready);
    let (mut bob, ready) = join(&s.url, "bob").await;
    let bob_id = ready.me;

    send(
        &mut alice,
        ClientMsg::SendPoll {
            channel_id: general,
            question: "Pizza?".into(),
            options: vec!["Yes".into()],
            multiple: false,
            duration_secs: None,
        },
    )
    .await;
    expect(&mut alice, |m| is_error(m, "answers")).await;
    send(
        &mut alice,
        ClientMsg::SendPoll {
            channel_id: general,
            question: "Pizza?".into(),
            options: vec!["Yes".into(), "No".into(), "Maybe".into()],
            multiple: false,
            duration_secs: None,
        },
    )
    .await;
    let ServerMsg::MessageCreated(msg) = expect(&mut bob, |m| matches!(m, ServerMsg::MessageCreated(_))).await else {
        unreachable!()
    };
    let poll = msg.poll.clone().expect("a poll");
    assert_eq!(poll.options.len(), 3);
    assert!(msg.content.contains("Pizza?") && msg.content.contains("• Maybe"), "older apps read it: {}", msg.content);

    send(&mut bob, ClientMsg::Vote { message_id: msg.id, option: 0, voted: true }).await;
    expect(&mut alice, |m| matches!(m, ServerMsg::PollUpdated { poll, .. } if poll.options[0].votes == vec![bob_id]))
        .await;
    // One answer only: voting again moves the vote.
    send(&mut bob, ClientMsg::Vote { message_id: msg.id, option: 2, voted: true }).await;
    expect(&mut alice, |m| {
        matches!(m, ServerMsg::PollUpdated { poll, .. } if poll.options[0].votes.is_empty() && poll.options[2].votes == vec![bob_id])
    })
    .await;
    send(&mut bob, ClientMsg::Vote { message_id: msg.id, option: 7, voted: true }).await;
    expect(&mut bob, |m| is_error(m, "isn't in the poll")).await;
    send(&mut bob, ClientMsg::EndPoll { message_id: msg.id }).await;
    expect(&mut bob, |m| is_error(m, "Only whoever made")).await;
    send(&mut alice, ClientMsg::EditMessage { message_id: msg.id, content: "changed".into() }).await;
    expect(&mut alice, |m| is_error(m, "edit")).await;

    // A poll with an end time closes by itself, with its results kept.
    send(
        &mut alice,
        ClientMsg::SendPoll {
            channel_id: general,
            question: "Quick".into(),
            options: vec!["A".into(), "B".into()],
            multiple: true,
            duration_secs: Some(1),
        },
    )
    .await;
    let ServerMsg::MessageCreated(quick) =
        expect(&mut bob, |m| matches!(m, ServerMsg::MessageCreated(x) if x.content.contains("Quick"))).await
    else {
        unreachable!()
    };
    send(&mut bob, ClientMsg::Vote { message_id: quick.id, option: 0, voted: true }).await;
    send(&mut bob, ClientMsg::Vote { message_id: quick.id, option: 1, voted: true }).await;
    expect(
        &mut alice,
        |m| matches!(m, ServerMsg::PollUpdated { poll, .. } if poll.voters() == 1 && poll.options[1].votes.len() == 1),
    )
    .await;
    let ServerMsg::PollUpdated { poll, .. } = tokio::time::timeout(
        Duration::from_secs(15),
        expect(
            &mut alice,
            |m| matches!(m, ServerMsg::PollUpdated { message_id, poll, .. } if *message_id == quick.id && poll.closed),
        ),
    )
    .await
    .expect("the poll ends by itself") else {
        unreachable!()
    };
    assert!(poll.options.iter().all(|o| o.votes == vec![bob_id]), "results stay: {poll:?}");
    send(&mut bob, ClientMsg::Vote { message_id: quick.id, option: 0, voted: false }).await;
    expect(&mut bob, |m| is_error(m, "ended")).await;
}

#[tokio::test]
async fn emoji_stickers_and_soundboard() {
    let s = start().await;
    let http = reqwest::Client::new();
    let (mut alice, ready) = join(&s.url, "alice").await;
    let (alice_token, general, voice) = (ready.token.clone(), general(&ready), voice(&ready));
    let (mut bob, ready) = join(&s.url, "bob").await;
    let bob_token = ready.token.clone();
    let upload = |path: &str, token: &str, body: Vec<u8>| {
        http.post(format!("{}{path}", s.base)).bearer_auth(token).body(body).send()
    };

    // Uploading needs Manage emoji.
    let r = upload("/api/emoji?name=party", &bob_token, png(64, 64)).await.unwrap();
    assert_eq!(r.status(), 403);
    let r = upload("/api/emoji?name=bad name", &alice_token, png(64, 64)).await.unwrap();
    assert_eq!(r.status(), 400);
    let r = upload("/api/emoji?name=party", &alice_token, png(400, 200)).await.unwrap();
    assert_eq!(r.status(), 200);
    let emoji: celerity_proto::CustomEmoji = r.json().await.unwrap();
    expect(&mut bob, |m| matches!(m, ServerMsg::EmojiUpdated(e) if e.id == emoji.id)).await;
    // Too big pictures are shrunk to 128 px.
    let got = http.get(format!("{}/api/emoji/{}", s.base, emoji.id)).bearer_auth(&bob_token).send().await.unwrap();
    let img = image::load_from_memory(&got.bytes().await.unwrap()).unwrap();
    assert_eq!((img.width(), img.height()), (128, 64));
    let r = upload("/api/emoji?name=PARTY", &alice_token, png(8, 8)).await.unwrap();
    assert_eq!(r.status(), 409, "names are unique");
    let r = upload("/api/emoji?name=nope", &alice_token, b"not a picture".to_vec()).await.unwrap();
    assert_eq!(r.status(), 400);

    // Messages and reactions use the emoji's code.
    send(&mut bob, say(general, &format!("yay {}", emoji.code()))).await;
    let ServerMsg::MessageCreated(m) = expect(&mut alice, |m| matches!(m, ServerMsg::MessageCreated(_))).await else {
        unreachable!()
    };
    send(&mut alice, ClientMsg::AddReaction { message_id: m.id, emoji: emoji.code() }).await;
    expect(&mut bob, |x| matches!(x, ServerMsg::ReactionUpdate { emoji: e, .. } if *e == emoji.code())).await;
    send(&mut bob, ClientMsg::RenameEmoji { emoji_id: emoji.id, name: "x2".into() }).await;
    expect(&mut bob, |m| is_error(m, "Manage emoji")).await;
    send(&mut alice, ClientMsg::RenameEmoji { emoji_id: emoji.id, name: "partytime".into() }).await;
    expect(&mut bob, |m| matches!(m, ServerMsg::EmojiUpdated(e) if e.name == "partytime")).await;

    // Stickers: uploaded the same way, sent as a message.
    let r = upload("/api/stickers?name=wave", &alice_token, png(300, 300)).await.unwrap();
    let sticker: celerity_proto::Sticker = r.json().await.unwrap();
    send(&mut bob, ClientMsg::SendSticker { channel_id: general, sticker_id: sticker.id, reply_to: None }).await;
    let ServerMsg::MessageCreated(m) =
        expect(&mut alice, |m| matches!(m, ServerMsg::MessageCreated(x) if x.sticker.is_some())).await
    else {
        unreachable!()
    };
    assert_eq!(m.sticker.unwrap().name, "wave");
    let (_carol, ready) = join(&s.url, "carol").await;
    assert_eq!(ready.emojis.len(), 1);
    assert_eq!(ready.stickers.len(), 1);
    send(&mut alice, ClientMsg::DeleteEmoji { emoji_id: emoji.id }).await;
    expect(&mut bob, |m| matches!(m, ServerMsg::EmojiDeleted { emoji_id } if *emoji_id == emoji.id)).await;
    let gone = http.get(format!("{}/api/emoji/{}", s.base, emoji.id)).bearer_auth(&bob_token).send().await.unwrap();
    assert_eq!(gone.status(), 404);

    // Sounds: at most 5 seconds, Manage soundboard to upload.
    let r = upload("/api/sounds?name=Air%20horn&emoji=%F0%9F%93%A3", &bob_token, wav(1000)).await.unwrap();
    assert_eq!(r.status(), 403);
    let r = upload("/api/sounds?name=Too%20long", &alice_token, wav(6000)).await.unwrap();
    assert_eq!(r.status(), 400);
    let r = upload("/api/sounds?name=Noise", &alice_token, b"definitely not audio".to_vec()).await.unwrap();
    assert_eq!(r.status(), 400);
    let r = upload("/api/sounds?name=Air%20horn&emoji=%F0%9F%93%A3", &alice_token, wav(1000)).await.unwrap();
    assert_eq!(r.status(), 200);
    let sound: celerity_proto::Sound = r.json().await.unwrap();
    assert_eq!((sound.name.as_str(), sound.emoji.as_deref(), sound.duration_ms), ("Air horn", Some("📣"), 1000));
    expect(&mut bob, |m| matches!(m, ServerMsg::SoundUpdated(x) if x.id == sound.id)).await;
    let file = http.get(format!("{}/api/sounds/{}", s.base, sound.id)).bearer_auth(&bob_token).send().await.unwrap();
    assert_eq!(file.bytes().await.unwrap().to_vec(), wav(1000), "kept as uploaded");

    // Playing: only in voice, for the people in that channel, rate limited.
    send(&mut bob, ClientMsg::PlaySound { sound_id: sound.id }).await;
    expect(&mut bob, |m| is_error(m, "Join a voice channel")).await;
    send(&mut alice, ClientMsg::JoinVoice { channel_id: voice, encrypted: true }).await;
    send(&mut bob, ClientMsg::JoinVoice { channel_id: voice, encrypted: true }).await;
    expect(&mut bob, |m| matches!(m, ServerMsg::VoiceReady { .. })).await;
    for _ in 0..3 {
        send(&mut bob, ClientMsg::PlaySound { sound_id: sound.id }).await;
        expect(&mut alice, |m| matches!(m, ServerMsg::SoundPlayed { sound_id, .. } if *sound_id == sound.id)).await;
    }
    send(&mut bob, ClientMsg::PlaySound { sound_id: sound.id }).await;
    expect(&mut bob, |m| is_error(m, "Wait a moment")).await;
    send(&mut alice, ClientMsg::DeleteSound { sound_id: sound.id }).await;
    expect(&mut bob, |m| matches!(m, ServerMsg::SoundDeleted { sound_id } if *sound_id == sound.id)).await;
}

#[tokio::test]
async fn encryption_is_stored_and_relayed() {
    let s = start().await;
    let (mut alice, ready) = join(&s.url, "alice").await;
    let general = general(&ready);
    let (mut bob, ready) = join(&s.url, "bob").await;
    let bob_id = ready.me;

    send(
        &mut alice,
        ClientMsg::PublishIdentityKeys {
            agreement_key: "short".into(),
            signing_key: "x".into(),
            signature: String::new(),
        },
    )
    .await;
    expect(&mut alice, |m| is_error(m, "aren't valid")).await;
    let (ak, sk) = (b64_encode(&[1; 32]), b64_encode(&[2; 32]));
    send(
        &mut alice,
        ClientMsg::PublishIdentityKeys {
            agreement_key: ak.clone(),
            signing_key: sk.clone(),
            signature: b64_encode(&[3; 64]),
        },
    )
    .await;
    let ServerMsg::IdentityKeysUpdated(keys) =
        expect(&mut bob, |m| matches!(m, ServerMsg::IdentityKeysUpdated(_))).await
    else {
        unreachable!()
    };
    assert_eq!((keys.agreement_key.as_str(), keys.signing_key.as_str()), (ak.as_str(), sk.as_str()));
    send(&mut bob, ClientMsg::FetchIdentityKeys { user_ids: vec![keys.user_id, bob_id] }).await;
    let ServerMsg::IdentityKeys { keys: fetched } =
        expect(&mut bob, |m| matches!(m, ServerMsg::IdentityKeys { .. })).await
    else {
        unreachable!()
    };
    assert_eq!(fetched.len(), 1, "Bob hasn't published any");

    let payload = EncryptedPayload {
        ciphertext: b64_encode(b"opaque bytes the server can't read"),
        nonce: b64_encode(&[9; 24]),
        sender_key_id: "k1".into(),
        recipients: vec![],
    };
    send(
        &mut alice,
        ClientMsg::SendEncrypted { channel_id: general, payload: payload.clone(), attachments: vec![], reply_to: None },
    )
    .await;
    expect(&mut alice, |m| is_error(m, "only for direct messages")).await;
    send(&mut alice, ClientMsg::OpenDirect { user_id: bob_id }).await;
    let ServerMsg::DirectOpened(dm) = expect(&mut alice, |m| matches!(m, ServerMsg::DirectOpened(_))).await else {
        unreachable!()
    };
    send(
        &mut alice,
        ClientMsg::SendEncrypted { channel_id: dm.id, payload: payload.clone(), attachments: vec![], reply_to: None },
    )
    .await;
    let ServerMsg::MessageCreated(m) = expect(&mut bob, |m| matches!(m, ServerMsg::MessageCreated(_))).await else {
        unreachable!()
    };
    assert_eq!((m.content.as_str(), m.encrypted.as_ref()), ("", Some(&payload)));
    send(&mut bob, ClientMsg::AddReaction { message_id: m.id, emoji: "🔒".into() }).await;
    expect(&mut alice, |x| matches!(x, ServerMsg::ReactionUpdate { message_id, .. } if *message_id == m.id)).await;
    let edited = EncryptedPayload { ciphertext: b64_encode(b"new"), ..payload.clone() };
    send(&mut alice, ClientMsg::EditEncrypted { message_id: m.id, payload: edited.clone() }).await;
    expect(
        &mut bob,
        |x| matches!(x, ServerMsg::MessageUpdated(u) if u.encrypted.as_ref() == Some(&edited) && u.edited_at.is_some()),
    )
    .await;
    send(&mut bob, ClientMsg::EditEncrypted { message_id: m.id, payload: edited }).await;
    expect(&mut bob, |m| is_error(m, "your own")).await;
    send(&mut bob, ClientMsg::Search(SearchQuery { from: Some(m.author_id), ..Default::default() })).await;
    let ServerMsg::SearchResults { results, .. } =
        expect(&mut bob, |m| matches!(m, ServerMsg::SearchResults { .. })).await
    else {
        unreachable!()
    };
    assert!(results.iter().all(|r| r.id != m.id), "encrypted messages aren't searched");

    // Call key exchange: relayed only to the others in the call.
    send(&mut alice, ClientMsg::CallSignal { to: None, data: "hello".into() }).await;
    expect(&mut alice, |m| is_error(m, "not in a call")).await;
    send(&mut alice, ClientMsg::JoinVoice { channel_id: dm.id, encrypted: true }).await;
    send(&mut bob, ClientMsg::JoinVoice { channel_id: dm.id, encrypted: true }).await;
    expect(&mut alice, |m| matches!(m, ServerMsg::VoiceStateUpdate { user_id, state: Some(_) } if *user_id == bob_id))
        .await;
    send(&mut alice, ClientMsg::CallSignal { to: Some(bob_id), data: "key-offer".into() }).await;
    expect(
        &mut bob,
        |m| matches!(m, ServerMsg::CallSignal { channel_id, data, .. } if *channel_id == dm.id && data == "key-offer"),
    )
    .await;
    send(&mut alice, ClientMsg::LeaveVoice).await;
    send(&mut alice, ClientMsg::JoinVoice { channel_id: voice(&ready), encrypted: true }).await;
    expect(&mut alice, |m| matches!(m, ServerMsg::VoiceReady { .. })).await;
    send(&mut alice, ClientMsg::CallSignal { to: None, data: "x".into() }).await;
    expect(&mut alice, |m| is_error(m, "only for direct message and group calls")).await;
}

/// A stand-in for YouTube, fxtwitter and Spotify.
async fn start_fake_providers() -> String {
    use axum::{Router, http::header, routing::get};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let thumb = png(64, 36);
    let b = base.clone();
    let json = |v: serde_json::Value| ([(header::CONTENT_TYPE, "application/json")], v.to_string());
    let app = Router::new()
        .route(
            "/youtube/oembed",
            get(move || {
                let b = b.clone();
                async move {
                    json(serde_json::json!({"title": "Never Gonna", "author_name": "Rick", "author_url": "https://www.youtube.com/@rick",
                        "thumbnail_url": format!("{b}/thumb.png")}))
                }
            }),
        )
        .route(
            "/youtube/watch",
            get(|| async {
                ([(header::CONTENT_TYPE, "text/html")], r#"<html><head><meta property="og:description" content="The video">
                <meta itemprop="duration" content="PT3M33S"></head></html>"#)
            }),
        )
        .route(
            "/fxtwitter/rustlang/status/{id}",
            get({
                let b = base.clone();
                move || {
                    let b = b.clone();
                    async move {
                        json(serde_json::json!({"code": 200, "tweet": {"text": "Rust 2.0 is out", "author": {"name": "Rust", "screen_name": "rustlang"},
                            "media": {"photos": [{"url": format!("{b}/thumb.png")}]}}}))
                    }
                }
            }),
        )
        .route(
            "/spotify/oembed",
            get({
                let b = base.clone();
                move || {
                    let b = b.clone();
                    async move { json(serde_json::json!({"title": "Song Title", "thumbnail_url": format!("{b}/thumb.png")})) }
                }
            }),
        )
        .route(
            "/spotify/track/{id}",
            get(|| async {
                ([(header::CONTENT_TYPE, "text/html")], r#"<html><head>
                <meta property="og:description" content="Listen to Song Title on Spotify. The Band · Song · 2024.">
                <meta name="music:duration" content="201"></head></html>"#)
            }),
        )
        .route("/thumb.png", get(move || async move { ([(header::CONTENT_TYPE, "image/png")], thumb) }));
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    base
}

#[tokio::test]
async fn rich_link_previews() {
    let providers = start_fake_providers().await;
    let s = start_with(|c| {
        c.unsafe_allow_private_link_previews = true;
        c.preview_api_base = providers.clone();
    })
    .await;
    let (mut alice, ready) = join(&s.url, "alice").await;
    let general = general(&ready);

    send(&mut alice, say(general, "https://youtu.be/dQw4w9WgXcQ")).await;
    let ServerMsg::MessageUpdated(m) =
        expect(&mut alice, |m| matches!(m, ServerMsg::MessageUpdated(x) if !x.embeds.is_empty())).await
    else {
        unreachable!()
    };
    let e = &m.embeds[0];
    assert_eq!(e.provider.as_deref(), Some("youtube"));
    assert_eq!((e.title.as_deref(), e.author.as_deref()), (Some("Never Gonna"), Some("Rick")));
    assert_eq!((e.duration_secs, e.embed_id.as_deref()), (Some(213), Some("dQw4w9WgXcQ")));
    assert!(e.image.is_some(), "the thumbnail");
    assert_eq!(e.url, "https://youtu.be/dQw4w9WgXcQ");

    send(&mut alice, say(general, "https://x.com/rustlang/status/123456")).await;
    let ServerMsg::MessageUpdated(m) = expect(&mut alice, |m| {
        matches!(m, ServerMsg::MessageUpdated(x) if x.embeds.first().is_some_and(|e| e.provider.as_deref() == Some("twitter")))
    })
    .await
    else {
        unreachable!()
    };
    let e = &m.embeds[0];
    assert_eq!(e.author.as_deref(), Some("Rust (@rustlang)"));
    assert_eq!(e.description.as_deref(), Some("Rust 2.0 is out"));
    assert!(e.image.is_some());

    send(&mut alice, say(general, "https://open.spotify.com/track/4uLU6hMCjMI75M1A2tKUQC")).await;
    let ServerMsg::MessageUpdated(m) = expect(&mut alice, |m| {
        matches!(m, ServerMsg::MessageUpdated(x) if x.embeds.first().is_some_and(|e| e.provider.as_deref() == Some("spotify")))
    })
    .await
    else {
        unreachable!()
    };
    let e = &m.embeds[0];
    assert_eq!((e.title.as_deref(), e.author.as_deref()), (Some("Song Title"), Some("The Band")));
    assert_eq!((e.duration_secs, e.embed_id.as_deref()), (Some(201), Some("track/4uLU6hMCjMI75M1A2tKUQC")));
}

/// The value of a `Set-Cookie` header with this name.
fn set_cookie(resp: &reqwest::Response, name: &str) -> Option<String> {
    resp.headers()
        .get_all(reqwest::header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find_map(|v| v.strip_prefix(&format!("{name}="))?.split(';').next().map(String::from))
}

fn hidden(html: &str, name: &str) -> String {
    let marker = format!("name=\"{name}\" value=\"");
    let at = html.find(&marker).unwrap_or_else(|| panic!("no {name} in {html}")) + marker.len();
    html[at..].split('"').next().unwrap().to_string()
}

#[tokio::test]
async fn admin_page() {
    let s = start().await;
    let http = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().unwrap();
    let (_alice, _) = join(&s.url, "alice").await;
    let (mut bob, ready) = join(&s.url, "bob").await;
    let bob_id = ready.me;

    // Signing in needs the form's own token, the right password and an admin account.
    let page = http.get(format!("{}/admin", s.base)).send().await.unwrap();
    assert_eq!(page.headers().get("x-frame-options").unwrap(), "DENY");
    let login_cookie = set_cookie(&page, "celerity_admin_login").expect("login form token");
    let raw = page.headers().get(reqwest::header::SET_COOKIE).unwrap().to_str().unwrap().to_string();
    assert!(raw.contains("HttpOnly") && raw.contains("SameSite=Strict"), "{raw}");
    let html = page.text().await.unwrap();
    let csrf = hidden(&html, "csrf");
    assert_eq!(csrf, login_cookie);
    let sign_in = |user: &str, pass: &str, csrf: &str| {
        http.post(format!("{}/admin/login", s.base))
            .header("cookie", format!("celerity_admin_login={login_cookie}"))
            .header("content-type", "application/x-www-form-urlencoded")
            .body(form(&[("username", user), ("password", pass), ("csrf", csrf)]))
            .send()
    };
    let r = sign_in("alice", "correct horse", "forged").await.unwrap();
    assert!(r.text().await.unwrap().contains("expired"));
    let r = sign_in("bob", "correct horse", &csrf).await.unwrap();
    assert!(r.text().await.unwrap().contains("Only the owner and administrators"));
    let r = sign_in("alice", "correct horse", &csrf).await.unwrap();
    assert_eq!(r.status(), 303);
    let session = set_cookie(&r, "celerity_admin").expect("session cookie");
    let cookie = format!("celerity_admin={session}");

    let dash =
        http.get(format!("{}/admin", s.base)).header("cookie", &cookie).send().await.unwrap().text().await.unwrap();
    for part in [
        "Overview",
        "online now",
        "database",
        "uploads",
        "free disk space",
        "Invite links",
        "Members",
        "Backups",
        "@bob",
    ] {
        assert!(dash.contains(part), "the dashboard shows {part}");
    }
    let csrf = hidden(&dash, "csrf");

    // Forms need the session's CSRF token.
    let r = http
        .post(format!("{}/admin/invites", s.base))
        .header("cookie", &cookie)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(form(&[("csrf", "nope"), ("max_age_hours", "24"), ("max_uses", "0")]))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
    let r = http
        .post(format!("{}/admin/invites", s.base))
        .header("cookie", &cookie)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(form(&[("csrf", csrf.as_str()), ("max_age_hours", "24"), ("max_uses", "0")]))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 303);
    let dash =
        http.get(format!("{}/admin", s.base)).header("cookie", &cookie).send().await.unwrap().text().await.unwrap();
    assert!(dash.contains("Made a new invite link") && dash.contains("celerity://join/"), "{dash}");

    // Kicking from the page uses the same rules as the app.
    let r = http
        .post(format!("{}/admin/kick", s.base))
        .header("cookie", &cookie)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(form(&[("csrf", csrf.as_str()), ("user_id", &bob_id.to_string())]))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 303);
    expect(&mut bob, |m| matches!(m, ServerMsg::MemberRemoved { .. })).await;
    let dash =
        http.get(format!("{}/admin", s.base)).header("cookie", &cookie).send().await.unwrap().text().await.unwrap();
    assert!(dash.contains("Kicked bob"));

    // Failed sign-ins are limited.
    let page = http.get(format!("{}/admin", s.base)).send().await.unwrap();
    let fresh = set_cookie(&page, "celerity_admin_login").unwrap();
    let mut last = String::new();
    for _ in 0..6 {
        last = http
            .post(format!("{}/admin/login", s.base))
            .header("cookie", format!("celerity_admin_login={fresh}"))
            .header("content-type", "application/x-www-form-urlencoded")
            .body(form(&[("username", "alice"), ("password", "wrong password"), ("csrf", fresh.as_str())]))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
    }
    assert!(last.contains("Too many failed sign-ins"), "{last}");

    // Signing out ends the session.
    let r = http
        .post(format!("{}/admin/logout", s.base))
        .header("cookie", &cookie)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(form(&[("csrf", csrf.as_str())]))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 303);
    let after =
        http.get(format!("{}/admin", s.base)).header("cookie", &cookie).send().await.unwrap().text().await.unwrap();
    assert!(after.contains("Sign in"));
}

#[tokio::test]
async fn apps_older_than_0_7_keep_working() {
    let s = start().await;
    // An old app's admin edits the everyone role: permissions it doesn't know are kept.
    let (mut old, ready) = register_at(&s.old_url, "alice", "letmein").await;
    let ServerMsg::Ready(ready) = ready else { panic!() };
    let everyone = ready.roles.iter().find(|r| r.id == EVERYONE_ROLE).unwrap().clone();
    assert!(everyone.permissions & perms::USE_SOUNDBOARD != 0, "on by default");
    let old_view =
        celerity_proto::Role { permissions: everyone.permissions & 0xFFF & !perms::ATTACH_FILES, ..everyone.clone() };
    send(&mut old, ClientMsg::UpdateRole(old_view)).await;
    let ServerMsg::RoleUpdated(r) =
        expect(&mut old, |m| matches!(m, ServerMsg::RoleUpdated(r) if r.id == EVERYONE_ROLE)).await
    else {
        unreachable!()
    };
    assert_eq!(r.permissions & perms::ATTACH_FILES, 0, "what it changed, changed");
    assert!(r.permissions & perms::USE_SOUNDBOARD != 0, "what it didn't know about stayed");

    // The JSON of an ordinary message and channel has nothing new in it.
    let (mut new, _) = join(&s.url, "bob").await;
    send(&mut new, say(general(&ready), "plain")).await;
    let raw = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(Ok(Ws::Text(t))) = old.next().await
                && t.contains("MessageCreated")
            {
                return t.to_string();
            }
        }
    })
    .await
    .unwrap();
    for field in ["thread", "poll", "sticker", "encrypted", "category_id", "allowed_roles"] {
        assert!(!raw.contains(field), "{field} in {raw}");
    }
}

/// A URL-encoded form body.
fn form(pairs: &[(&str, &str)]) -> String {
    let enc = |s: &str| -> String {
        s.bytes()
            .map(|b| match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
                b' ' => "+".into(),
                _ => format!("%{b:02X}"),
            })
            .collect()
    };
    pairs.iter().map(|(k, v)| format!("{}={}", enc(k), enc(v))).collect::<Vec<_>>().join("&")
}
