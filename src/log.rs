use std::fmt::Display;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

static LOG_FILE: OnceLock<Mutex<Option<PathBuf>>> = OnceLock::new();

pub fn init(path: &Path) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let slot = LOG_FILE.get_or_init(|| Mutex::new(None));
    *slot.lock().unwrap() = Some(path.to_path_buf());
}

pub fn info(msg: impl Display) {
    write_line("INFO", &msg.to_string());
}

pub fn warn(msg: impl Display) {
    write_line("WARN", &msg.to_string());
}

pub fn error(msg: impl Display) {
    write_line("ERROR", &msg.to_string());
}

fn write_line(level: &str, msg: &str) {
    let line = format!(
        "[{}] [{}] {}",
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
        level,
        msg
    );
    let mut stdout = std::io::stdout().lock();
    let _ = writeln!(stdout, "{line}");
    if let Some(slot) = LOG_FILE.get()
        && let Ok(guard) = slot.lock()
        && let Some(path) = guard.as_ref()
        && let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path)
    {
        let _ = writeln!(f, "{line}");
    }
}
