//! Web-based viewer for tttt (`--http-host` / `--http-port` / `--secure`).
//!
//! **EXPERIMENTAL**: this feature is under active development. Use it only
//! on trusted local networks or over a VPN — do not expose it to the open
//! internet.
//!
//! Serves a browser UI (xterm.js) that talks to tttt over WebSocket using the
//! same `tttt_tui::protocol` JSON messages as `tttt attach`, so a browser can
//! watch and drive any session remotely.
//!
//! Security model:
//! - Loopback (127.0.0.1 / ::1 / localhost) defaults to no auth.
//! - Binding to a non-loopback address without an htpasswd file auto-generates
//!   a random access token, printed on startup and required to connect.
//! - An htpasswd file (`--htpasswd`) enables username/password basic auth.

pub mod auth;
pub mod certs;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tttt_pty::{AnyPty, PtySession, RealPty, SessionManager, SessionStatus};
use tttt_tui::protocol::{ClientMsg, ServerMsg, SessionInfo};

/// Configuration for the web server.
#[derive(Debug, Clone)]
pub struct WebConfig {
    pub host: String,
    pub port: u16,
    /// Serve HTTPS with a (generated unless supplied) certificate.
    pub secure: bool,
    pub tls_cert: Option<PathBuf>,
    pub tls_key: Option<PathBuf>,
    /// htpasswd file for basic auth, if any.
    pub htpasswd: Option<PathBuf>,
    /// Explicit token, if any.
    pub token: Option<String>,
    /// Working directory for newly-created sessions.
    pub work_dir: PathBuf,
}

impl WebConfig {
    /// The auth instance to use, and whether a token was auto-generated.
    fn resolved_auth(&self) -> Result<(auth::Auth, Option<String>), Box<dyn std::error::Error>> {
        let mut auth = match &self.htpasswd {
            Some(ht) => auth::Auth::with_htpasswd(ht)?,
            None => auth::Auth::none(),
        };
        if let Some(tok) = &self.token {
            auth.set_token(tok.clone());
            return Ok((auth, None));
        }
        if self.htpasswd.is_none() && !is_loopback(&self.host) {
            let tok = random_token();
            auth.set_token(tok.clone());
            return Ok((auth, Some(tok)));
        }
        Ok((auth, None))
    }
}

/// Ensure a bind that would auto-generate an access token has that token
/// stored in the config, so the same token survives SIGUSR1 live reload
/// (the reload path restores the saved config and restarts the web server).
pub fn ensure_token(config: &mut crate::config::Config) {
    if config.http_port.is_none() || config.htpasswd.is_some() || config.token.is_some() {
        return;
    }
    let host = config.http_host.as_deref().unwrap_or("127.0.0.1");
    if !is_loopback(host) {
        config.token = Some(random_token());
    }
}

/// True if the host string refers to the loopback interface.
pub fn is_loopback(host: &str) -> bool {
    match host {
        "localhost" | "127.0.0.1" | "::1" | "0:0:0:0:0:0:0:1" => return true,
        _ => {}
    }
    // 127.x.x.x
    if let Some(rest) = host.strip_prefix("127.") {
        return rest.split('.').all(|p| p.parse::<u8>().is_ok());
    }
    false
}

/// Generate a URL-safe random access token.
fn random_token() -> String {
    use base64::Engine;
    use rand::RngCore;
    let mut bytes = [0u8; 24];
    rand::thread_rng().fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// Immutable per-tick view of one session, shared across all connections.
struct SessionSnapshot {
    /// PTY dimensions (cols, rows).
    dims: (u16, u16),
    /// Cursor position (row, col).
    cursor: (u16, u16),
    /// Hash of `content` for change detection.
    content_hash: u64,
    /// Rendered contents_formatted() + input modes.
    content: Arc<Vec<u8>>,
}

/// Immutable per-tick view of the session manager. Built once per tick by
/// the publisher task and fanned out to every connection via a watch
/// channel, so N viewers cost one render + one manager lock per tick
/// instead of N.
#[derive(Default)]
struct Snapshot {
    list: Vec<SessionInfo>,
    list_hash: u64,
    /// Session a viewer should fall back to (root, else first).
    pick_default: Option<String>,
    /// Rendered state for sessions at least one viewer watches.
    sessions: HashMap<String, SessionSnapshot>,
}

/// Refcounts of sessions currently watched by connected viewers; the
/// publisher only renders these.
type WatchedSessions = Arc<Mutex<HashMap<String, usize>>>;

/// Move a viewer's watch refcount from `old` to `new`.
fn retarget_watch(watched: &WatchedSessions, old: Option<&str>, new: Option<&str>) {
    if old == new {
        return;
    }
    let mut w = watched.lock().unwrap();
    if let Some(old) = old {
        if let Some(n) = w.get_mut(old) {
            *n -= 1;
            if *n == 0 {
                w.remove(old);
            }
        }
    }
    if let Some(new) = new {
        *w.entry(new.to_string()).or_insert(0) += 1;
    }
}

/// Shared state for axum handlers.
#[derive(Clone)]
struct WebState {
    sessions: Arc<Mutex<SessionManager<AnyPty>>>,
    auth: Arc<auth::Auth>,
    work_dir: PathBuf,
    snapshot_rx: tokio::sync::watch::Receiver<Arc<Snapshot>>,
    watched: WatchedSessions,
}

/// Shared WebSocket sender (split sink) used by both the push loop and the
/// read loop (for responses to CreateSession/KillSession).
type WsSender = tokio::sync::Mutex<futures_util::stream::SplitSink<WebSocket, Message>>;

/// Shared cell for asynchronous status/errors from the web server thread;
/// the App drains it into the root terminal.
pub type WebStatus = Arc<Mutex<Option<String>>>;

/// Start the web server in a background thread. Returns the URL to open.
/// Configuration problems (bad host, TLS misconfiguration) are validated
/// here, synchronously; later runtime errors are posted to `status`.
pub fn start_web_server(
    cfg: WebConfig,
    sessions: Arc<Mutex<SessionManager<AnyPty>>>,
    status: WebStatus,
) -> Result<String, Box<dyn std::error::Error>> {
    if cfg.tls_cert.is_some() != cfg.tls_key.is_some() {
        return Err("--tls-cert and --tls-key must be supplied together".into());
    }
    let (auth, generated_token) = cfg.resolved_auth()?;

    // Resolve via ToSocketAddrs so hostnames and bare IPv6 work; refuse to
    // start rather than silently binding a different address than requested.
    use std::net::ToSocketAddrs;
    let addr: SocketAddr = (cfg.host.as_str(), cfg.port)
        .to_socket_addrs()
        .map_err(|e| format!("cannot resolve bind address {}:{}: {}", cfg.host, cfg.port, e))?
        .next()
        .ok_or_else(|| format!("no address found for {}:{}", cfg.host, cfg.port))?;

    // Build the TLS config up front so certificate problems surface now.
    let tls_config = if cfg.secure {
        Some(build_tls_config(&cfg)?)
    } else {
        None
    };

    let scheme = if cfg.secure { "https" } else { "http" };
    let display_host = if cfg.host == "0.0.0.0" {
        "localhost".to_string()
    } else {
        cfg.host.clone()
    };
    let base = format!("{}://{}:{}", scheme, display_host, cfg.port);

    // Include the access token in the printed URL whether it was generated
    // here or pre-generated into the config (see `ensure_token`).
    let mut url = base.clone();
    if let Some(tok) = generated_token.as_ref().or(cfg.token.as_ref()) {
        url = format!("{}?token={}", base, tok);
    }

    let (snapshot_tx, snapshot_rx) = tokio::sync::watch::channel(Arc::new(Snapshot::default()));
    let state = WebState {
        sessions,
        auth: Arc::new(auth),
        work_dir: cfg.work_dir.clone(),
        snapshot_rx,
        watched: Arc::new(Mutex::new(HashMap::new())),
    };

    std::thread::Builder::new()
        .name("tttt-web".to_string())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .worker_threads(2)
                .build()
                .expect("failed to build web runtime");
            rt.block_on(serve(addr, tls_config, state, snapshot_tx, status));
        })?;

    Ok(url)
}

async fn serve(
    addr: SocketAddr,
    tls_config: Option<rustls::ServerConfig>,
    state: WebState,
    snapshot_tx: tokio::sync::watch::Sender<Arc<Snapshot>>,
    status: WebStatus,
) {
    tokio::spawn(publisher(
        Arc::clone(&state.sessions),
        Arc::clone(&state.watched),
        snapshot_tx,
    ));
    let app = Router::new()
        .route("/", get(index))
        .route("/app.js", get(serve_app_js))
        .route("/xterm.js", get(serve_xterm_js))
        .route("/xterm.css", get(serve_xterm_css))
        .route("/api/auth", get(auth_info))
        .route("/ws", get(ws_handler))
        .with_state(state);

    let result = match tls_config {
        Some(sc) => {
            let tls = axum_server::tls_rustls::RustlsConfig::from_config(Arc::new(sc));
            axum_server::bind_rustls(addr, tls)
                .serve(app.into_make_service())
                .await
        }
        None => axum_server::bind(addr).serve(app.into_make_service()).await,
    };

    // Runtime failures (e.g. port already in use) are drained by the App
    // into the root terminal; the TUI owns the screen, so no eprintln here.
    if let Err(e) = result {
        *status.lock().unwrap() = Some(format!("web server error: {}", e));
    }
}

fn build_tls_config(cfg: &WebConfig) -> Result<rustls::ServerConfig, Box<dyn std::error::Error>> {
    // Generate a self-signed cert unless the user supplied cert/key files.
    let generated = if cfg.tls_cert.is_none() || cfg.tls_key.is_none() {
        let hosts = vec![
            cfg.host.clone(),
            "localhost".to_string(),
            "127.0.0.1".to_string(),
        ];
        Some(certs::generate_self_signed(&hosts)?)
    } else {
        None
    };
    certs::build_server_config(generated.as_ref(), cfg.tls_cert.as_deref(), cfg.tls_key.as_deref())
}

// --- Static content (embedded) ---

const INDEX_HTML: &str = include_str!("static/index.html");
const APP_JS: &str = include_str!("static/app.js");
const XTERM_JS: &str = include_str!("static/xterm.js");
const XTERM_CSS: &str = include_str!("static/xterm.css");

async fn index() -> Response {
    Html(INDEX_HTML).into_response()
}

async fn serve_app_js() -> Response {
    (
        [(header::CONTENT_TYPE, "application/javascript")],
        APP_JS,
    )
        .into_response()
}

async fn serve_xterm_js() -> Response {
    (
        [(header::CONTENT_TYPE, "application/javascript")],
        XTERM_JS,
    )
        .into_response()
}

async fn serve_xterm_css() -> Response {
    (
        [(header::CONTENT_TYPE, "text/css")],
        XTERM_CSS,
    )
        .into_response()
}

/// Report whether auth is required and its scheme, without leaking secrets.
/// The client uses this to decide what login UI to show.
async fn auth_info(State(state): State<WebState>) -> Response {
    // When both htpasswd and a token are configured, the login form uses
    // basic auth; the token still works via `?token=`/`Bearer`.
    let scheme = if !state.auth.required() {
        "none"
    } else if state.auth.has_htpasswd() {
        "basic"
    } else {
        "token"
    };
    let body = serde_json::json!({ "required": state.auth.required(), "scheme": scheme });
    ([(header::CONTENT_TYPE, "application/json")], body.to_string()).into_response()
}

// --- WebSocket viewer ---

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<WebState>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    if state.auth.required() {
        // bcrypt verification is CPU-heavy (~tens of ms); keep it off the
        // small async worker pool so screen pushes aren't stalled.
        let auth = Arc::clone(&state.auth);
        let ok = tokio::task::spawn_blocking(move || auth.check_request(&query, &headers))
            .await
            .unwrap_or(false);
        if !ok {
            return StatusCode::UNAUTHORIZED.into_response();
        }
    }
    ws.on_upgrade(move |socket| handle_ws(socket, state))
}

/// Per-connection viewer state (mirrors `ViewerClient` for the web).
struct WebViewer {
    active_session: Option<String>,
    last_content_hash: u64,
    last_cursor: (u16, u16),
    last_list_hash: u64,
    /// PTY dimensions last announced to this client via `WindowSize`.
    last_window_size: (u16, u16),
}

impl WebViewer {
    fn new() -> Self {
        Self {
            active_session: None,
            last_content_hash: 0,
            last_cursor: (0, 0),
            last_list_hash: 0,
            last_window_size: (0, 0),
        }
    }

    fn invalidate(&mut self) {
        self.last_content_hash = 0;
        self.last_cursor = (0, 0);
        self.last_window_size = (0, 0);
    }
}

async fn handle_ws(socket: WebSocket, state: WebState) {
    use futures_util::{SinkExt, StreamExt};

    // Split the socket so the push task can write independently of the read
    // loop. Sharing a single socket behind a Mutex deadlocks: the read loop
    // must hold the lock while awaiting recv(), starving the pusher.
    let (sender, mut receiver) = socket.split();
    let sender = Arc::new(tokio::sync::Mutex::new(sender));
    let viewer = Arc::new(tokio::sync::Mutex::new(WebViewer::new()));

    // Pick an initial active session (prefer the root session).
    let active = {
        let mgr = state.sessions.lock().unwrap();
        let list = mgr.list();
        list.iter()
            .find(|m| m.root)
            .or_else(|| list.first())
            .map(|m| m.id.clone())
    };
    retarget_watch(&state.watched, None, active.as_deref());
    viewer.lock().await.active_session = active;

    // Push loop: forward snapshot changes published by the shared publisher
    // task. Wakes only when something actually changed.
    let push_sender = Arc::clone(&sender);
    let push_viewer = Arc::clone(&viewer);
    let push_state = state.clone();
    let push_task = tokio::spawn(async move {
        let mut rx = push_state.snapshot_rx.clone();
        loop {
            let msgs = {
                let snap = Arc::clone(&rx.borrow_and_update());
                let mut v = push_viewer.lock().await;
                viewer_updates(&snap, &mut v, &push_state.watched)
            };
            if !msgs.is_empty() {
                let mut s = push_sender.lock().await;
                for msg in &msgs {
                    if s.send(encode_out_msg(msg)).await.is_err() {
                        return;
                    }
                }
            }
            if rx.changed().await.is_err() {
                return;
            }
        }
    });

    // Read loop: handle client messages.
    while let Some(msg) = receiver.next().await {
        let msg = match msg {
            Ok(m) => m,
            Err(_) => break,
        };
        match msg {
            Message::Text(t) => {
                if handle_incoming(&state, &viewer, &sender, t.as_bytes()).await {
                    break;
                }
            }
            Message::Binary(b) => {
                if handle_incoming(&state, &viewer, &sender, b.as_ref()).await {
                    break;
                }
            }
            Message::Close(_) => break,
            Message::Ping(p) => {
                let mut s = sender.lock().await;
                let _ = s.send(Message::Pong(p)).await;
            }
            _ => {}
        }
    }
    push_task.abort();
    // Drop this viewer's watch refcount so the publisher stops rendering
    // sessions nobody is looking at.
    let active = viewer.lock().await.active_session.clone();
    retarget_watch(&state.watched, active.as_deref(), None);
}

/// Push any updates the viewer needs right now (used by the read loop after
/// a session switch, so the new screen appears without waiting for the next
/// snapshot change).
async fn flush_viewer(
    state: &WebState,
    viewer: &Arc<tokio::sync::Mutex<WebViewer>>,
    sender: &Arc<WsSender>,
) {
    use futures_util::SinkExt;
    let msgs = {
        let snap = Arc::clone(&state.snapshot_rx.borrow());
        let mut v = viewer.lock().await;
        viewer_updates(&snap, &mut v, &state.watched)
    };
    if msgs.is_empty() {
        return;
    }
    let mut s = sender.lock().await;
    for msg in &msgs {
        if s.send(encode_out_msg(msg)).await.is_err() {
            return;
        }
    }
}

/// Parse and handle one incoming client message. Returns true if the
/// connection should close (Detach).
async fn handle_incoming(
    state: &WebState,
    viewer: &Arc<tokio::sync::Mutex<WebViewer>>,
    sender: &Arc<WsSender>,
    data: &[u8],
) -> bool {
    let parsed: Option<ClientMsg> = serde_json::from_slice(data).ok();
    match parsed {
        Some(cmsg) => handle_client_msg(state, viewer, sender, cmsg).await,
        None => false,
    }
}

/// Publisher task: once per tick, render each watched session and publish a
/// shared snapshot. Connections wake only when something actually changed.
async fn publisher(
    sessions: Arc<Mutex<SessionManager<AnyPty>>>,
    watched: WatchedSessions,
    tx: tokio::sync::watch::Sender<Arc<Snapshot>>,
) {
    let mut interval = tokio::time::interval(std::time::Duration::from_millis(50));
    let mut prev = Arc::new(Snapshot::default());
    loop {
        interval.tick().await;
        let snap = build_snapshot(&sessions, &watched);
        if snapshot_changed(&prev, &snap) {
            prev = Arc::new(snap);
            if tx.send(Arc::clone(&prev)).is_err() {
                return; // server dropped
            }
        }
    }
}

/// Build one immutable snapshot of the session manager: list metadata for
/// all sessions, rendered screen state for watched ones.
fn build_snapshot(
    sessions: &Arc<Mutex<SessionManager<AnyPty>>>,
    watched: &WatchedSessions,
) -> Snapshot {
    let watched_ids: Vec<String> = watched.lock().unwrap().keys().cloned().collect();
    let mgr = sessions.lock().unwrap();

    let meta = mgr.list();
    let list: Vec<SessionInfo> = meta
        .iter()
        .map(|m| SessionInfo {
            id: m.id.clone(),
            command: m.command.clone(),
            status: status_str(&m.status),
        })
        .collect();
    let list_json = serde_json::to_vec(&list).unwrap_or_default();
    let list_hash = tttt_tui::viewer::hash_bytes(&list_json);
    let pick_default = meta
        .iter()
        .find(|m| m.root)
        .or_else(|| meta.first())
        .map(|m| m.id.clone());

    let mut snapshot_sessions = HashMap::new();
    for sid in watched_ids {
        let Ok(session) = mgr.get(&sid) else {
            continue; // killed; viewers re-pick via pick_default
        };
        // contents_formatted() PLUS the terminal input modes (bracketed
        // paste, application keypad/cursor, mouse). xterm.js uses the
        // bracketed paste flag to wrap pastes in \x1b[200~...\x1b[201~ so
        // multi-line pastes aren't executed line-by-line by the shell.
        let mut content = session.get_screen_formatted();
        content.extend_from_slice(&session.screen().screen().input_mode_formatted());
        let content_hash = tttt_tui::viewer::hash_bytes(&content);
        snapshot_sessions.insert(
            sid,
            SessionSnapshot {
                dims: session.screen().size(),
                cursor: session.cursor_position(),
                content_hash,
                content: Arc::new(content),
            },
        );
    }

    Snapshot {
        list,
        list_hash,
        pick_default,
        sessions: snapshot_sessions,
    }
}

/// True if anything a viewer could care about differs between snapshots.
fn snapshot_changed(a: &Snapshot, b: &Snapshot) -> bool {
    if a.list_hash != b.list_hash || a.pick_default != b.pick_default {
        return true;
    }
    if a.sessions.len() != b.sessions.len() {
        return true;
    }
    b.sessions.iter().any(|(sid, s)| {
        a.sessions.get(sid).map_or(true, |p| {
            p.content_hash != s.content_hash || p.cursor != s.cursor || p.dims != s.dims
        })
    })
}

/// An outgoing message: protocol JSON, or a raw binary screen frame
/// (`[cursor_row: u16 BE][cursor_col: u16 BE][screen bytes]`) which avoids
/// serializing screen data as a JSON number array (~4x smaller).
enum OutMsg {
    Json(ServerMsg),
    Screen {
        cursor: (u16, u16),
        content: Arc<Vec<u8>>,
    },
}

/// Build the messages a viewer needs to catch up to `snap`.
fn viewer_updates(snap: &Snapshot, viewer: &mut WebViewer, watched: &WatchedSessions) -> Vec<OutMsg> {
    let mut out = Vec::new();

    // (Re)pick an active session if the current one is gone (killed by
    // another viewer or the TUI) or was never set (connected before any
    // session existed). Without this the viewer freezes on a dead session.
    let current_ok = viewer
        .active_session
        .as_ref()
        .map(|sid| snap.list.iter().any(|s| &s.id == sid))
        .unwrap_or(false);
    if !current_ok && viewer.active_session != snap.pick_default {
        retarget_watch(
            watched,
            viewer.active_session.as_deref(),
            snap.pick_default.as_deref(),
        );
        viewer.active_session = snap.pick_default.clone();
        viewer.invalidate();
        // Force a SessionList push so the sidebar highlight follows.
        viewer.last_list_hash = 0;
    }

    // Session list (only when changed)
    if snap.list_hash != viewer.last_list_hash {
        viewer.last_list_hash = snap.list_hash;
        out.push(OutMsg::Json(ServerMsg::SessionList {
            sessions: snap.list.clone(),
            active_id: viewer.active_session.clone(),
        }));
    }

    // Screen update for the active session
    let Some(session) = viewer
        .active_session
        .as_ref()
        .and_then(|sid| snap.sessions.get(sid))
    else {
        return out;
    };
    // Keep the browser terminal at the PTY's dimensions. The TUI owns the
    // PTY size (min-across-viewers arbitration); the web client follows.
    if session.dims != viewer.last_window_size {
        viewer.last_window_size = session.dims;
        out.push(OutMsg::Json(ServerMsg::WindowSize {
            cols: session.dims.0,
            rows: session.dims.1,
        }));
    }
    if session.content_hash == viewer.last_content_hash && session.cursor == viewer.last_cursor {
        return out;
    }
    viewer.last_content_hash = session.content_hash;
    viewer.last_cursor = session.cursor;
    out.push(OutMsg::Screen {
        cursor: session.cursor,
        content: Arc::clone(&session.content),
    });
    out
}

/// Encode an OutMsg as a WebSocket message.
fn encode_out_msg(msg: &OutMsg) -> Message {
    match msg {
        OutMsg::Json(m) => Message::Text(serde_json::to_string(m).unwrap_or_default()),
        OutMsg::Screen { cursor, content } => {
            let mut frame = Vec::with_capacity(4 + content.len());
            frame.extend_from_slice(&cursor.0.to_be_bytes());
            frame.extend_from_slice(&cursor.1.to_be_bytes());
            frame.extend_from_slice(content);
            Message::Binary(frame)
        }
    }
}

/// Handle one client message. Returns true if the connection should close.
async fn handle_client_msg(
    state: &WebState,
    viewer: &Arc<tokio::sync::Mutex<WebViewer>>,
    sender: &Arc<WsSender>,
    msg: ClientMsg,
) -> bool {
    match msg {
        ClientMsg::KeyInput { bytes } => {
            let sid = viewer.lock().await.active_session.clone();
            if let Some(sid) = sid {
                let mut mgr = state.sessions.lock().unwrap();
                if let Ok(session) = mgr.get_mut(&sid) {
                    let _ = session.send_raw(&bytes);
                }
            }
        }
        ClientMsg::SwitchSession { session_id } => {
            let exists = state.sessions.lock().unwrap().exists(&session_id);
            if exists {
                let mut v = viewer.lock().await;
                retarget_watch(&state.watched, v.active_session.as_deref(), Some(&session_id));
                v.active_session = Some(session_id);
                v.invalidate();
                drop(v);
                flush_viewer(state, viewer, sender).await;
            }
        }
        ClientMsg::Resize { .. } => {
            // The PTY size is owned by the TUI (min-across-viewers
            // arbitration in App); resizing it here would garble the local
            // display. The browser terminal follows the PTY instead, via
            // the WindowSize messages pushed from build_updates.
        }
        ClientMsg::CreateSession {
            command,
            args,
            cols,
            rows,
        } => {
            let result = create_session(state, &command, &args, cols, rows);
            match result {
                Ok(new_id) => {
                    // Auto-switch this viewer to the freshly created session.
                    {
                        let mut v = viewer.lock().await;
                        retarget_watch(&state.watched, v.active_session.as_deref(), Some(&new_id));
                        v.active_session = Some(new_id.clone());
                        v.invalidate();
                    }
                    let resp = ServerMsg::SessionCreated {
                        session_id: Some(new_id),
                        error: None,
                    };
                    send_server_msg(sender, &resp).await;
                }
                Err(e) => {
                    let resp = ServerMsg::SessionCreated {
                        session_id: None,
                        error: Some(e),
                    };
                    send_server_msg(sender, &resp).await;
                }
            }
        }
        ClientMsg::KillSession { session_id } => {
            let kill_result: Result<(), String> = {
                let mut mgr = state.sessions.lock().unwrap();
                // Never kill the root session: the TUI depends on it
                // (respawn/restart logic), and with no other sessions
                // running its removal would exit the whole tttt process.
                if mgr.get(&session_id).map(|s| s.is_root()).unwrap_or(false) {
                    Err("cannot close the root session from the web UI".to_string())
                } else {
                    mgr.kill_session(&session_id).map_err(|e| e.to_string())
                }
            };
            // If this viewer was watching the killed session, switch it away.
            // (Other viewers re-pick via `pick_default` in viewer_updates.)
            {
                let mut v = viewer.lock().await;
                if v.active_session.as_deref() == Some(session_id.as_str()) {
                    let new_active = {
                        let mgr = state.sessions.lock().unwrap();
                        let list = mgr.list();
                        list.iter()
                            .find(|m| m.id != session_id)
                            .map(|m| m.id.clone())
                    };
                    retarget_watch(&state.watched, v.active_session.as_deref(), new_active.as_deref());
                    v.active_session = new_active;
                    v.invalidate();
                }
            }
            let resp = match kill_result {
                Ok(_) => ServerMsg::SessionKilled {
                    session_id,
                    success: true,
                    error: None,
                },
                Err(e) => ServerMsg::SessionKilled {
                    session_id,
                    success: false,
                    error: Some(e),
                },
            };
            send_server_msg(sender, &resp).await;
        }
        ClientMsg::Detach => return true,
    }
    false
}

/// Launch a new PTY session in the web server's working directory.
fn create_session(
    state: &WebState,
    command: &str,
    args: &[String],
    cols: u16,
    rows: u16,
) -> Result<String, String> {
    let cols = cols.max(2);
    let rows = rows.max(2);
    let default_shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string());
    let cmd = if command.trim().is_empty() {
        default_shell
    } else {
        command.to_string()
    };
    let cmd_args: Vec<&str> = args.iter().map(|s| s.as_str()).collect();

    // Same env as App::create_session: TTTT_PID lets tools inside the
    // session signal this process (e.g. SIGUSR1 live reload).
    let backend = RealPty::spawn_with_cwd_and_env(
        &cmd,
        &cmd_args,
        Some(&state.work_dir),
        cols,
        rows,
        [("TTTT_PID".to_string(), std::process::id().to_string())],
    )
    .map_err(|e| e.to_string())?;
    let mut mgr = state.sessions.lock().unwrap();
    let id = mgr.generate_id();
    let mut session = PtySession::new(id.clone(), AnyPty::Real(backend), cmd, cols, rows);
    session.set_working_dir(state.work_dir.to_string_lossy().into_owned());
    mgr.add_session(session).map_err(|e| e.to_string())?;
    Ok(id)
}

/// Send a ServerMsg back to the client (for create/kill acknowledgements).
async fn send_server_msg(sender: &Arc<WsSender>, msg: &ServerMsg) {
    use futures_util::SinkExt;
    let text = serde_json::to_string(msg).unwrap_or_default();
    let mut s = sender.lock().await;
    let _ = s.send(Message::Text(text)).await;
}

fn status_str(status: &SessionStatus) -> String {
    match status {
        SessionStatus::Running => "running".to_string(),
        SessionStatus::Exited(code) => format!("exited({})", code),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_loopback() {
        assert!(is_loopback("127.0.0.1"));
        assert!(is_loopback("localhost"));
        assert!(is_loopback("::1"));
        assert!(is_loopback("127.8.9.10"));
        assert!(!is_loopback("0.0.0.0"));
        assert!(!is_loopback("192.168.1.1"));
        assert!(!is_loopback("example.com"));
    }

    fn snap_session(hash: u64) -> SessionSnapshot {
        SessionSnapshot {
            dims: (80, 24),
            cursor: (0, 0),
            content_hash: hash,
            content: Arc::new(vec![]),
        }
    }

    fn info(id: &str) -> SessionInfo {
        SessionInfo {
            id: id.to_string(),
            command: "sh".to_string(),
            status: "running".to_string(),
        }
    }

    #[test]
    fn test_snapshot_changed() {
        let mut a = Snapshot::default();
        let mut b = Snapshot::default();
        assert!(!snapshot_changed(&a, &b));
        b.list_hash = 1;
        assert!(snapshot_changed(&a, &b));
        a.list_hash = 1;
        a.sessions.insert("pty-1".into(), snap_session(10));
        b.sessions.insert("pty-1".into(), snap_session(10));
        assert!(!snapshot_changed(&a, &b));
        b.sessions.insert("pty-1".into(), snap_session(11));
        assert!(snapshot_changed(&a, &b));
        b.sessions.insert("pty-1".into(), snap_session(10));
        b.sessions.get_mut("pty-1").unwrap().cursor = (1, 2);
        assert!(snapshot_changed(&a, &b));
    }

    #[test]
    fn test_viewer_updates_repicks_dead_session() {
        let watched: WatchedSessions = Arc::new(Mutex::new(HashMap::new()));
        let mut snap = Snapshot::default();
        snap.list = vec![info("pty-1")];
        snap.list_hash = 42;
        snap.pick_default = Some("pty-1".to_string());
        snap.sessions.insert("pty-1".into(), snap_session(7));

        let mut viewer = WebViewer::new();
        viewer.active_session = Some("pty-9".to_string()); // killed elsewhere
        retarget_watch(&watched, None, Some("pty-9"));

        let msgs = viewer_updates(&snap, &mut viewer, &watched);
        assert_eq!(viewer.active_session.as_deref(), Some("pty-1"));
        // Watch refcount moved from the dead session to the new one.
        let w = watched.lock().unwrap();
        assert!(!w.contains_key("pty-9"));
        assert_eq!(w.get("pty-1"), Some(&1));
        drop(w);
        // Gets the list, the window size, and the screen frame.
        assert_eq!(msgs.len(), 3);
        assert!(matches!(&msgs[0], OutMsg::Json(ServerMsg::SessionList { .. })));
        assert!(matches!(&msgs[1], OutMsg::Json(ServerMsg::WindowSize { .. })));
        assert!(matches!(&msgs[2], OutMsg::Screen { .. }));

        // Second pass with the same snapshot: nothing new to send.
        let msgs = viewer_updates(&snap, &mut viewer, &watched);
        assert!(msgs.is_empty());
    }

    #[test]
    fn test_encode_screen_frame() {
        let msg = OutMsg::Screen {
            cursor: (3, 260),
            content: Arc::new(vec![0x41, 0x42]),
        };
        match encode_out_msg(&msg) {
            Message::Binary(b) => {
                assert_eq!(&b[..4], &[0, 3, 1, 4]); // 3, 260 as u16 BE
                assert_eq!(&b[4..], b"AB");
            }
            _ => panic!("expected binary frame"),
        }
    }

    #[test]
    fn test_retarget_watch_refcounts() {
        let watched: WatchedSessions = Arc::new(Mutex::new(HashMap::new()));
        retarget_watch(&watched, None, Some("a"));
        retarget_watch(&watched, None, Some("a"));
        assert_eq!(watched.lock().unwrap().get("a"), Some(&2));
        retarget_watch(&watched, Some("a"), Some("b"));
        {
            let w = watched.lock().unwrap();
            assert_eq!(w.get("a"), Some(&1));
            assert_eq!(w.get("b"), Some(&1));
        }
        retarget_watch(&watched, Some("a"), None);
        retarget_watch(&watched, Some("b"), None);
        assert!(watched.lock().unwrap().is_empty());
    }

    #[test]
    fn test_status_str() {
        assert_eq!(status_str(&SessionStatus::Running), "running");
        assert_eq!(status_str(&SessionStatus::Exited(0)), "exited(0)");
    }

    #[test]
    fn test_random_token_length() {
        let t = random_token();
        assert!(!t.is_empty());
        // 24 bytes -> 32 base64url chars
        assert_eq!(t.len(), 32);
        let t2 = random_token();
        assert_ne!(t, t2);
    }

    #[test]
    fn test_auth_loopback_no_auth() {
        let cfg = WebConfig {
            host: "127.0.0.1".into(),
            port: 8080,
            secure: false,
            tls_cert: None,
            tls_key: None,
            htpasswd: None,
            token: None,
            work_dir: std::path::PathBuf::from("."),
        };
        let (auth, gen) = cfg.resolved_auth().unwrap();
        assert!(!auth.required());
        assert!(gen.is_none());
    }

    #[test]
    fn test_ensure_token_non_loopback() {
        let mut config = crate::config::Config::default();
        config.http_port = Some(8080);
        config.http_host = Some("0.0.0.0".to_string());
        ensure_token(&mut config);
        assert!(config.token.is_some());
        // Idempotent: an existing token is kept.
        let tok = config.token.clone();
        ensure_token(&mut config);
        assert_eq!(config.token, tok);
    }

    #[test]
    fn test_ensure_token_loopback_or_disabled() {
        // Loopback: no token needed.
        let mut config = crate::config::Config::default();
        config.http_port = Some(8080);
        ensure_token(&mut config);
        assert!(config.token.is_none());
        // Web disabled: no token even for non-loopback host.
        let mut config = crate::config::Config::default();
        config.http_host = Some("0.0.0.0".to_string());
        ensure_token(&mut config);
        assert!(config.token.is_none());
    }

    #[test]
    fn test_tls_cert_without_key_rejected() {
        let cfg = WebConfig {
            host: "127.0.0.1".into(),
            port: 0,
            secure: true,
            tls_cert: Some(std::path::PathBuf::from("/nonexistent/cert.pem")),
            tls_key: None,
            htpasswd: None,
            token: None,
            work_dir: std::path::PathBuf::from("."),
        };
        let sessions: Arc<Mutex<SessionManager<AnyPty>>> =
            Arc::new(Mutex::new(SessionManager::new()));
        let status: WebStatus = Arc::new(Mutex::new(None));
        assert!(start_web_server(cfg, sessions, status).is_err());
    }

    #[test]
    fn test_auth_non_loopback_generates_token() {
        let cfg = WebConfig {
            host: "0.0.0.0".into(),
            port: 8080,
            secure: false,
            tls_cert: None,
            tls_key: None,
            htpasswd: None,
            token: None,
            work_dir: std::path::PathBuf::from("."),
        };
        let (auth, gen) = cfg.resolved_auth().unwrap();
        assert!(auth.required());
        assert!(gen.is_some());
    }
}
