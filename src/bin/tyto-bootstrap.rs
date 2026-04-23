// Universal tyto entrypoint: exec tyto with any args if installed and current.
// MCP server mode (no args / "serve"): serve a loading JSON-RPC stub while
// downloading tyto in the background.
// Hook mode (inject / stop / compact): print an unavailable message and exit 0
// immediately - the MCP server instance handles the download.
//
// Bundled in agents/claude/bin/ and invoked by agents/claude/scripts/tyto.cmd.

use std::io::{BufRead, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

const VERSION: &str = env!("CARGO_PKG_VERSION");
const REPO: &str = "tyto-ai/tyto";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    // Dev override: exec directly, pass all args through.
    if let Some(p) = std::env::var("TYTO_BINARY_OVERRIDE").ok().filter(|p| !p.is_empty()) {
        exec_tyto(Path::new(&p), &call_args(&args));
    }

    let bin = resolve_binary_path();
    if binary_is_current(&bin) {
        exec_tyto(&bin, &call_args(&args));
    } else if is_serve(&args) {
        serve_loading(bin);
    } else {
        // Hook invocation (inject / stop / compact): exit gracefully.
        // The MCP server stub is handling the download in the background.
        println!(
            "[tyto] Memory tools unavailable: tyto is downloading on first install. \
             Restart your session once the download completes."
        );
    }
}

fn is_serve(args: &[String]) -> bool {
    args.is_empty() || args.first().map(|a| a == "serve").unwrap_or(false)
}

// Effective args to pass to tyto; defaults to ["serve"] when invoked bare.
fn call_args(args: &[String]) -> Vec<String> {
    if args.is_empty() {
        vec!["serve".to_string()]
    } else {
        args.to_vec()
    }
}

// ---------------------------------------------------------------------------
// Binary path resolution
// ---------------------------------------------------------------------------

fn resolve_binary_path() -> PathBuf {
    if let Ok(data) = std::env::var("TYTO_PLUGIN_DATA") {
        return PathBuf::from(data).join(exe("tyto"));
    }
    if let Ok(data) = std::env::var("CLAUDE_PLUGIN_DATA") {
        return PathBuf::from(data).join(exe("tyto"));
    }
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("tyto")
        .join(exe("tyto"))
}

fn exe(name: &str) -> String {
    if cfg!(target_os = "windows") {
        format!("{}.exe", name)
    } else {
        name.to_string()
    }
}

fn version_file(bin: &Path) -> PathBuf {
    bin.parent()
        .unwrap_or_else(|| Path::new("."))
        .join("tyto.version")
}

fn binary_is_current(bin: &Path) -> bool {
    if !bin.exists() {
        return false;
    }
    std::fs::read_to_string(version_file(bin))
        .map(|v| v.trim() == VERSION)
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Fast path: exec tyto (transparent pass-through)
// ---------------------------------------------------------------------------

fn exec_tyto(bin: &Path, args: &[String]) -> ! {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = std::process::Command::new(bin).args(args).exec();
        eprintln!("tyto stub: exec failed: {}", err);
        std::process::exit(1);
    }
    #[cfg(not(unix))]
    {
        match std::process::Command::new(bin).args(args).status() {
            Ok(s) => std::process::exit(s.code().unwrap_or(1)),
            Err(e) => {
                eprintln!("tyto stub: failed to start: {}", e);
                std::process::exit(1);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Slow path: loading MCP stub + background download
// ---------------------------------------------------------------------------

fn serve_loading(bin: PathBuf) {
    let ready = Arc::new(AtomicBool::new(false));
    let ready2 = Arc::clone(&ready);

    std::thread::spawn(move || {
        if let Err(e) = download_with_lock(&bin) {
            eprintln!("tyto stub: {}", e);
        } else {
            ready2.store(true, Ordering::Release);
            eprintln!("tyto stub: download complete - restart your session to activate tyto");
        }
    });

    run_mcp_server(ready);
}

fn download_with_lock(bin: &Path) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(parent) = bin.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let lock_path = bin.parent().unwrap_or(Path::new(".")).join("tyto.download.lock");
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)?;

    if lock_file.try_lock().is_err() {
        // Another stub instance is already downloading; poll until it finishes.
        eprintln!("tyto stub: download in progress in another instance, waiting...");
        for _ in 0..600 {
            std::thread::sleep(std::time::Duration::from_millis(500));
            if binary_is_current(bin) {
                return Ok(());
            }
        }
        return Err("timeout waiting for concurrent download to complete".into());
    }

    let result = download_tyto(bin);
    let _ = lock_file.unlock();
    result
}

fn run_mcp_server(ready: Arc<AtomicBool>) {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut out = BufWriter::new(stdout.lock());

    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }
        let msg: serde_json::Value = match serde_json::from_str(&line) {
            Ok(m) => m,
            Err(_) => continue,
        };

        let id = msg.get("id").cloned();
        let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");

        let response = match method {
            "initialize" => {
                let proto = msg["params"]["protocolVersion"]
                    .as_str()
                    .unwrap_or("2024-11-05");
                Some(serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "protocolVersion": proto,
                        "capabilities": { "tools": {} },
                        "serverInfo": { "name": "tyto", "version": "0.0.0" },
                        "instructions":
                            "tyto is installing. Call session_context to check download status, \
                             then restart your session when the binary is ready."
                    }
                }))
            }
            "notifications/initialized" => None,
            "tools/list" => Some(serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "tools": [{
                        "name": "session_context",
                        "description": "Check tyto install status during first-time download.",
                        "inputSchema": { "type": "object", "properties": {} }
                    }]
                }
            })),
            "tools/call" => {
                let text = if ready.load(Ordering::Acquire) {
                    "tyto binary is now ready. Please restart your session to activate memory tools."
                } else {
                    "tyto is downloading its binary for first-time setup (up to ~30 seconds). \
                     Please restart your session once the download completes."
                };
                Some(serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": { "content": [{ "type": "text", "text": text }] }
                }))
            }
            _ => id.as_ref().map(|_| {
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": -32601, "message": "Method not found" }
                })
            }),
        };

        if let Some(resp) = response {
            let _ = writeln!(out, "{}", resp);
            let _ = out.flush();
        }
    }
}

// ---------------------------------------------------------------------------
// Download and extract tyto binary
// ---------------------------------------------------------------------------

fn download_tyto(bin: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let platform = detect_platform()?;
    let ext = if cfg!(target_os = "windows") { "zip" } else { "tar.gz" };
    let url = format!(
        "https://github.com/{}/releases/download/v{}/tyto-{}.{}",
        REPO, VERSION, platform, ext
    );
    eprintln!("tyto stub: downloading {}", url);

    if let Some(parent) = bin.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let bytes = ureq::get(&url).call()?.body_mut().read_to_vec()?;

    let bin_name = exe("tyto");
    let extracted = if cfg!(target_os = "windows") {
        extract_zip(&bytes, &bin_name)?
    } else {
        extract_tar_gz(&bytes, &bin_name)?
    };

    let tmp = bin.with_extension("tmp");
    std::fs::write(&tmp, &extracted)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
    }

    std::fs::rename(&tmp, bin)?;
    std::fs::write(version_file(bin), VERSION)?;
    Ok(())
}

fn detect_platform() -> Result<&'static str, Box<dyn std::error::Error>> {
    match std::env::consts::OS {
        "linux" => match std::env::consts::ARCH {
            "x86_64" => Ok("linux-x86_64"),
            "aarch64" => Ok("linux-aarch64"),
            a => Err(format!("unsupported Linux arch: {}", a).into()),
        },
        // macOS ships only aarch64; Rosetta handles Intel Macs transparently.
        "macos" => Ok("macos-aarch64"),
        "windows" => Ok("windows-x86_64"),
        os => Err(format!("unsupported OS: {}", os).into()),
    }
}

fn extract_tar_gz(data: &[u8], name: &str) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    use flate2::read::GzDecoder;
    use tar::Archive;
    let gz = GzDecoder::new(std::io::Cursor::new(data));
    let mut archive = Archive::new(gz);
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        if path.file_name().and_then(|n| n.to_str()) == Some(name) {
            let mut buf = Vec::new();
            entry.read_to_end(&mut buf)?;
            return Ok(buf);
        }
    }
    Err(format!("'{}' not found in tar.gz", name).into())
}

fn extract_zip(data: &[u8], name: &str) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let cursor = std::io::Cursor::new(data);
    let mut archive = zip::ZipArchive::new(cursor)?;
    for i in 0..archive.len() {
        let mut file = archive.by_index(i)?;
        if file.name().ends_with(name) {
            let mut buf = Vec::new();
            file.read_to_end(&mut buf)?;
            return Ok(buf);
        }
    }
    Err(format!("'{}' not found in zip", name).into())
}
