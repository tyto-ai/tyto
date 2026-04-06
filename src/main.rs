use anyhow::Result;
use clap::{Parser, Subcommand};
use memso::{capture, config::Config, inject, install, serve, status};
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
        #[arg(long, default_value_t = 8000, help = "Max output characters")]
        budget: usize,
    },
    /// Capture a PostToolUse hook event for later review at session start
    Capture {
        #[arg(long, help = "Override project ID")]
        project: Option<String>,
    },
    /// Migrate local database to Turso Cloud
    Migrate {
        #[arg(long, help = "Turso database URL to migrate to")]
        to_turso: String,
        #[arg(long, help = "Turso auth token")]
        token: String,
    },
    /// Install memso into Claude Code (adds MCP server + hooks to settings.json)
    Install {
        #[arg(long, help = "Preview changes without writing anything")]
        dry_run: bool,
    },
    /// Show current configuration and database status
    Status,
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
        Command::Capture { project } => {
            if let Err(e) = capture::run(project).await {
                eprintln!("memso capture error: {e}");
            }
        }
        Command::Migrate { to_turso, token } => {
            eprintln!("migrate not yet implemented");
            let _ = (to_turso, token);
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
            if result.capture_hook_added {
                println!("{prefix}Added PostToolUse hook");
            } else {
                println!("PostToolUse hook already configured - skipped");
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
                && !result.capture_hook_added && !result.stop_hook_added && !result.compact_hook_added {
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

