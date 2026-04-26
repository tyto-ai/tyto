use anyhow::Result;
use clap::{Parser, Subcommand};
use tyto::{config::Config, inject, install, remote, request, serve, status};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "tyto", version, about = "Persistent memory and code intelligence for AI agents")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Start the MCP server over stdio
    Serve {
        #[arg(long, help = "Path to .tyto.toml (default: auto-discover)")]
        config: Option<PathBuf>,
    },
    /// Inject memory context into agent hooks (short-lived, always exits 0)
    Inject {
        #[arg(long, default_value = "prompt", help = "Injection type: prompt | session | stop | compact")]
        r#type: String,
        #[arg(long, help = "Explicit query string (prompt type only)")]
        query: Option<String>,
        #[arg(long, default_value_t = 5)]
        limit: usize,
        #[arg(long, default_value_t = 9500, help = "Max output bytes")]
        budget: usize,
        #[arg(long, default_value_t = 400, help = "Socket call timeout in milliseconds (0 = no timeout)")]
        socket_timeout: u64,
    },
    /// Manage remote database sync
    Remote {
        #[command(subcommand)]
        subcommand: RemoteCommand,
    },
    /// Install tyto into Claude Code (adds MCP server + hooks to settings.json)
    Install {
        #[arg(long, help = "Preview changes without writing anything")]
        dry_run: bool,
    },
    /// Call an MCP tool on the running tyto serve instance via the local socket
    Request {
        #[arg(help = "Tool name to call")]
        tool: String,
        #[arg(help = "Tool arguments as a JSON object string (optional)")]
        args: Option<String>,
    },
    /// Show current configuration and database status
    Status,
}

#[derive(Subcommand)]
enum RemoteCommand {
    /// Migrate local database to a remote backend and enable sync
    Enable {
        #[arg(long, help = "Remote database URL")]
        url: Option<String>,
        #[arg(long, help = "Auth token")]
        token: Option<String>,
        #[arg(long, help = "Overwrite remote database if it already has data")]
        force: bool,
    },
    /// Seed an empty remote database from the local backup
    Sync {
        #[arg(long, help = "Overwrite remote database even if it already has data")]
        force: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::Serve { config: config_path } => {
            // init_tracing_to_file() is called inside serve::run() after log::init(),
            // so tracing output lands in the log file rather than discarded stderr.
            let cwd = std::env::current_dir()?;
            let start = config_path
                .as_deref()
                .and_then(|p| p.parent())
                .unwrap_or(&cwd);
            let config = Config::load(start)?;
            serve::run(config).await?;
        }
        Command::Inject { r#type, query, limit, budget, socket_timeout } => {
            tyto::log::init_tracing();
            if let Err(e) = inject::run(&r#type, query, limit, budget, socket_timeout).await {
                eprintln!("tyto inject error: {e}");
            }
        }
        Command::Remote { subcommand } => {
            let cwd = std::env::current_dir()?;
            let config = Config::load(&cwd)?;
            match subcommand {
                RemoteCommand::Enable { url, token, force } => {
                    remote::enable(&config, url, token, force).await?;
                }
                RemoteCommand::Sync { force } => {
                    let msg = remote::sync(&config, force).await?;
                    println!("{msg}");
                }
            }
        }
        Command::Install { dry_run } => {
            let result = install::run(dry_run)?;
            let prefix = if dry_run { "[dry-run] " } else { "" };
            if result.mcp_added {
                println!("{prefix}Added MCP server 'tyto' to {}", result.settings_path.display());
            } else {
                println!("MCP server 'tyto' already configured - skipped");
            }
            if result.session_hook_added {
                println!("{prefix}Added SessionStart hook");
            } else {
                println!("SessionStart hook already configured - skipped");
            }
            if result.prompt_hook_added {
                println!("{prefix}Added UserPromptSubmit hook");
            } else {
                println!("UserPromptSubmit hook already configured - skipped");
            }
            if result.stop_hook_added {
                println!("{prefix}Added Stop hook");
            } else {
                println!("Stop hook already configured - skipped");
            }
            if result.compact_hook_added {
                println!("{prefix}Added PostCompact hook");
            } else {
                println!("PostCompact hook already configured - skipped");
            }
            if !result.mcp_added && !result.session_hook_added && !result.prompt_hook_added
                && !result.stop_hook_added && !result.compact_hook_added {
                println!("Nothing to do - tyto is already fully configured.");
            } else if !dry_run {
                println!("\nDone. Restart Claude Code for changes to take effect.");
            }
        }
        Command::Request { tool, args } => {
            tyto::log::init_tracing();
            let cwd = std::env::current_dir()?;
            let config = Config::load(&cwd)?;
            if let Err(e) = request::run(&config, &tool, args.as_deref()).await {
                eprintln!("tyto request error: {e}");
                std::process::exit(1);
            }
        }
        Command::Status => {
            let cwd = std::env::current_dir()?;
            let config = Config::load(&cwd)?;
            status::run(&config).await?;
        }
    }

    Ok(())
}
