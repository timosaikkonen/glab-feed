//! File output for the `log` macros, enabled with `--debug`.
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, Result};
use chrono::{Local, SecondsFormat};
use log::{LevelFilter, Log, Metadata, Record};

struct FileLogger {
    file: Mutex<File>,
}

impl Log for FileLogger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        is_own_target(metadata.target())
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let line = format!(
            "{} {} {}\n",
            Local::now().to_rfc3339_opts(SecondsFormat::Millis, false),
            record.level(),
            record.args()
        );
        if let Ok(mut file) = self.file.lock() {
            let _ = file.write_all(line.as_bytes());
        }
    }

    fn flush(&self) {
        if let Ok(mut file) = self.file.lock() {
            let _ = file.flush();
        }
    }
}

/// Dependencies such as `mio` also log through `log`; keep only our records.
fn is_own_target(target: &str) -> bool {
    target == "glab_feed" || target.starts_with("glab_feed::")
}

/// Append debug records to `path` for the rest of the process.
pub fn init(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating log dir {}", parent.display()))?;
    }
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("opening debug log {}", path.display()))?;
    log::set_boxed_logger(Box::new(FileLogger {
        file: Mutex::new(file),
    }))
    .context("installing debug logger")?;
    log::set_max_level(LevelFilter::Debug);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::is_own_target;

    #[test]
    fn keeps_only_glab_feed_targets() {
        assert!(is_own_target("glab_feed"));
        assert!(is_own_target("glab_feed::gitlab"));
        assert!(!is_own_target("mio::poll"));
        assert!(!is_own_target("glab_feedback"));
    }
}
