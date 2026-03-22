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

    #[command(subcommand)]
    subcommand: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Run as a standalone MCP server (for integration with AI agents)
    McpServer {
        /// Working directory for sessions
        #[arg(short, long)]
        workdir: Option<PathBuf>,
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
        Some(Commands::McpServer { workdir }) => {
            run_standalone_mcp_server(workdir);
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
        config.root_command = cmd;
    }
    if let Some(dir) = cli.workdir {
        config.work_dir = dir;
    }

    let mut app = app::App::new(config);

    if let Err(e) = app.init_loggers() {
        eprintln!("Warning: failed to initialize loggers: {}", e);
    }

    // Start viewer socket listener
    match app.start_viewer_listener() {
        Ok(path) => {
            eprintln!("Viewer socket: {}", path);
            eprintln!("Connect with: tttt attach -s {}", path);
        }
        Err(e) => {
            eprintln!("Warning: failed to start viewer listener: {}", e);
        }
    }

    match app.launch_root() {
        Ok(id) => {
            eprintln!("Launched root session: {}", id);
        }
        Err(e) => {
            eprintln!("Failed to launch root session: {}", e);
            std::process::exit(1);
        }
    }

    if let Err(e) = app.run() {
        eprintln!("\nError: {}", e);
        std::process::exit(1);
    }

    // Clean up socket
    if let Some(ref path) = app.socket_path {
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
