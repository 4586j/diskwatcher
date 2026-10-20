use std::fmt::Display;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// 单个日志文件的尺寸上限。超过后整份改名为 `<名>.1`（覆盖上一份）再重开，
/// 因此每种日志在盘上最多占 2 倍上限。按实测增速（collect.log ≈ 120 KB/天）约一个多月轮转一次。
pub const MAX_LOG_BYTES: u64 = 4 * 1024 * 1024;

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

/// 以追加方式打开日志，超过上限先轮转。所有写日志文件的地方都走这里，
/// 否则总有一份日志会被漏掉、继续无界增长。
pub fn open_capped(path: &Path) -> std::io::Result<File> {
    open_capped_with(path, MAX_LOG_BYTES)
}

fn open_capped_with(path: &Path, max: u64) -> std::io::Result<File> {
    let f = OpenOptions::new().create(true).append(true).open(path)?;
    // 在已打开的句柄上取长度，不额外按路径再查一次
    if f.metadata().map(|m| m.len() < max).unwrap_or(true) {
        return Ok(f);
    }
    drop(f);
    // CLI 与服务可能同时写同一份日志，改名失败（被别的进程占着）就先接着追加，下一行再试
    let _ = std::fs::rename(path, rotated(path));
    OpenOptions::new().create(true).append(true).open(path)
}

fn rotated(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".1");
    path.with_file_name(name)
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
        && let Ok(mut f) = open_capped(path)
    {
        let _ = writeln!(f, "{line}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("diskwatch-log-tests").join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("collect.log")
    }

    #[test]
    fn under_cap_appends_in_place() {
        let p = tmp("under");
        std::fs::write(&p, "a\n").unwrap();
        writeln!(open_capped_with(&p, 100).unwrap(), "b").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "a\nb\n");
        assert!(!rotated(&p).exists());
    }

    #[test]
    fn over_cap_rotates_to_dot_one_and_starts_fresh() {
        let p = tmp("over");
        std::fs::write(&p, "x".repeat(100)).unwrap();
        writeln!(open_capped_with(&p, 100).unwrap(), "new").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "new\n");
        assert_eq!(std::fs::read_to_string(rotated(&p)).unwrap().len(), 100);
    }

    #[test]
    fn only_one_backup_is_kept() {
        let p = tmp("one-backup");
        std::fs::write(&p, "first".repeat(30)).unwrap();
        drop(open_capped_with(&p, 100).unwrap());
        std::fs::write(&p, "second".repeat(30)).unwrap();
        drop(open_capped_with(&p, 100).unwrap());
        // 第二次轮转覆盖 .1，盘上始终只有 当前 + 一份备份
        assert!(std::fs::read_to_string(rotated(&p)).unwrap().starts_with("second"));
        assert_eq!(std::fs::read_dir(p.parent().unwrap()).unwrap().count(), 2);
    }

    #[test]
    fn rotated_name_appends_suffix() {
        assert_eq!(
            rotated(Path::new(r"C:\x\logs\alerts.log")),
            PathBuf::from(r"C:\x\logs\alerts.log.1")
        );
    }
}
