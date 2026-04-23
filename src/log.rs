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
        Err(e) => eprintln!("[tyto] WARNING: could not open log file {}: {e}", path.display()),
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

/// Initialize the tracing subscriber. Reads RUST_LOG for filter directives.
/// If RUST_LOG is unset or empty, tracing is disabled (no output).
/// Call once at process startup, before spawning threads. Idempotent - safe to
/// call multiple times (subsequent calls are silently ignored).
///
/// For short-lived processes (inject, request): writes to stderr.
/// For the serve process: call init_tracing_to_file() after log::init() so
/// tracing output lands in the log file rather than the discarded stdio stderr.
pub fn init_tracing() {
    use tracing_subscriber::{fmt, EnvFilter};
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("off"));
    let _ = fmt::Subscriber::builder()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}

/// Like init_tracing(), but routes output to the open log file.
/// Use in `tyto serve` after calling log::init() — the serve process's stderr
/// is not captured by Claude Code, so tracing would be silently discarded otherwise.
pub fn init_tracing_to_file() {
    use tracing_subscriber::{fmt, EnvFilter};
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("off"));
    // MakeWriter that appends each tracing event to the log file via log::write.
    // Uses a newtype so we can implement tracing_subscriber::fmt::MakeWriter.
    struct LogFileWriter;
    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogFileWriter {
        type Writer = LogFileWriter;
        fn make_writer(&'a self) -> Self::Writer { LogFileWriter }
    }
    impl std::io::Write for LogFileWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if let Ok(s) = std::str::from_utf8(buf) {
                let trimmed = s.trim_end_matches('\n');
                if !trimmed.is_empty() {
                    write(trimmed);
                }
            }
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
    }
    let _ = fmt::Subscriber::builder()
        .with_env_filter(filter)
        .with_writer(LogFileWriter)
        .try_init();
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
