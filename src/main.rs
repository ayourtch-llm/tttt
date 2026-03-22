mod app;
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
}

fn main() {
    let cli = Cli::parse();

    match cli.subcommand {
        Some(Commands::McpServer { workdir }) => {
            run_standalone_mcp_server(workdir);
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

    let work_dir = config.work_dir.clone();
    let mut app = app::App::new(config);

    if let Err(e) = app.init_loggers() {
        eprintln!("Warning: failed to initialize loggers: {}", e);
    }

    // Start the MCP server thread with the shared session manager.
    // The MCP server listens on a Unix socket that the root agent can connect to.
    // For now, we also support the `tttt mcp-server` standalone mode for stdio.
    let shared_sessions = app.shared_sessions();
    let _mcp_thread = std::thread::spawn(move || {
        // The MCP server thread will be activated when we implement
        // the pipe-based connection from the root agent.
        // For now, just hold the shared reference.
        let _sessions = shared_sessions;
    });

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
}

/// Standalone MCP server mode — runs on stdin/stdout with its own session manager.
fn run_standalone_mcp_server(workdir: Option<PathBuf>) {
    use tttt_mcp::{McpServer, PtyToolHandler};
    use tttt_pty::{RealPty, SessionManager};

    let work_dir = workdir.unwrap_or_else(|| {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
    });

    let manager: SessionManager<RealPty> = SessionManager::new();
    let handler = PtyToolHandler::new_owned(manager, work_dir);

    let stdin = std::io::stdin().lock();
    let stdout = std::io::stdout().lock();
    let reader = std::io::BufReader::new(stdin);

    let mut server = McpServer::new(reader, stdout, handler);

    if let Err(e) = server.run() {
        eprintln!("MCP server error: {}", e);
        std::process::exit(1);
    }
}
