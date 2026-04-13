use std::{
    fs::File,
    io::Write,
    path::Path,
    sync::{Mutex, OnceLock},
};

static LOG_FILE: OnceLock<Mutex<File>> = OnceLock::new();

/// Open `path` as the process-wide log file (append mode).
/// All subsequent `mlog!` calls will mirror their output there.
/// Idempotent — subsequent calls are silently ignored.
/// Must be called before spawning any threads that use `mlog!`.
pub fn init(path: &Path) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match File::options().create(true).append(true).open(path) {
        Ok(f) => {
            let _ = LOG_FILE.set(Mutex::new(f));
        }
        Err(e) => eprintln!("[memso] WARNING: could not open log file {}: {e}", path.display()),
    }
}

/// Append `msg` as a line to the open log file. Flushes immediately.
/// No-op if `init` was not called (e.g. in `inject` or `remote` subcommands).
pub fn write(msg: &str) {
    if let Some(lock) = LOG_FILE.get()
        && let Ok(mut f) = lock.lock()
    {
        let _ = writeln!(f, "{msg}");
        let _ = f.flush();
    }
}

/// Log to both stderr and the log file with a UTC timestamp prefix.
/// If no log file has been opened via `log::init`, output goes to stderr only.
#[macro_export]
macro_rules! mlog {
    ($($arg:tt)*) => {{
        let _msg = format!($($arg)*);
        let _line = format!("[{}] {}", ::chrono::Utc::now().format("%Y-%m-%d %H:%M:%S"), _msg);
        eprintln!("{}", _line);
        $crate::log::write(&_line);
    }};
}
