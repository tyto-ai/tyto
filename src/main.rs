use anyhow::Result;
use clap::{Parser, Subcommand};
use memso::{config::Config, inject, install, remote, serve, status};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "memso", version, about = "Persistent memory for AI agents")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Start the MCP server over stdio
    Serve {
        #[arg(long, help = "Path to .memso.toml (default: auto-discover)")]
        config: Option<PathBuf>,
    },
    /// Inject memory context into agent hooks (short-lived, always exits 0)
    Inject {
        #[arg(long, default_value = "prompt", help = "Injection type: prompt | session | stop | compact")]
        r#type: String,
        #[arg(long, help = "Override project ID")]
        project: Option<String>,
        #[arg(long, help = "Explicit query string (prompt type only)")]
        query: Option<String>,
        #[arg(long, default_value_t = 5)]
        limit: usize,
        #[arg(long, default_value_t = 9500, help = "Max output bytes")]
        budget: usize,
    },
    /// Manage remote database sync
    Remote {
        #[command(subcommand)]
        subcommand: RemoteCommand,
    },
    /// Install memso into Claude Code (adds MCP server + hooks to settings.json)
    Install {
        #[arg(long, help = "Preview changes without writing anything")]
        dry_run: bool,
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
            let cwd = std::env::current_dir()?;
            let start = config_path
                .as_deref()
                .and_then(|p| p.parent())
                .unwrap_or(&cwd);
            let config = Config::load(start)?;
            serve::run(config).await?;
        }
        Command::Inject { r#type, project, query, limit, budget } => {
            if let Err(e) = inject::run(&r#type, project, query, limit, budget).await {
                eprintln!("memso inject error: {e}");
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
                println!("{prefix}Added MCP server 'memso' to {}", result.settings_path.display());
            } else {
                println!("MCP server 'memso' already configured - skipped");
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
                println!("Nothing to do - memso is already fully configured.");
            } else if !dry_run {
                println!("\nDone. Restart Claude Code for changes to take effect.");
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
