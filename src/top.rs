use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::{Datelike, Local, TimeZone, Timelike};

use crate::db::Db;
use crate::util;

pub fn run(
    db_path: &Path,
    last: &str,
    by: &str,
    limit: usize,
    contains: Option<&str>,
) -> Result<()> {
    let dur = humantime::parse_duration(last)
        .with_context(|| format!("无法解析时间段: {last}（示例: 30m / 1h / 2d）"))?;
    let now = Local::now().timestamp();
    let from = now - dur.as_secs() as i64;

    let db = Db::open(db_path)?;
    let rows = db.top(from, usize::MAX.min(1_000_000), contains)?;

    match by {
        "file" => print_rows(&rows.into_iter().take(limit).collect::<Vec<_>>(), from, now),
        "dir" => {
            let mut dirs: HashMap<String, (u64, u64)> = HashMap::new();
            for r in &rows {
                let parent = match r.path.rfind('\\') {
                    Some(i) if i > 2 => r.path[..i].to_string(),
                    _ => r.path.clone(),
                };
                let e = dirs.entry(parent).or_insert((0, 0));
                e.0 += r.total;
                e.1 += r.ops;
            }
            let mut v: Vec<(String, u64, u64)> =
                dirs.into_iter().map(|(p, (b, o))| (p, b, o)).collect();
            v.sort_by(|a, b| b.1.cmp(&a.1));
            v.truncate(limit);
            println!("时间范围: {} ~ {}（按目录汇总）", fmt_ts(from), fmt_ts(now));
            println!("{:>10}  {:>8}  {}", "写入量", "次数", "目录");
            for (p, b, o) in &v {
                println!("{:>10}  {:>8}  {}", util::human_bytes(*b), o, p);
            }
            Ok(())
        }
        other => anyhow::bail!("--by 只支持 file 或 dir，收到: {other}"),
    }
}

fn print_rows(rows: &[crate::db::TopRow], from: i64, now: i64) -> Result<()> {
    println!("时间范围: {} ~ {}", fmt_ts(from), fmt_ts(now));
    if rows.is_empty() {
        println!("（该时间段内没有达到入库阈值的写入记录）");
        return Ok(());
    }
    println!(
        "{:>10}  {:>8}  {:>23}  {:<28}  {}",
        "写入量", "次数", "时间范围", "进程", "路径"
    );
    for r in rows {
        println!(
            "{:>10}  {:>8}  {:>11} ~ {:<11}  {:<28}  {}",
            util::human_bytes(r.total),
            r.ops,
            fmt_ts(r.first),
            fmt_ts(r.last),
            truncate(&r.procs, 28),
            r.path
        );
    }
    Ok(())
}

fn fmt_ts(ts: i64) -> String {
    let dt = Local
        .timestamp_opt(ts, 0)
        .single()
        .unwrap_or_else(|| Local::now());
    format!(
        "{:02}-{:02} {:02}:{:02}",
        dt.month(),
        dt.day(),
        dt.hour(),
        dt.minute()
    )
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{cut}…")
    }
}
