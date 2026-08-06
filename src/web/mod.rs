//! Web-based viewer for tttt (`--http-host` / `--http-port` / `--secure`).
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
fn is_loopback(host: &str) -> bool {
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

/// Shared state for axum handlers.
#[derive(Clone)]
struct WebState {
    sessions: Arc<Mutex<SessionManager<AnyPty>>>,
    auth: Arc<auth::Auth>,
    work_dir: PathBuf,
}

/// Shared WebSocket sender (split sink) used by both the push loop and the
/// read loop (for responses to CreateSession/KillSession).
type WsSender = tokio::sync::Mutex<futures_util::stream::SplitSink<WebSocket, Message>>;

/// Start the web server in a background thread. Returns the URL to open.
pub fn start_web_server(
    cfg: WebConfig,
    sessions: Arc<Mutex<SessionManager<AnyPty>>>,
) -> Result<String, Box<dyn std::error::Error>> {
    if cfg.tls_cert.is_some() != cfg.tls_key.is_some() {
        return Err("--tls-cert and --tls-key must be supplied together".into());
    }
    let (auth, generated_token) = cfg.resolved_auth()?;

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

    let state = WebState {
        sessions,
        auth: Arc::new(auth),
        work_dir: cfg.work_dir.clone(),
    };

    let cfg = Arc::new(cfg);
    std::thread::Builder::new()
        .name("tttt-web".to_string())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .worker_threads(2)
                .build()
                .expect("failed to build web runtime");
            rt.block_on(serve(cfg, state));
        })?;

    Ok(url)
}

async fn serve(cfg: Arc<WebConfig>, state: WebState) {
    let app = Router::new()
        .route("/", get(index))
        .route("/app.js", get(serve_app_js))
        .route("/xterm.js", get(serve_xterm_js))
        .route("/xterm.css", get(serve_xterm_css))
        .route("/api/auth", get(auth_info))
        .route("/ws", get(ws_handler))
        .with_state(state);

    // Resolve via ToSocketAddrs so hostnames and bare IPv6 work; refuse to
    // start rather than silently binding a different address than requested.
    use std::net::ToSocketAddrs;
    let addr: SocketAddr = match (cfg.host.as_str(), cfg.port).to_socket_addrs() {
        Ok(mut addrs) => match addrs.next() {
            Some(a) => a,
            None => {
                eprintln!("[tttt web] no address found for {}:{}", cfg.host, cfg.port);
                return;
            }
        },
        Err(e) => {
            eprintln!(
                "[tttt web] cannot resolve bind address {}:{}: {}",
                cfg.host, cfg.port, e
            );
            return;
        }
    };

    let result = if cfg.secure {
        let server_config = build_tls_config(&cfg);
        match server_config {
            Ok(sc) => {
                let tls = axum_server::tls_rustls::RustlsConfig::from_config(Arc::new(sc));
                axum_server::bind_rustls(addr, tls)
                    .serve(app.into_make_service())
                    .await
            }
            Err(e) => {
                eprintln!("[tttt web] TLS setup failed: {}", e);
                return;
            }
        }
    } else {
        axum_server::bind(addr).serve(app.into_make_service()).await
    };

    if let Err(e) = result {
        eprintln!("[tttt web] server error: {}", e);
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

fn fnv1a(data: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for &b in data {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
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
    viewer.lock().await.active_session = active;

    // Push loop: periodically send screen + session-list updates.
    let push_sender = Arc::clone(&sender);
    let push_viewer = Arc::clone(&viewer);
    let push_state = state.clone();
    let push_task = tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_millis(50));
        loop {
            interval.tick().await;
            let msgs = {
                let mut v = push_viewer.lock().await;
                let mut out = Vec::new();
                build_updates(&push_state.sessions, &mut v, &mut out);
                out
            };
            if msgs.is_empty() {
                continue;
            }
            let mut s = push_sender.lock().await;
            for msg in msgs {
                let text = serde_json::to_string(&msg).unwrap_or_default();
                if s.send(Message::Text(text)).await.is_err() {
                    return;
                }
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

/// Build any pending screen/session-list updates for a viewer.
fn build_updates(
    sessions: &Arc<Mutex<SessionManager<AnyPty>>>,
    viewer: &mut WebViewer,
    out: &mut Vec<ServerMsg>,
) {
    let mgr = sessions.lock().unwrap();

    // (Re)pick an active session if the current one is gone (killed by
    // another viewer or the TUI) or was never set (connected before any
    // session existed). Without this the viewer freezes on a dead session.
    let current_ok = viewer
        .active_session
        .as_ref()
        .map(|sid| mgr.exists(sid))
        .unwrap_or(false);
    if !current_ok {
        let list = mgr.list();
        let new_active = list
            .iter()
            .find(|m| m.root)
            .or_else(|| list.first())
            .map(|m| m.id.clone());
        if new_active != viewer.active_session {
            viewer.active_session = new_active;
            viewer.invalidate();
            // Force a SessionList push so the sidebar highlight follows.
            viewer.last_list_hash = 0;
        }
    }

    // Session list (only when changed)
    let list: Vec<SessionInfo> = mgr
        .list()
        .iter()
        .map(|m| SessionInfo {
            id: m.id.clone(),
            command: m.command.clone(),
            status: status_str(&m.status),
        })
        .collect();
    let list_json = serde_json::to_vec(&list).unwrap_or_default();
    let list_hash = fnv1a(&list_json);
    if list_hash != viewer.last_list_hash {
        viewer.last_list_hash = list_hash;
        out.push(ServerMsg::SessionList {
            sessions: list,
            active_id: viewer.active_session.clone(),
        });
    }

    // Screen update for the active session
    let Some(sid) = viewer.active_session.clone() else {
        return;
    };
    let Ok(session) = mgr.get(&sid) else {
        return;
    };
    // Keep the browser terminal at the PTY's dimensions. The TUI owns the
    // PTY size (min-across-viewers arbitration); the web client follows.
    let dims = session.screen().size();
    if dims != viewer.last_window_size {
        viewer.last_window_size = dims;
        out.push(ServerMsg::WindowSize {
            cols: dims.0,
            rows: dims.1,
        });
    }
    // Send the formatted contents PLUS the terminal input modes (bracketed
    // paste, application keypad/cursor, mouse). xterm.js uses the bracketed
    // paste mode flag to wrap pastes in \x1b[200~...\x1b[201~ so multi-line
    // pastes aren't executed line-by-line by the shell.
    let mut content = session.get_screen_formatted();
    content.extend_from_slice(&session.screen().screen().input_mode_formatted());
    let hash = fnv1a(&content);
    let cursor = session.cursor_position();
    if hash == viewer.last_content_hash && cursor == viewer.last_cursor {
        return;
    }
    viewer.last_content_hash = hash;
    viewer.last_cursor = cursor;
    out.push(ServerMsg::ScreenUpdate {
        screen_data: content,
        cursor_row: cursor.0,
        cursor_col: cursor.1,
    });
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
            let mut v = viewer.lock().await;
            let exists = state.sessions.lock().unwrap().exists(&session_id);
            if exists {
                v.active_session = Some(session_id);
                v.invalidate();
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
            {
                let mut v = viewer.lock().await;
                if v.active_session.as_deref() == Some(session_id.as_str()) {
                    let mgr = state.sessions.lock().unwrap();
                    let list = mgr.list();
                    v.active_session = list
                        .iter()
                        .find(|m| m.id != session_id)
                        .map(|m| m.id.clone());
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

    #[test]
    fn test_fnv1a_stability() {
        assert_eq!(fnv1a(b"abc"), fnv1a(b"abc"));
        assert_ne!(fnv1a(b"abc"), fnv1a(b"abd"));
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
        assert!(start_web_server(cfg, sessions).is_err());
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
