use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Local};

use crate::db::Db;
use crate::eventlog;
use crate::log;
use crate::util;

pub const EVENT_ID_ALERT: u32 = 100;

fn alerts_log_path() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("获取程序路径失败")?;
    Ok(exe
        .parent()
        .unwrap_or(Path::new("."))
        .join("logs")
        .join("alerts.log"))
}

fn fmt_ts(ts: i64) -> String {
    DateTime::from_timestamp(ts, 0)
        .map(|d| d.with_timezone(&Local).format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| ts.to_string())
}

/// 扫描越限写入：入库 → 写 alerts.log → 写事件日志 → 标记已通知。
/// scan_alerts 幂等（UNIQUE(ts,path)+OR IGNORE），重复调用不会重复告警。
pub fn scan(db: &mut Db, from_ts: i64, threshold: u64) -> Result<()> {
    if threshold == 0 {
        return Ok(());
    }
    db.scan_alerts(from_ts, threshold)?;
    let pending = db.pending_alerts()?;
    if pending.is_empty() {
        return Ok(());
    }
    let path = alerts_log_path()?;
    let mut f = log::open_capped(&path)
        .with_context(|| format!("打开告警日志 {} 失败", path.display()))?;
    let mut ids = Vec::with_capacity(pending.len());
    for a in &pending {
        let t = fmt_ts(a.ts);
        let size = util::human_bytes(a.bytes);
        let _ = writeln!(f, "[{t}] {size} {} ({})", a.path, a.process);
        log::warn(format!("告警: {t} 单分钟写入 {size}：{} ({})", a.path, a.process));
        eventlog::warn(
            EVENT_ID_ALERT,
            &format!("磁盘大写入：{} 单分钟写入 {} ({})", a.path, size, a.process),
        );
        ids.push(a.id);
    }
    db.mark_notified(&ids)?;
    Ok(())
}
