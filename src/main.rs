mod app;
mod attach;
mod config;

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
            // Auto-detect: find /tmp/tttt-*.sock
            match find_tttt_socket() {
                Some(path) => path,
                None => {
                    eprintln!("No running tttt instance found. Specify socket with -s.");
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

fn find_tttt_socket() -> Option<String> {
    let read_dir = std::fs::read_dir("/tmp").ok()?;
    for entry in read_dir.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with("tttt-") && name.ends_with(".sock") {
            let path = entry.path().to_string_lossy().to_string();
            // Check if socket is alive by trying to connect
            if std::os::unix::net::UnixStream::connect(&path).is_ok() {
                return Some(path);
            }
        }
    }
    None
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

    let mut composite = CompositeToolHandler::new();
    composite.add_handler(Box::new(pty_handler));
    composite.add_handler(Box::new(scheduler_handler));

    let stdin = std::io::stdin().lock();
    let stdout = std::io::stdout().lock();
    let reader = std::io::BufReader::new(stdin);

    let mut server = McpServer::new(reader, stdout, composite);

    if let Err(e) = server.run() {
        eprintln!("MCP server error: {}", e);
        std::process::exit(1);
    }
}
