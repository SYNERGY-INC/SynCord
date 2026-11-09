//! The admin web page at `/admin`, and the invite page at `/invite/<token>`.
//!
//! Plain server-rendered HTML, so the owner can look after the server from any browser
//! (also over the server's own encrypted connection: `https://host:7878/admin`, after
//! accepting its self-made certificate). Signing in takes the username and password of
//! the owner or an administrator, checked exactly like the app does. The session lives in
//! an `HttpOnly`, `SameSite=Strict` cookie; every form carries a CSRF token; failed sign-ins
//! are rate-limited per address.

use std::collections::{HashMap, VecDeque};
use std::fmt::Write as _;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Router;
use axum::extract::{ConnectInfo, Extension};
use axum::extract::{Form, Path as UrlPath, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use celerity_proto::invites::{DOWNLOAD_URL, invite_link};
use celerity_proto::{UserId, perms};
use rand::Rng;

use crate::tls::Peer;
use crate::{AppState, backup, db};

const SESSION_COOKIE: &str = "celerity_admin";
const LOGIN_COOKIE: &str = "celerity_admin_login";
/// Signed-in admin sessions last this long.
const SESSION_TTL: Duration = Duration::from_secs(12 * 3600);
/// Failed sign-ins allowed per address (and per username) in [`FAILURE_WINDOW`].
const MAX_FAILURES: usize = 5;
const FAILURE_WINDOW: Duration = Duration::from_secs(15 * 60);

struct AdminSession {
    user: UserId,
    csrf: String,
    expires: Instant,
    /// A message to show once, after a form was sent.
    flash: Option<String>,
}

/// Signed-in admin sessions and recent failed sign-ins, kept in memory.
#[derive(Default)]
pub struct Sessions {
    sessions: Mutex<HashMap<String, AdminSession>>,
    failures: Mutex<HashMap<String, VecDeque<Instant>>>,
}

fn random_hex(bytes: usize) -> String {
    let mut rng = rand::rng();
    (0..bytes).map(|_| format!("{:02x}", rng.random::<u8>())).collect()
}

/// Compare secrets without stopping at the first difference.
fn same(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

impl Sessions {
    fn create(&self, user: UserId) -> String {
        let token = random_hex(32);
        let mut sessions = self.sessions.lock().unwrap();
        let now = Instant::now();
        sessions.retain(|_, s| s.expires > now);
        sessions.insert(
            token.clone(),
            AdminSession { user, csrf: random_hex(16), expires: now + SESSION_TTL, flash: None },
        );
        token
    }

    /// The signed-in user and their CSRF token, taking any message waiting for them.
    fn get(&self, token: &str) -> Option<(UserId, String, Option<String>)> {
        let mut sessions = self.sessions.lock().unwrap();
        let s = sessions.get_mut(token).filter(|s| s.expires > Instant::now())?;
        Some((s.user, s.csrf.clone(), s.flash.take()))
    }

    fn flash(&self, token: &str, message: impl Into<String>) {
        if let Some(s) = self.sessions.lock().unwrap().get_mut(token) {
            s.flash = Some(message.into());
        }
    }

    fn remove(&self, token: &str) {
        self.sessions.lock().unwrap().remove(token);
    }

    /// Sign someone out of the admin page (they were kicked or banned).
    pub fn forget_user(&self, user: UserId) {
        self.sessions.lock().unwrap().retain(|_, s| s.user != user);
    }

    fn too_many_failures(&self, keys: &[String]) -> bool {
        let mut failures = self.failures.lock().unwrap();
        let now = Instant::now();
        keys.iter().any(|k| {
            let list = failures.entry(k.clone()).or_default();
            while list.front().is_some_and(|t| now.duration_since(*t) > FAILURE_WINDOW) {
                list.pop_front();
            }
            list.len() >= MAX_FAILURES
        })
    }

    fn failed(&self, keys: &[String]) {
        let mut failures = self.failures.lock().unwrap();
        for k in keys {
            failures.entry(k.clone()).or_default().push_back(Instant::now());
        }
    }
}

/// Escape text for HTML.
pub fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

fn cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|kv| kv.trim().split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v.to_string())
}

fn set_cookie(name: &str, value: &str, max_age: u64, secure: bool) -> String {
    let secure = if secure { "; Secure" } else { "" };
    format!("{name}={value}; HttpOnly; SameSite=Strict; Path=/admin; Max-Age={max_age}{secure}")
}

const STYLE: &str = r#"
:root { color-scheme: dark; }
* { box-sizing: border-box; }
body { margin: 0; background: #1e1f22; color: #dbdee1; font: 15px/1.45 system-ui, -apple-system, "Segoe UI", Roboto, sans-serif; }
main { max-width: 980px; margin: 0 auto; padding: 24px 16px 48px; }
header { display: flex; align-items: center; justify-content: space-between; gap: 12px; margin-bottom: 20px; }
h1 { font-size: 22px; margin: 0; color: #f2f3f5; }
h2 { font-size: 13px; text-transform: uppercase; letter-spacing: .04em; color: #b5bac1; margin: 0 0 12px; }
section { background: #2b2d31; border-radius: 12px; padding: 18px 20px; margin-bottom: 16px; box-shadow: 0 2px 10px rgba(0,0,0,.25); }
table { width: 100%; border-collapse: collapse; }
td, th { text-align: left; padding: 8px 6px; border-top: 1px solid #3f4147; vertical-align: middle; }
th { color: #949ba4; font-weight: 600; font-size: 12px; border-top: none; }
.muted { color: #949ba4; }
.pill { display: inline-block; padding: 1px 8px; border-radius: 999px; background: #3f4147; font-size: 12px; margin-right: 4px; }
.online { color: #23a55a; }
.grid { display: grid; grid-template-columns: repeat(auto-fit, minmax(180px, 1fr)); gap: 12px; }
.stat { background: #232428; border-radius: 10px; padding: 12px 14px; }
.stat b { display: block; font-size: 20px; color: #f2f3f5; }
form { display: inline; margin: 0; }
input, select { background: #1e1f22; color: #dbdee1; border: 1px solid #3f4147; border-radius: 8px; padding: 8px 10px; font: inherit; }
button { background: #5865f2; color: white; border: 0; border-radius: 8px; padding: 8px 14px; font: inherit; font-weight: 600; cursor: pointer; }
button:hover { background: #4752c4; }
button.danger { background: #da373c; }
button.danger:hover { background: #a12828; }
button.quiet { background: #4e5058; }
.notice { background: #2f3a5c; border-left: 4px solid #5865f2; padding: 10px 14px; border-radius: 8px; margin-bottom: 16px; }
.login { max-width: 380px; margin: 10vh auto 0; }
.login label { display: block; margin: 12px 0 4px; color: #b5bac1; font-size: 13px; }
.login input { width: 100%; }
.login button { width: 100%; margin-top: 18px; }
code { background: #1e1f22; padding: 2px 6px; border-radius: 6px; word-break: break-all; }
.row { display: flex; flex-wrap: wrap; gap: 8px; align-items: center; }
"#;

fn page(title: &str, body: &str) -> Response {
    let html = format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <meta name=\"referrer\" content=\"no-referrer\"><title>{}</title><style>{STYLE}</style></head>\
         <body><main>{body}</main></body></html>",
        esc(title)
    );
    let mut resp = axum::response::Html(html).into_response();
    let h = resp.headers_mut();
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert("x-frame-options", HeaderValue::from_static("DENY"));
    h.insert("x-content-type-options", HeaderValue::from_static("nosniff"));
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'none'; style-src 'unsafe-inline'; script-src 'unsafe-inline'; img-src 'self'; \
             form-action 'self'; frame-ancestors 'none'; base-uri 'none'",
        ),
    );
    resp
}

/// The `/admin` routes.
pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/admin", get(dashboard))
        .route("/admin/login", post(login))
        .route("/admin/logout", post(logout))
        .route("/admin/invites", post(create_invite))
        .route("/admin/invites/revoke", post(revoke_invite))
        .route("/admin/kick", post(kick))
        .route("/admin/ban", post(ban))
        .route("/admin/unban", post(unban))
        .route("/admin/backup", post(backup_now))
}

type Peer0 = Option<Extension<ConnectInfo<Peer>>>;

fn is_tls(peer: &Peer0) -> bool {
    peer.as_ref().is_some_and(|p| p.0.0.tls)
}

/// The signed-in admin, if the cookie is for a live session and they're still an admin.
async fn signed_in(state: &AppState, headers: &HeaderMap) -> Option<(String, UserId, String, Option<String>)> {
    let token = cookie(headers, SESSION_COOKIE)?;
    let (user, csrf, flash) = state.admin.get(&token)?;
    let still_admin = state.db.call(move |c| is_admin(c, user)).await.unwrap_or(false);
    if !still_admin {
        state.admin.remove(&token);
        return None;
    }
    Some((token, user, csrf, flash))
}

fn is_admin(c: &rusqlite::Connection, user: UserId) -> db::DbResult<bool> {
    let member = db::membership(c, user)? == Some(db::MEMBER);
    Ok(member && db::access(c, user)?.can(perms::ADMINISTRATOR))
}

fn login_page(state: &AppState, error: Option<&str>, secure: bool) -> Response {
    let csrf = random_hex(16);
    let error = error.map(|e| format!("<div class=\"notice\">{}</div>", esc(e))).unwrap_or_default();
    let body = format!(
        "<section class=\"login\"><h1>{}</h1><p class=\"muted\">Server admin. Sign in with the owner's \
         or an administrator's Celerity account.</p>{error}\
         <form method=\"post\" action=\"/admin/login\">\
         <input type=\"hidden\" name=\"csrf\" value=\"{csrf}\">\
         <label for=\"u\">Username</label><input id=\"u\" name=\"username\" autocomplete=\"username\" required autofocus>\
         <label for=\"p\">Password</label><input id=\"p\" name=\"password\" type=\"password\" autocomplete=\"current-password\" required>\
         <button type=\"submit\">Sign in</button></form></section>",
        esc(&state.config.community_name)
    );
    let mut resp = page("Sign in · Celerity admin", &body);
    if let Ok(v) = HeaderValue::from_str(&set_cookie(LOGIN_COOKIE, &csrf, 3600, secure)) {
        resp.headers_mut().append(header::SET_COOKIE, v);
    }
    resp
}

fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["bytes", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 { format!("{n} bytes") } else { format!("{v:.1} {}", UNITS[i]) }
}

/// Total size of the files in a folder (not following links).
fn folder_size(dir: &Path) -> u64 {
    let mut total = 0;
    let mut todo = vec![dir.to_path_buf()];
    while let Some(d) = todo.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else { continue };
        for e in entries.flatten() {
            match e.file_type() {
                Ok(t) if t.is_dir() => todo.push(e.path()),
                Ok(t) if t.is_file() => total += e.metadata().map(|m| m.len()).unwrap_or(0),
                _ => {}
            }
        }
    }
    total
}

/// Free space on the disk holding `dir`, where the system can tell.
#[cfg(unix)]
pub fn free_space(dir: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(dir.as_os_str().as_bytes()).ok()?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `path` is a valid NUL-terminated string and `stat` is a writable statvfs.
    let rc = unsafe { libc::statvfs(path.as_ptr(), &mut stat) };
    (rc == 0).then(|| stat.f_bavail as u64 * stat.f_frsize as u64)
}

#[cfg(not(unix))]
pub fn free_space(_dir: &Path) -> Option<u64> {
    None
}

/// Database file size, including its write-ahead log.
fn database_size(path: Option<&Path>) -> Option<u64> {
    let path = path?;
    let mut total = std::fs::metadata(path).ok()?.len();
    for ext in ["-wal", "-shm"] {
        let mut p = path.as_os_str().to_owned();
        p.push(ext);
        total += std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
    }
    Some(total)
}

/// The address invite links should use: the configured one, or how this page was reached.
fn invite_address(state: &AppState, headers: &HeaderMap) -> String {
    let configured = state.config.public_address.trim();
    if !configured.is_empty() {
        return configured.to_string();
    }
    headers.get(header::HOST).and_then(|h| h.to_str().ok()).unwrap_or("localhost:7878").to_string()
}

fn when(ms: i64) -> String {
    let secs = ms / 1000;
    format!("{} {:02}:{:02} UTC", backup::date_of(secs), secs.rem_euclid(86_400) / 3600, secs.rem_euclid(3600) / 60)
}

async fn dashboard(State(state): State<Arc<AppState>>, headers: HeaderMap, peer: Peer0) -> Response {
    let Some((_, me, csrf, flash)) = signed_in(&state, &headers).await else {
        return login_page(&state, None, is_tls(&peer));
    };
    let online: Vec<UserId> = state.online.lock().unwrap().keys().copied().collect();
    let data = state
        .db
        .call(move |c| {
            let users = db::all_users(c)?;
            let roles = db::all_roles(c)?;
            let invites = db::usable_invites(c)?;
            let mut removable = Vec::new();
            for u in &users {
                if crate::moderation::strictly_outranks(c, me, u.id)? {
                    removable.push(u.id);
                }
            }
            Ok((users, roles, invites, removable))
        })
        .await;
    let Ok((users, roles, invites, removable)) = data else {
        return (StatusCode::INTERNAL_SERVER_ERROR, "database error").into_response();
    };
    let db_path = state.db.path().map(Path::to_path_buf);
    let uploads = state.config.uploads.clone();
    let (db_size, uploads_size, free, backups) = tokio::task::spawn_blocking(move || {
        let backups = db_path.as_deref().map(|p| backup::list(&backup::folder_for(p))).unwrap_or_default();
        (database_size(db_path.as_deref()), folder_size(&uploads), free_space(&uploads), backups)
    })
    .await
    .unwrap_or((None, 0, None, Vec::new()));

    let name_of = |id: UserId| users.iter().find(|u| u.id == id).map(|u| u.name().to_string()).unwrap_or_default();
    let hidden = format!("<input type=\"hidden\" name=\"csrf\" value=\"{csrf}\">");
    let mut b = String::new();
    let _ = write!(
        b,
        "<header><h1>{} <span class=\"muted\">· admin</span></h1>\
         <form method=\"post\" action=\"/admin/logout\">{hidden}<button class=\"quiet\">Sign out</button></form></header>",
        esc(&state.config.community_name)
    );
    if let Some(f) = flash {
        let _ = write!(b, "<div class=\"notice\">{}</div>", esc(&f));
    }

    // Online now and storage.
    let members = users.iter().filter(|u| !u.removed).count();
    let _ = write!(
        b,
        "<section><h2>Overview</h2><div class=\"grid\">\
         <div class=\"stat\"><b>{}</b>online now</div><div class=\"stat\"><b>{members}</b>members</div>\
         <div class=\"stat\"><b>{}</b>database</div><div class=\"stat\"><b>{}</b>uploads</div>\
         <div class=\"stat\"><b>{}</b>free disk space</div></div>",
        online.len(),
        db_size.map(human_bytes).unwrap_or_else(|| "in memory".into()),
        human_bytes(uploads_size),
        free.map(human_bytes).unwrap_or_else(|| "unknown".into()),
    );
    let mut names: Vec<String> = online.iter().map(|u| esc(&name_of(*u))).collect();
    names.sort();
    let _ = write!(
        b,
        "<p>{}</p></section>",
        if names.is_empty() {
            "<span class=\"muted\">Nobody is online.</span>".to_string()
        } else {
            names.iter().map(|n| format!("<span class=\"pill online\">● {n}</span>")).collect::<String>()
        }
    );

    // Invite links.
    let address = invite_address(&state, &headers);
    let _ = write!(
        b,
        "<section><h2>Invite links</h2><form method=\"post\" action=\"/admin/invites\" class=\"row\">{hidden}\
         <label>Lasts <select name=\"max_age_hours\"><option value=\"1\">1 hour</option>\
         <option value=\"24\">1 day</option><option value=\"168\" selected>7 days</option>\
         <option value=\"720\">30 days</option><option value=\"0\">forever</option></select></label>\
         <label>Uses <select name=\"max_uses\"><option value=\"1\">1</option><option value=\"5\">5</option>\
         <option value=\"10\">10</option><option value=\"0\" selected>no limit</option></select></label>\
         <button>Make an invite link</button></form>"
    );
    if invites.is_empty() {
        b.push_str("<p class=\"muted\">No invite links yet.</p>");
    } else {
        b.push_str("<table><tr><th>Link</th><th>Made by</th><th>Uses</th><th>Expires</th><th></th></tr>");
        for i in &invites {
            let uses = match i.max_uses {
                Some(m) => format!("{} of {m}", i.uses),
                None => i.uses.to_string(),
            };
            let _ = write!(
                b,
                "<tr><td><code>{}</code><br><span class=\"muted\">or the page <code>/invite/{}</code></span></td>\
                 <td>{}</td><td>{uses}</td><td>{}</td><td>\
                 <form method=\"post\" action=\"/admin/invites/revoke\">{hidden}\
                 <input type=\"hidden\" name=\"token\" value=\"{}\"><button class=\"danger\">Revoke</button></form></td></tr>",
                esc(&invite_link(&address, &i.token)),
                esc(&i.token),
                esc(&name_of(i.created_by)),
                i.expires_at.map(when).unwrap_or_else(|| "never".into()),
                esc(&i.token),
            );
        }
        b.push_str("</table>");
    }
    b.push_str("</section>");

    // Members.
    b.push_str("<section><h2>Members</h2><table><tr><th>Name</th><th>Roles</th><th></th></tr>");
    for u in users.iter().filter(|u| !u.removed) {
        let role_names: String = u
            .roles
            .iter()
            .filter_map(|r| roles.iter().find(|x| x.id == *r))
            .map(|r| format!("<span class=\"pill\">{}</span>", esc(&r.name)))
            .collect();
        let owner = if u.is_owner { "<span class=\"pill\">owner</span>" } else { "" };
        let status = if online.contains(&u.id) { " <span class=\"online\">●</span>" } else { "" };
        let actions = if removable.contains(&u.id) {
            format!(
                "<form method=\"post\" action=\"/admin/kick\" onsubmit=\"return confirm('Kick {n}? They can come back with a new invite.')\">{hidden}\
                 <input type=\"hidden\" name=\"user_id\" value=\"{id}\"><button class=\"quiet\">Kick</button></form> \
                 <form method=\"post\" action=\"/admin/ban\" onsubmit=\"return confirm('Ban {n}? They won\\'t be able to come back.')\">{hidden}\
                 <input type=\"hidden\" name=\"user_id\" value=\"{id}\"><button class=\"danger\">Ban</button></form>",
                n = esc(&u.username).replace('\\', ""),
                id = u.id
            )
        } else {
            String::new()
        };
        let _ = write!(
            b,
            "<tr><td>{}{status} <span class=\"muted\">@{}</span></td><td>{owner}{role_names}</td><td>{actions}</td></tr>",
            esc(u.name()),
            esc(&u.username)
        );
    }
    b.push_str("</table>");
    let banned: Vec<_> = users.iter().filter(|u| u.banned).collect();
    if !banned.is_empty() {
        b.push_str("<h2 style=\"margin-top:18px\">Banned</h2><table>");
        for u in banned {
            let _ = write!(
                b,
                "<tr><td>{} <span class=\"muted\">@{}</span></td><td><form method=\"post\" action=\"/admin/unban\">{hidden}\
                 <input type=\"hidden\" name=\"user_id\" value=\"{}\"><button class=\"quiet\">Unban</button></form></td></tr>",
                esc(u.name()),
                esc(&u.username),
                u.id
            );
        }
        b.push_str("</table>");
    }
    b.push_str("</section>");

    // Backups.
    b.push_str("<section><h2>Backups</h2>");
    if !state.config.backups {
        b.push_str("<p class=\"muted\">Daily backups are off (<code>backups = false</code> in the settings file).</p>");
    }
    if backups.is_empty() {
        b.push_str("<p class=\"muted\">No backups yet.</p>");
    } else {
        b.push_str("<table><tr><th>File</th><th>Date</th><th>Size</th></tr>");
        for f in &backups {
            let _ = write!(
                b,
                "<tr><td><code>backups/{}</code></td><td>{}</td><td>{}</td></tr>",
                esc(&f.name),
                f.date,
                human_bytes(f.size)
            );
        }
        b.push_str("</table>");
    }
    let _ = write!(
        b,
        "<p class=\"muted\">The newest {} are kept, next to the database. Uploaded files aren't included: \
         back up the uploads folder yourself.</p>\
         <form method=\"post\" action=\"/admin/backup\">{hidden}<button class=\"quiet\">Back up now</button></form></section>",
        backup::KEEP
    );
    page(&format!("{} · Celerity admin", state.config.community_name), &b)
}

/// Where a form goes back to, with a message for the signed-in admin.
fn back(state: &AppState, token: &str, message: impl Into<String>) -> Response {
    state.admin.flash(token, message);
    Redirect::to("/admin").into_response()
}

/// Check the session and CSRF token of a form; `Err` is the response to send instead.
async fn check_form(
    state: &AppState,
    headers: &HeaderMap,
    form: &HashMap<String, String>,
) -> Result<(String, UserId), Box<Response>> {
    let Some((token, user, csrf, _)) = signed_in(state, headers).await else {
        return Err(Box::new(Redirect::to("/admin").into_response()));
    };
    match form.get("csrf") {
        Some(sent) if same(sent, &csrf) => Ok((token, user)),
        _ => Err(Box::new(
            (StatusCode::FORBIDDEN, "This form has expired. Go back, reload the page and try again.").into_response(),
        )),
    }
}

async fn login(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    peer: Peer0,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    let secure = is_tls(&peer);
    let expected = cookie(&headers, LOGIN_COOKIE);
    if !matches!((expected, form.get("csrf")), (Some(a), Some(b)) if same(&a, b)) {
        return login_page(&state, Some("That sign-in form expired. Try again."), secure);
    }
    let username = form.get("username").map(|u| u.trim().to_string()).unwrap_or_default();
    let password = form.get("password").cloned().unwrap_or_default();
    let ip = peer.as_ref().map(|p| p.0.0.addr.ip().to_string()).unwrap_or_else(|| "unknown".into());
    let keys = [format!("ip:{ip}"), format!("user:{}", username.to_lowercase())];
    if state.admin.too_many_failures(&keys) {
        return login_page(&state, Some("Too many failed sign-ins. Wait a few minutes and try again."), secure);
    }
    let name = username.clone();
    let creds = state.db.call(move |c| db::credentials(c, &name)).await.ok().flatten();
    let user = match creds {
        Some(creds) => tokio::task::spawn_blocking(move || {
            crate::gateway::verify_password(&password, &creds.password_hash).then_some(creds.id)
        })
        .await
        .ok()
        .flatten(),
        None => None,
    };
    let Some(user) = user else {
        state.admin.failed(&keys);
        return login_page(&state, Some("Wrong username or password."), secure);
    };
    if !state.db.call(move |c| is_admin(c, user)).await.unwrap_or(false) {
        state.admin.failed(&keys);
        return login_page(&state, Some("Only the owner and administrators can use this page."), secure);
    }
    tracing::info!("{username} signed in to the admin page from {ip}");
    let token = state.admin.create(user);
    let mut resp = Redirect::to("/admin").into_response();
    let h = resp.headers_mut();
    for c in
        [set_cookie(SESSION_COOKIE, &token, SESSION_TTL.as_secs(), secure), set_cookie(LOGIN_COOKIE, "", 0, secure)]
    {
        if let Ok(v) = HeaderValue::from_str(&c) {
            h.append(header::SET_COOKIE, v);
        }
    }
    resp
}

async fn logout(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    peer: Peer0,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    let (token, _) = match check_form(&state, &headers, &form).await {
        Ok(t) => t,
        Err(r) => return *r,
    };
    state.admin.remove(&token);
    let mut resp = Redirect::to("/admin").into_response();
    if let Ok(v) = HeaderValue::from_str(&set_cookie(SESSION_COOKIE, "", 0, is_tls(&peer))) {
        resp.headers_mut().append(header::SET_COOKIE, v);
    }
    resp
}

async fn create_invite(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    let (token, me) = match check_form(&state, &headers, &form).await {
        Ok(t) => t,
        Err(r) => return *r,
    };
    let hours: i64 = form.get("max_age_hours").and_then(|v| v.parse().ok()).unwrap_or(0).clamp(0, 24 * 365);
    let uses: u32 = form.get("max_uses").and_then(|v| v.parse().ok()).unwrap_or(0).min(1000);
    let expires_at = (hours > 0).then(|| db::now_ms() + hours * 3600 * 1000);
    let code = crate::gateway::random_token(10);
    let made = state.db.call(move |c| db::create_invite(c, &code, me, expires_at, (uses > 0).then_some(uses))).await;
    match made {
        Ok(_) => back(&state, &token, "Made a new invite link."),
        Err(_) => back(&state, &token, "Couldn't make an invite link. Try again."),
    }
}

async fn revoke_invite(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    let (token, _) = match check_form(&state, &headers, &form).await {
        Ok(t) => t,
        Err(r) => return *r,
    };
    let code = form.get("token").cloned().unwrap_or_default();
    match state.db.call(move |c| db::revoke_invite(c, &code)).await {
        Ok(true) => back(&state, &token, "Revoked the invite link."),
        _ => back(&state, &token, "That invite link was already gone."),
    }
}

async fn moderate(state: Arc<AppState>, headers: HeaderMap, form: HashMap<String, String>, action: &str) -> Response {
    let (token, me) = match check_form(&state, &headers, &form).await {
        Ok(t) => t,
        Err(r) => return *r,
    };
    let Some(target) = form.get("user_id").and_then(|v| v.parse::<UserId>().ok()) else {
        return back(&state, &token, "That person doesn't exist.");
    };
    let name =
        state.db.call(move |c| db::get_user(c, target)).await.ok().flatten().map(|u| u.username).unwrap_or_default();
    let result = match action {
        "kick" => crate::moderation::remove(&state, me, target, false).await,
        "ban" => crate::moderation::remove(&state, me, target, true).await,
        _ => crate::moderation::unban(&state, me, target).await,
    };
    match result {
        Ok(()) => back(
            &state,
            &token,
            match action {
                "kick" => format!("Kicked {name}."),
                "ban" => format!("Banned {name}."),
                _ => format!("Unbanned {name}. They can come back with a new invite."),
            },
        ),
        Err(e) => back(&state, &token, e),
    }
}

async fn kick(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    moderate(state, headers, form, "kick").await
}

async fn ban(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    moderate(state, headers, form, "ban").await
}

async fn unban(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    moderate(state, headers, form, "unban").await
}

async fn backup_now(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    let (token, _) = match check_form(&state, &headers, &form).await {
        Ok(t) => t,
        Err(r) => return *r,
    };
    let Some(path) = state.db.path().map(Path::to_path_buf) else {
        return back(&state, &token, "This server's database isn't a file, so it can't be backed up.");
    };
    let made = tokio::task::spawn_blocking(move || {
        let dir = backup::folder_for(&path);
        let made = backup::make(&path, &dir, &backup::today());
        backup::prune(&dir, backup::KEEP);
        made
    })
    .await;
    match made {
        Ok(Ok(file)) => back(
            &state,
            &token,
            format!("Backed up to {}.", file.file_name().and_then(|n| n.to_str()).unwrap_or("the backups folder")),
        ),
        Ok(Err(e)) => back(&state, &token, format!("The backup didn't work: {e}")),
        Err(_) => back(&state, &token, "The backup didn't work."),
    }
}

/// `GET /invite/<token>`: a page with an "Open in Celerity" button.
pub async fn invite_page(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    UrlPath(token): UrlPath<String>,
) -> Response {
    let valid_token = !token.is_empty() && token.len() <= 64 && token.bytes().all(|b| b.is_ascii_alphanumeric());
    let t = token.clone();
    let invite = if valid_token { state.db.call(move |c| db::get_invite(c, &t)).await.ok().flatten() } else { None };
    let name = esc(&state.config.community_name);
    let usable = invite.as_ref().is_some_and(|i| i.usable(db::now_ms()));
    let body = if usable {
        let link = invite_link(&invite_address(&state, &headers), &token);
        format!(
            "<section class=\"login\" style=\"text-align:center\"><p class=\"muted\">You're invited to join</p>\
             <h1 style=\"font-size:28px;margin:4px 0 18px\">{name}</h1>\
             <p><a href=\"{link}\"><button style=\"width:100%\">Open in Celerity</button></a></p>\
             <p class=\"muted\">Don't have Celerity yet? <a style=\"color:#00a8fc\" href=\"{DOWNLOAD_URL}\">Download it</a>, \
             then come back and click the button.</p>\
             <p class=\"muted\">Or register in the app with the invite code <code>{code}</code>.</p></section>",
            link = esc(&link),
            code = esc(&token),
        )
    } else {
        format!(
            "<section class=\"login\" style=\"text-align:center\"><h1>{name}</h1>\
             <p>This invite has expired or was revoked. Ask whoever sent it for a new one.</p></section>"
        )
    };
    let mut resp = page(&format!("Join {} on Celerity", state.config.community_name), &body);
    if !usable {
        *resp.status_mut() = StatusCode::NOT_FOUND;
    }
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escaping_and_cookies() {
        assert_eq!(esc("<a href=\"x\">'&'</a>"), "&lt;a href=&quot;x&quot;&gt;&#39;&amp;&#39;&lt;/a&gt;");
        let mut h = HeaderMap::new();
        h.insert(header::COOKIE, HeaderValue::from_static("a=1; celerity_admin=abc; b=2"));
        assert_eq!(cookie(&h, SESSION_COOKIE).as_deref(), Some("abc"));
        assert_eq!(cookie(&h, "missing"), None);
        assert!(set_cookie("x", "y", 5, true).contains("HttpOnly; SameSite=Strict; Path=/admin; Max-Age=5; Secure"));
        assert!(same("abc", "abc") && !same("abc", "abd") && !same("abc", "ab"));
        assert_eq!(human_bytes(1536), "1.5 KB");
    }

    #[test]
    fn failed_sign_ins_are_limited() {
        let s = Sessions::default();
        let keys = ["ip:1.2.3.4".to_string()];
        for _ in 0..MAX_FAILURES {
            assert!(!s.too_many_failures(&keys));
            s.failed(&keys);
        }
        assert!(s.too_many_failures(&keys));
        assert!(!s.too_many_failures(&["ip:5.6.7.8".to_string()]), "other addresses are fine");
        let token = s.create(7);
        assert_eq!(s.get(&token).map(|(u, ..)| u), Some(7));
        s.forget_user(7);
        assert!(s.get(&token).is_none());
    }
}
