//! The web UI: the files in `ui/`, a small JSON API over her memory, and a
//! WebSocket that streams the core's events and carries commands back.
//!
//! Who may use it:
//! - this machine (`127.0.0.1`, `localhost`), freely;
//! - any other device only with the secret key (`UI_BIND=0.0.0.0` prints a
//!   link containing it; the key then lives in a cookie);
//! - never another website open in your browser: a request carrying a
//!   foreign `Origin` is refused, and so is a foreign `Host` on the loopback
//!   address (DNS rebinding).

use std::net::{IpAddr, SocketAddr};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result};
use axum::Router;
use axum::body::Body;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, Path as UrlPath, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::get;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::broadcast::error::RecvError;

use crate::core_loop::{Command, CoreHandle, Input};
use crate::memory::Memory;
use crate::speech_out::VOICES;

const COOKIE: &str = "lasthope_key";
/// Longest line accepted from the "speak in writing" field.
const MAX_SAID: usize = 2000;

#[derive(Clone)]
struct App {
    handle: CoreHandle,
    memory: Memory,
    dir: PathBuf,
    /// Required from other devices; None when bound to loopback only.
    key: Option<String>,
    /// "voice" or "text"
    mode: &'static str,
    /// The grimoire's PDFs.
    books_dir: PathBuf,
}

/// Start the server in the background. Returns the links to print.
pub async fn start(
    bind: IpAddr,
    port: u16,
    dir: PathBuf,
    handle: CoreHandle,
    memory: Memory,
    mode: &'static str,
    books_dir: PathBuf,
) -> Result<Vec<String>> {
    anyhow::ensure!(dir.join("index.html").exists(), "no web UI files in {}", dir.display());
    let key = if bind.is_loopback() { None } else { Some(ui_key(&memory).await?) };
    let listener = tokio::net::TcpListener::bind((bind, port))
        .await
        .with_context(|| format!("listening on {bind}:{port}"))?;

    let mut links = vec![format!("http://localhost:{port}/")];
    if let Some(key) = &key {
        let ip = lan_ip().map_or_else(|| "<this-machine's-ip>".into(), |ip| ip.to_string());
        links.push(format!("http://{ip}:{port}/?key={key}  (other devices on your network)"));
    }

    let app = App { handle, memory, dir, key, mode, books_dir };
    let router = Router::new()
        .route("/api/chamber", get(chamber))
        .route("/api/sessions", get(sessions))
        .route("/api/sessions/{id}", get(session))
        .route("/api/search", get(search))
        .route("/api/grimoire", get(grimoire))
        .route("/api/books/{id}/file", get(book_file))
        .route("/ws", get(socket))
        .fallback(get(file))
        .layer(middleware::from_fn_with_state(app.clone(), guard))
        .with_state(app);
    tokio::spawn(async move {
        let service = router.into_make_service_with_connect_info::<SocketAddr>();
        if let Err(e) = axum::serve(listener, service).await {
            eprintln!("  ! web UI stopped: {e}");
        }
    });
    Ok(links)
}

// --- who may come in ------------------------------------------------------

async fn guard(
    State(app): State<App>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    req: Request,
    next: Next,
) -> Response {
    let headers = req.headers();
    let host = headers.get(header::HOST).and_then(|h| h.to_str().ok()).unwrap_or("");
    if let Some(origin) = headers.get(header::ORIGIN) {
        let origin = origin.to_str().unwrap_or("null");
        let origin_host = origin.split_once("://").map_or("", |(_, h)| h);
        if origin_host != host {
            return refuse(StatusCode::FORBIDDEN, "Another site may not command her.");
        }
    }
    if peer.ip().is_loopback() && is_local_host(host) {
        return next.run(req).await;
    }
    let Some(key) = app.key.as_deref() else {
        return refuse(StatusCode::FORBIDDEN, "This chamber only opens on its own machine.");
    };
    if cookie(headers, COOKIE).is_some_and(|c| same(c, key)) {
        return next.run(req).await;
    }
    let from_link = req.uri().query().and_then(|q| query_value(q, "key")).is_some_and(|k| same(&k, key));
    if !from_link {
        return refuse(
            StatusCode::UNAUTHORIZED,
            "The chamber is sealed. Open the link with the key that lasthope printed in its terminal.",
        );
    }
    let mut res = next.run(req).await;
    let cookie = format!("{COOKIE}={key}; Path=/; HttpOnly; SameSite=Strict; Max-Age=31536000");
    if let Ok(v) = HeaderValue::from_str(&cookie) {
        res.headers_mut().append(header::SET_COOKIE, v);
    }
    res
}

fn is_local_host(host: &str) -> bool {
    let name = match host.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or(""),
        None => host.split(':').next().unwrap_or(""),
    };
    matches!(name, "localhost" | "127.0.0.1" | "::1")
}

fn cookie<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|kv| kv.trim().split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v)
}

fn query_value(query: &str, name: &str) -> Option<String> {
    query.split('&').filter_map(|kv| kv.split_once('=')).find(|(k, _)| *k == name).map(|(_, v)| v.to_string())
}

/// Compare secrets without stopping at the first difference.
fn same(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn refuse(status: StatusCode, why: &str) -> Response {
    let page = format!(
        "<!doctype html><meta charset=utf-8><meta name=viewport content='width=device-width'>\
         <title>Lasthope</title><body style='background:#070506;color:#DDD2C6;font:16px Georgia,serif;\
         padding:48px 16px;text-align:center'><p>{why}</p>"
    );
    (status, [(header::CONTENT_TYPE, "text/html; charset=utf-8")], page).into_response()
}

/// The key for other devices: made once, kept in memory, so a bookmark on
/// the phone keeps working.
async fn ui_key(memory: &Memory) -> Result<String> {
    if let Some(key) = memory.setting("ui_key").await? {
        return Ok(key);
    }
    let mut bytes = [0u8; 16];
    std::io::Read::read_exact(&mut std::fs::File::open("/dev/urandom")?, &mut bytes)?;
    let key: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    memory.set_setting("ui_key", &key).await?;
    Ok(key)
}

/// This machine's address on the local network (no packet is sent: a UDP
/// "connect" only picks the outgoing interface).
fn lan_ip() -> Option<IpAddr> {
    let s = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("10.255.255.255:9").ok()?;
    Some(s.local_addr().ok()?.ip()).filter(|ip| !ip.is_loopback() && !ip.is_unspecified())
}

// --- files ------------------------------------------------------------------

async fn file(State(app): State<App>, uri: axum::http::Uri) -> Response {
    let path = match uri.path() {
        "/" => "index.html",
        "/archive" => "archive.html",
        "/grimoire" => "grimoire.html",
        p => p.trim_start_matches('/'),
    };
    let Some(full) = safe_join(&app.dir, path) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    match tokio::fs::read(&full).await {
        Ok(bytes) => (
            [(header::CONTENT_TYPE, content_type(&full)), (header::CACHE_CONTROL, "no-cache")],
            Body::from(bytes),
        )
            .into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

/// `dir/path`, only for plain relative paths (no `..`, no hidden files).
fn safe_join(dir: &Path, path: &str) -> Option<PathBuf> {
    let rel = Path::new(path);
    let ok = rel.components().all(|c| matches!(c, Component::Normal(s) if !s.to_string_lossy().starts_with('.')));
    (ok && !path.is_empty()).then(|| dir.join(rel))
}

fn content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "wasm" => "application/wasm",
        "ttf" => "font/ttf",
        "pdf" => "application/pdf",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "woff2" => "font/woff2",
        "txt" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

// --- the API ----------------------------------------------------------------

struct Failure(anyhow::Error);

impl<E: Into<anyhow::Error>> From<E> for Failure {
    fn from(e: E) -> Self {
        Self(e.into())
    }
}

impl IntoResponse for Failure {
    fn into_response(self) -> Response {
        (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": format!("{:#}", self.0) }))).into_response()
    }
}

/// Everything the Chamber shows, at this moment.
async fn chamber(State(app): State<App>) -> Result<Json<Value>, Failure> {
    let m = &app.memory;
    let live = app.handle.live();
    let today = m.today().await?;
    let transcript = match live.session {
        Some(s) => m.session_lines(s).await?,
        None => vec![],
    };
    let pending = m.pending_oath(today, live.session.unwrap_or(0)).await?;
    let upcoming = m.upcoming_oath(today).await?;
    let focus = m.focus_on(today).await?;
    let voice: i32 = m.setting("voice").await?.and_then(|v| v.parse().ok()).unwrap_or(crate::speech_out::DEFAULT_VOICE);
    let speed: f32 = m.setting("speed").await?.and_then(|v| v.parse().ok()).unwrap_or(crate::speech_out::DEFAULT_SPEED);
    Ok(Json(json!({
        "mode": app.mode,
        "today": today.to_string(),
        "live": live,
        "transcript": transcript,
        "mission": m.mission().await?,
        "streak": m.streak().await?.current(today),
        "pending_oath": pending.map(|o| json!({ "text": o.text, "day": o.day.to_string() })),
        "upcoming_oath": upcoming.map(|o| json!({ "text": o.text, "day": o.day.to_string() })),
        "facts": m.facts().await?,
        "focus_today": { "minutes": focus.minutes, "vigils": focus.vigils },
        "settings": {
            "voice": voice,
            "pace": (speed * 100.0).round() as u32,
            "speaks_first": m.speaks_first().await?,
        },
        "voices": VOICES.iter().map(|(id, name)| json!({ "id": id, "name": name })).collect::<Vec<_>>(),
    })))
}

async fn sessions(State(app): State<App>) -> Result<Json<Value>, Failure> {
    let open = app.handle.live().session;
    Ok(Json(json!({ "open": open, "sessions": app.memory.sessions().await? })))
}

async fn session(State(app): State<App>, UrlPath(id): UrlPath<i64>) -> Result<Response, Failure> {
    let m = &app.memory;
    let Some(info) = m.session_info(id).await? else {
        return Ok((StatusCode::NOT_FOUND, Json(json!({ "error": "no such night" }))).into_response());
    };
    Ok(Json(json!({
        "open": app.handle.live().session == Some(id),
        "session": info,
        "lines": m.session_lines(id).await?,
        "learned": m.facts_of_session(id).await?,
    }))
    .into_response())
}

#[derive(Deserialize)]
struct SearchQuery {
    q: String,
}

async fn search(State(app): State<App>, Query(q): Query<SearchQuery>) -> Result<Json<Value>, Failure> {
    let hits = app.memory.search(&q.q, 30, None).await?;
    Ok(Json(json!({
        "hits": hits.iter().map(|h| json!({
            "session": h.session,
            "day": h.day.to_string(),
            "speaker": h.speaker,
            "text": h.text,
        })).collect::<Vec<_>>(),
    })))
}

/// The shelf, the page open, and the questions asked.
async fn grimoire(State(app): State<App>) -> Result<Json<Value>, Failure> {
    let m = &app.memory;
    let live = app.handle.live();
    let (answered, right) = m.question_tally().await?;
    Ok(Json(json!({
        "dir": app.books_dir.display().to_string(),
        "dir_exists": app.books_dir.is_dir(),
        "open": live.session,
        "indexing": live.indexing,
        "reading": live.reading,
        "current": m.current_book().await?.map(|b| b.id),
        "books": m.books().await?,
        "questions": m.questions(None, 60).await?,
        "tally": { "answered": answered, "right": right },
    })))
}

/// The PDF itself, for the viewer.
async fn book_file(State(app): State<App>, UrlPath(id): UrlPath<i64>) -> Result<Response, Failure> {
    let Some(book) = app.memory.book(id).await? else {
        return Ok(StatusCode::NOT_FOUND.into_response());
    };
    let Some(full) = safe_join(&app.books_dir, &book.path) else {
        return Ok(StatusCode::NOT_FOUND.into_response());
    };
    Ok(match tokio::fs::read(&full).await {
        Ok(bytes) => ([(header::CONTENT_TYPE, "application/pdf"), (header::CACHE_CONTROL, "no-cache")], Body::from(bytes))
            .into_response(),
        Err(_) => (StatusCode::NOT_FOUND, Json(json!({ "error": "the file has left the books folder" }))).into_response(),
    })
}

// --- the live line ----------------------------------------------------------

async fn socket(ws: WebSocketUpgrade, State(app): State<App>) -> Response {
    ws.on_upgrade(move |socket| client(socket, app))
}

/// Events go out as JSON (`{"event": …}`); commands come in as JSON
/// (`{"cmd": …}`, see `Command`), plus `{"cmd": "say", "text": …}` for
/// words typed to her.
async fn client(mut socket: WebSocket, app: App) {
    let mut events = app.handle.subscribe();
    loop {
        tokio::select! {
            msg = socket.recv() => match msg {
                Some(Ok(Message::Text(text))) => from_client(&app, &text),
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                Some(Ok(_)) => {}
            },
            event = events.recv() => {
                let json = match event {
                    Ok(event) => serde_json::to_string(&event).unwrap_or_default(),
                    // the page missed events: it re-reads everything
                    Err(RecvError::Lagged(_)) => r#"{"event":"lagged"}"#.into(),
                    Err(RecvError::Closed) => break,
                };
                if socket.send(Message::Text(json.into())).await.is_err() {
                    break;
                }
            }
        }
    }
}

fn from_client(app: &App, text: &str) {
    let Ok(value) = serde_json::from_str::<Value>(text) else { return };
    if value["cmd"] == "say" {
        let said = value["text"].as_str().unwrap_or("").trim();
        if !said.is_empty() {
            app.handle.send(Input::Heard(said.chars().take(MAX_SAID).collect()));
        }
        return;
    }
    match serde_json::from_value::<Command>(value) {
        Ok(cmd) => app.handle.command(cmd),
        Err(e) => eprintln!("  ! web UI sent an unknown command: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_plain_paths_are_served() {
        let dir = Path::new("/srv/ui");
        assert_eq!(safe_join(dir, "app.css"), Some(dir.join("app.css")));
        assert_eq!(safe_join(dir, "fonts/cinzel.woff2"), Some(dir.join("fonts/cinzel.woff2")));
        assert_eq!(safe_join(dir, "../lasthope.db"), None);
        assert_eq!(safe_join(dir, "fonts/../../.env"), None);
        assert_eq!(safe_join(dir, ".env"), None);
        assert_eq!(safe_join(dir, "/etc/passwd"), None);
        assert_eq!(safe_join(dir, ""), None);
    }

    #[test]
    fn local_hosts_and_secrets() {
        assert!(is_local_host("localhost:7700"));
        assert!(is_local_host("127.0.0.1:7700"));
        assert!(is_local_host("[::1]:7700"));
        assert!(!is_local_host("evil.example:7700"));
        assert!(!is_local_host("192.168.1.5:7700"));
        assert!(same("abc", "abc"));
        assert!(!same("abc", "abd"));
        assert!(!same("abc", "abcd"));
        let mut h = HeaderMap::new();
        h.insert(header::COOKIE, HeaderValue::from_static("a=1; lasthope_key=xyz; b=2"));
        assert_eq!(cookie(&h, COOKIE), Some("xyz"));
        assert_eq!(query_value("x=1&key=abc", "key").as_deref(), Some("abc"));
    }
}
