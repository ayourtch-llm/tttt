mod app;
mod attach;
mod config;
mod reload;

use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "tttt", about = "Autonomous multiagent terminal harness")]
struct Cli {
    /// Path to config file
    #[arg(short, long)]
    config: Option<PathBuf>,

    /// Command to run as root process (overrides config)
    #[arg(short = 'e', long)]
    command: Option<String>,

    /// Working directory
    #[arg(short, long)]
    workdir: Option<PathBuf>,

    /// Show full initial screen dump on exit (default: first 10 lines)
    #[arg(long)]
    full_dump: bool,

    /// Arguments to pass to the root command (after --)
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    root_args: Vec<String>,

    #[command(subcommand)]
    subcommand: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Run as an MCP proxy server (for integration with AI agents)
    /// Always connects to a running tttt instance via Unix socket.
    McpServer {
        /// Working directory for PTY sessions (when running standalone, not in proxy mode)
        #[arg(short, long)]
        workdir: Option<PathBuf>,

        /// Connect to a running tttt instance via Unix socket (proxy mode).
        /// When set, tool calls are forwarded to the TUI's shared SessionManager.
        #[arg(short, long)]
        connect: Option<String>,
    },

    /// Attach to a running tttt instance as a viewer
    Attach {
        /// Socket path (default: auto-detect from /tmp/tttt-*.sock)
        #[arg(short, long)]
        socket: Option<String>,
    },
}

fn main() {
    // Check for restore mode FIRST — before parsing CLI args
    if let Ok(restore_file) = std::env::var(reload::RESTORE_ENV_VAR) {
        std::env::remove_var(reload::RESTORE_ENV_VAR);
        run_restored(&restore_file);
        return;
    }

    let cli = Cli::parse();

    match cli.subcommand {
        Some(Commands::McpServer { workdir, connect }) => {
            if let Some(socket_path) = connect {
                run_proxy_mcp_server(&socket_path);
            } else {
                run_standalone_mcp_server(workdir);
            }
        }
        Some(Commands::Attach { socket }) => {
            run_attach(socket);
        }
        None => {
            run_tui(cli);
        }
    }
}

fn run_tui(cli: Cli) {
    let mut config = match cli.config {
        Some(path) => config::Config::load(&path).unwrap_or_else(|e| {
            eprintln!("Warning: failed to load config {}: {}", path.display(), e);
            config::Config::default()
        }),
        None => config::Config::load_default(),
    };

    config.apply_env_overrides();

    if let Some(cmd) = cli.command {
        // Shell-style parsing: supports quotes and escapes
        // e.g., -e 'claude --prompt "hello world"' → ["claude", "--prompt", "hello world"]
        match shell_words::split(&cmd) {
            Ok(parts) if !parts.is_empty() => {
                config.root_command = parts[0].clone();
                config.root_args = parts[1..].to_vec();
            }
            _ => {
                config.root_command = cmd;
            }
        }
    }
    // Append any trailing arguments (after --)
    if !cli.root_args.is_empty() {
        config.root_args.extend(cli.root_args);
    }
    if let Some(dir) = cli.workdir {
        config.work_dir = dir;
    }

    let mut app = app::App::new(config);

    if let Err(e) = app.init_loggers() {
        eprintln!("Warning: failed to initialize loggers: {}", e);
    }

    // Start MCP proxy socket listener
    if let Err(e) = app.start_mcp_listener() {
        eprintln!("Warning: failed to start MCP listener: {}", e);
    }

    // Start viewer socket listener
    if let Err(e) = app.start_viewer_listener() {
        eprintln!("Warning: failed to start viewer listener: {}", e);
    }

    if let Err(e) = app.launch_root() {
        eprintln!("Failed to launch root session: {}", e);
        std::process::exit(1);
    }

    if let Err(e) = app.run() {
        eprintln!("\nError: {}", e);
        std::process::exit(1);
    }

    // If reload was requested, perform execv (does not return on success)
    if app.reload_requested {
        if let Err(e) = app.execute_reload() {
            eprintln!("Reload failed: {}", e);
            eprintln!("Continuing with current process...");
            // Fall through to normal exit
        }
    }

    // Show root session's last screen on exit for diagnostics
    if let Some((screen, status)) = &app.last_root_screen {
        let screen_trimmed = screen.trim();
        if !screen_trimmed.is_empty() {
            let exit_info = match status {
                tttt_pty::SessionStatus::Exited(code) => format!("exited with code {}", code),
                tttt_pty::SessionStatus::Running => "still running".to_string(),
            };
            eprintln!("\n--- Root session {} ---", exit_info);
            let lines: Vec<&str> = screen_trimmed.lines().collect();
            if cli.full_dump || lines.len() <= 10 {
                eprintln!("{}", screen_trimmed);
            } else {
                for line in &lines[..10] {
                    eprintln!("{}", line);
                }
                eprintln!("... ({} more lines, use --full-dump to show all)", lines.len() - 10);
            }
            eprintln!("---");
        }
    }

    // Clean up sockets
    if let Some(ref path) = app.socket_path {
        let _ = std::fs::remove_file(path);
    }
    if let Some(ref path) = app.mcp_socket_path {
        let _ = std::fs::remove_file(path);
    }
}

/// Restore from a saved state file after execv().
fn run_restored(restore_file: &str) {
    let state = match reload::SavedState::read_from_file(restore_file) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Failed to read restore state: {}", e);
            eprintln!("Starting fresh instead.");
            // Parse CLI and start normally
            let cli = Cli::parse();
            run_tui(cli);
            return;
        }
    };

    let config = state.config.clone();

    // Check if root session should be restarted:
    // - Explicitly requested via SIGUSR2 (restart_root flag in saved state)
    // - Only if root command has --resume so it can recover its conversation
    let restart_root = state.restart_root
        && config.root_args.iter().any(|a| a == "--resume");

    let mut app = app::App::new(config);

    if let Err(e) = app.init_loggers() {
        eprintln!("Warning: failed to initialize loggers: {}", e);
    }

    // Restore sessions from inherited FDs.
    // If root has --resume, skip the root session (pty-1) so we can relaunch it fresh
    // with updated MCP tool definitions.
    if let Err(e) = app.restore_sessions_filtered(&state, |saved| {
        if restart_root && state.session_order.first().map(|s| s.as_str()) == Some(&saved.id) {
            // Kill the old root process so it doesn't linger
            if let Some(pid) = saved.child_pid {
                unsafe { libc::kill(pid, libc::SIGTERM); }
            }
            false // skip restoring this session
        } else {
            true // restore normally
        }
    }) {
        eprintln!("Warning: failed to restore some sessions: {}", e);
    }

    // Re-create socket listeners
    if let Err(e) = app.start_mcp_listener() {
        eprintln!("Warning: failed to start MCP listener: {}", e);
    }
    if let Err(e) = app.start_viewer_listener() {
        eprintln!("Warning: failed to start viewer listener: {}", e);
    }

    // If root was skipped, remove it from session_order and relaunch fresh
    // (gets new MCP config + tool discovery)
    if restart_root {
        if let Some(root_id) = state.session_order.first() {
            app.remove_from_session_order(root_id);
        }
        match app.launch_root() {
            Ok(new_root_id) => {
                // Auto-inject "Continue" when Claude shows its prompt,
                // so it resumes without waiting for human input.
                app.setup_auto_continue(&new_root_id);
            }
            Err(e) => {
                eprintln!("Failed to relaunch root session: {}", e);
                std::process::exit(1);
            }
        }
    }

    // For lightweight reloads (SIGUSR1), the root session is still alive but Claude
    // may go quiet. Queue a "Continue" injection into the root session so it gets
    // kicked automatically after the event loop starts.
    if !restart_root {
        if let Some(root_id) = state.session_order.first() {
            app.queue_injection(root_id, "Continue from where you left off.");
        }
    }

    // Restore cron jobs
    app.restore_cron_jobs(&state.cron_jobs);

    // Restore notification watchers
    app.restore_watchers(&state.watchers);

    // Restore scratchpad
    app.restore_scratchpad(&state.scratchpad);

    if let Err(e) = app.run() {
        eprintln!("\nError: {}", e);
        std::process::exit(1);
    }

    // Handle reload request from restored instance
    if app.reload_requested {
        if let Err(e) = app.execute_reload() {
            eprintln!("Reload failed: {}", e);
        }
    }

    // Clean up sockets
    if let Some(ref path) = app.socket_path {
        let _ = std::fs::remove_file(path);
    }
    if let Some(ref path) = app.mcp_socket_path {
        let _ = std::fs::remove_file(path);
    }
}

fn run_attach(socket: Option<String>) {
    let socket_path = match socket {
        Some(s) => s,
        None => {
            // Auto-detect: find /tmp/tttt-[0-9]+.sock
            match find_tttt_socket() {
                SocketResult::Found(path) => path,
                SocketResult::None => {
                    eprintln!("No running tttt instance found. Specify socket with -s.");
                    std::process::exit(1);
                }
                SocketResult::Multiple(sockets) => {
                    eprintln!("Multiple running tttt instances found:");
                    for socket in sockets {
                        eprintln!("  {}", socket);
                    }
                    eprintln!("Please use -s <socket path> to connect.");
                    std::process::exit(1);
                }
            }
        }
    };

    if let Err(e) = attach::run_attach(&socket_path) {
        eprintln!("Attach error: {}", e);
        std::process::exit(1);
    }
}

enum SocketResult {
    None,
    Found(String),
    Multiple(Vec<String>),
}

fn find_tttt_socket() -> SocketResult {
    let read_dir = match std::fs::read_dir("/tmp") {
        Ok(d) => d,
        Err(_) => return SocketResult::None,
    };
    
    let mut sockets: Vec<String> = Vec::new();
    
    for entry in read_dir.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        // Match pattern: tttt-[digits].sock
        if name.starts_with("tttt-") && name.ends_with(".sock") {
            // Extract the numeric part to validate the pattern
            let stem = name.strip_prefix("tttt-").unwrap();
            let stem = stem.strip_suffix(".sock").unwrap();
            if stem.parse::<u64>().is_ok() {
                let path = entry.path().to_string_lossy().to_string();
                // Check if socket is alive by trying to connect
                if std::os::unix::net::UnixStream::connect(&path).is_ok() {
                    sockets.push(path);
                }
            }
        }
    }
    
    match sockets.len() {
        0 => SocketResult::None,
        1 => SocketResult::Found(sockets.pop().unwrap()),
        _ => SocketResult::Multiple(sockets),
    }
}

/// Proxy MCP server mode — forwards JSON-RPC between Claude (stdio) and tttt TUI (socket).
fn run_proxy_mcp_server(socket_path: &str) {
    use tttt_mcp::proxy::run_proxy;

    let stdin = std::io::stdin().lock();
    let stdout = std::io::stdout().lock();
    let reader = std::io::BufReader::new(stdin);

    if let Err(e) = run_proxy(reader, stdout, socket_path) {
        eprintln!("MCP proxy error: {}", e);
        std::process::exit(1);
    }
}

/// Standalone MCP server mode — runs on stdin/stdout with its own session manager.
fn run_standalone_mcp_server(workdir: Option<PathBuf>) {
    use tttt_mcp::{
        CompositeToolHandler, McpServer, PtyToolHandler, SchedulerToolHandler,
        ScratchpadToolHandler,
    };
    use tttt_pty::{RealPty, SessionManager};
    use tttt_scheduler::Scheduler;

    let work_dir = workdir.unwrap_or_else(|| {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
    });

    let manager: SessionManager<RealPty> = SessionManager::new();
    let pty_handler = PtyToolHandler::new_owned(manager, work_dir);

    let scheduler = Scheduler::new();
    let scheduler_handler = SchedulerToolHandler::new_owned(scheduler);

    let scratchpad_handler = ScratchpadToolHandler::new();

    let mut composite = CompositeToolHandler::new();
    composite.add_handler(Box::new(pty_handler));
    composite.add_handler(Box::new(scheduler_handler));
    composite.add_handler(Box::new(scratchpad_handler));

    let stdin = std::io::stdin().lock();
    let stdout = std::io::stdout().lock();
    let reader = std::io::BufReader::new(stdin);

    let mut server = McpServer::new(reader, stdout, composite);

    if let Err(e) = server.run() {
        eprintln!("MCP server error: {}", e);
        std::process::exit(1);
    }
}
