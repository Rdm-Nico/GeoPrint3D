use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::Path;
use std::sync::Mutex;
use std::time::SystemTime;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

const WEEK_SECS: u64 = 7 * 24 * 3600;

/// Rotates `log_path` to `log_path.YYYY-MM-DD` when the file is older than 1 week.
pub fn rotate_if_needed(log_path: &Path) -> anyhow::Result<()> {
    if !log_path.exists() {
        return Ok(());
    }

    let meta = fs::metadata(log_path)?;
    let birth = meta.created().or_else(|_| meta.modified())?;
    let age = SystemTime::now()
        .duration_since(birth)
        .unwrap_or_default();

    if age.as_secs() > WEEK_SECS {
        let date = chrono::Local::now().format("%Y-%m-%d");
        let archived = log_path.with_file_name(format!(
            "{}.{}",
            log_path.file_name().unwrap_or_default().to_string_lossy(),
            date
        ));
        fs::rename(log_path, &archived)?;
        eprintln!(
            "Log rotated: {} → {}",
            log_path.display(),
            archived.display()
        );
    }

    Ok(())
}

/// Initialises tracing with both a stdout layer and an appending file layer inside `log_dir`.
/// The returned `WorkerGuard` must be kept alive for the duration of the process.
pub fn init_logging(log_dir: &str) -> anyhow::Result<WorkerGuard> {
    fs::create_dir_all(log_dir)?;

    let log_path = Path::new(log_dir).join("backend.log");
    rotate_if_needed(&log_path)?;

    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;

    let (non_blocking, guard) = tracing_appender::non_blocking(file);

    let env_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "geoprint3d_backend=debug,tower_http=debug".into());

    // Console layer (with ANSI colours)
    let console_layer = fmt::layer()
        .with_file(true)
        .with_line_number(true)
        .with_target(true);

    // File layer (plain text, no ANSI codes)
    let file_layer = fmt::layer()
        .with_file(true)
        .with_line_number(true)
        .with_target(true)
        .with_ansi(false)
        .with_writer(non_blocking);

    tracing_subscriber::registry()
        .with(env_filter)
        .with(console_layer)
        .with(file_layer)
        .init();

    tracing::info!("Logging initialised — file: {}", log_path.display());
    Ok(guard)
}

// ─── Frontend log writer ──────────────────────────────────────────────────────

#[derive(Debug, serde::Deserialize)]
pub struct FrontendLogEntry {
    pub timestamp: String,
    pub level: String,
    pub file: String,
    pub line: Option<u32>,
    pub message: String,
}

#[derive(Debug, serde::Deserialize)]
pub struct FrontendLogBatch {
    pub entries: Vec<FrontendLogEntry>,
}

/// Thread-safe writer that appends frontend log entries to `frontend/logs/frontend.log`.
pub struct FrontendLogWriter {
    writer: Mutex<BufWriter<File>>,
}

impl FrontendLogWriter {
    pub fn new(log_dir: &str) -> anyhow::Result<Self> {
        fs::create_dir_all(log_dir)?;

        let log_path = Path::new(log_dir).join("frontend.log");
        rotate_if_needed(&log_path)?;

        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)?;

        tracing::info!("Frontend log file: {}", log_path.display());
        Ok(Self {
            writer: Mutex::new(BufWriter::new(file)),
        })
    }

    pub fn write_batch(&self, batch: &FrontendLogBatch) {
        let mut w = match self.writer.lock() {
            Ok(g) => g,
            Err(e) => {
                tracing::error!("Frontend log mutex poisoned: {}", e);
                return;
            }
        };
        for entry in &batch.entries {
            let line_info = entry
                .line
                .map(|l| l.to_string())
                .unwrap_or_else(|| "?".to_string());

            let _ = writeln!(
                w,
                "{} [{:>5}] {}:{} — {}",
                entry.timestamp, entry.level, entry.file, line_info, entry.message
            );
        }
        let _ = w.flush();
    }
}
