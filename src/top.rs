// top：按时间段查询写入量排行。文本对齐表是给人看的；--json 输出仅 stdout、无夹带，
// 字段全用原始整数/Unix 秒（与 Web API 口径一致），供脚本与 agent 直接消费。

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::{Datelike, Local, TimeZone, Timelike};
use serde_json::{json, Value};

use crate::db::{Db, ProcessRow, Range, TopRow};
use crate::util;

pub fn run(
    db_path: &Path,
    last: &str,
    by: &str,
    limit: usize,
    contains: Option<&str>,
    json: bool,
) -> Result<()> {
    let dur = humantime::parse_duration(last)
        .with_context(|| format!("无法解析时间段: {last}（示例: 30m / 1h / 2d）"))?;
    let now = Local::now().timestamp();
    let from = now - dur.as_secs() as i64;

    let db = Db::open(db_path)?;
    let contains = contains.unwrap_or("");

    match by {
        "file" => {
            let rows = db.top(from, limit.min(1_000_000), Some(contains))?;
            if json {
                print_json(json!({
                    "ok": true,
                    "group": "file",
                    "from": from, "to": now,
                    "from_human": fmt_ts(from), "to_human": fmt_ts(now),
                    "rows": file_rows_json(&rows),
                }))?;
            } else {
                print_file_rows(&rows, from, now);
            }
            Ok(())
        }
        "dir" => {
            let rows = db.top(from, usize::MAX.min(1_000_000), Some(contains))?;
            let dirs = fold_dirs(&rows, limit);
            if json {
                let rows: Vec<Value> = dirs
                    .iter()
                    .map(|d| {
                        json!({
                            "path": d.dir,
                            "bytes": d.bytes,
                            "ops": d.ops,
                            "first": d.first, "last": d.last,
                            "process": serde_json::Value::Null,
                        })
                    })
                    .collect();
                print_json(json!({
                    "ok": true,
                    "group": "dir",
                    "from": from, "to": now,
                    "from_human": fmt_ts(from), "to_human": fmt_ts(now),
                    "rows": rows,
                }))?;
            } else {
                print_dir_rows(&dirs, from, now);
            }
            Ok(())
        }
        "process" => {
            let rows = db.by_process_in(
                Range { from, to: i64::MAX },
                contains,
                "",
                limit,
            )?;
            if json {
                print_json(json!({
                    "ok": true,
                    "group": "process",
                    "from": from, "to": now,
                    "from_human": fmt_ts(from), "to_human": fmt_ts(now),
                    "rows": process_rows_json(&rows),
                }))?;
            } else {
                print_process_rows(&rows, from, now);
            }
            Ok(())
        }
        other => anyhow::bail!("--by 只支持 file / dir / process，收到: {other}"),
    }
}

// ---------- 行 → JSON ----------

fn file_rows_json(rows: &[TopRow]) -> Vec<Value> {
    rows.iter()
        .map(|r| {
            json!({
                "path": r.path,
                "bytes": r.total, "ops": r.ops,
                "first": r.first, "last": r.last,
                "process": r.procs,
            })
        })
        .collect()
}

fn process_rows_json(rows: &[ProcessRow]) -> Vec<Value> {
    rows.iter()
        .map(|r| {
            json!({
                "process": r.process,
                "bytes": r.bytes, "ops": r.ops, "files": r.files,
            })
        })
        .collect()
}

fn print_json(v: Value) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(&v)?);
    Ok(())
}

// ---------- 目录折叠（file 行 → 目录行；dir 文本与 dir JSON 共用同口径，返回已排序截断） ----------

struct DirAgg {
    dir: String,
    bytes: u64,
    ops: u64,
    first: i64,
    last: i64,
}

fn fold_dirs(rows: &[TopRow], limit: usize) -> Vec<DirAgg> {
    let mut dirs: HashMap<String, DirAgg> = HashMap::new();
    for r in rows {
        let parent = match r.path.rfind('\\') {
            Some(i) if i > 2 => r.path[..i].to_string(),
            _ => r.path.clone(),
        };
        let e = dirs.entry(parent.clone()).or_insert(DirAgg {
            dir: parent,
            bytes: 0,
            ops: 0,
            first: i64::MAX,
            last: i64::MIN,
        });
        e.bytes += r.total;
        e.ops += r.ops;
        e.first = e.first.min(r.first);
        e.last = e.last.max(r.last);
    }
    let mut v: Vec<DirAgg> = dirs.into_values().collect();
    v.sort_by(|a, b| b.bytes.cmp(&a.bytes));
    v.truncate(limit);
    v
}

// ---------- 文本输出 ----------

fn print_file_rows(rows: &[TopRow], from: i64, now: i64) {
    println!("时间范围: {} ~ {}", fmt_ts(from), fmt_ts(now));
    if rows.is_empty() {
        println!("（该时间段内没有达到入库阈值的写入记录）");
        return;
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
}

fn print_dir_rows(dirs: &[DirAgg], from: i64, now: i64) {
    println!("时间范围: {} ~ {}（按目录汇总）", fmt_ts(from), fmt_ts(now));
    println!("{:>10}  {:>8}  {}", "写入量", "次数", "目录");
    for d in dirs {
        println!("{:>10}  {:>8}  {}", util::human_bytes(d.bytes), d.ops, d.dir);
    }
}

fn print_process_rows(rows: &[ProcessRow], from: i64, now: i64) {
    println!("时间范围: {} ~ {}（按进程汇总）", fmt_ts(from), fmt_ts(now));
    if rows.is_empty() {
        println!("（该时间段内没有达到入库阈值的写入记录）");
        return;
    }
    println!("{:>10}  {:>8}  {:>8}  {}", "写入量", "次数", "文件数", "进程");
    for r in rows {
        println!(
            "{:>10}  {:>8}  {:>8}  {}",
            util::human_bytes(r.bytes),
            r.ops,
            r.files,
            r.process
        );
    }
}

fn fmt_ts(ts: i64) -> String {
    let dt = Local
        .timestamp_opt(ts, 0)
        .single()
        .unwrap_or_else(Local::now);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn top_row(path: &str, total: u64, ops: u64, first: i64, last: i64) -> TopRow {
        TopRow {
            path: path.to_string(),
            total,
            ops,
            first,
            last,
            procs: "a.exe".to_string(),
        }
    }

    #[test]
    fn fold_dirs_aggregates_by_parent() {
        let rows = vec![
            top_row(r"C:\app\a.txt", 100, 1, 1, 1),
            top_row(r"C:\app\b.txt", 50, 2, 2, 2),
            top_row(r"C:\other\c.txt", 200, 3, 3, 3),
        ];
        let dirs = fold_dirs(&rows, 10);
        assert_eq!(dirs.len(), 2);
        // 按字节降序：C:\other 200 > C:\app 150
        assert_eq!(dirs[0].dir, r"C:\other");
        assert_eq!(dirs[0].bytes, 200);
        assert_eq!(dirs[0].first, 3);
        assert_eq!(dirs[0].last, 3);
        assert_eq!(dirs[1].dir, r"C:\app");
        assert_eq!(dirs[1].bytes, 150);
        assert_eq!(dirs[1].ops, 3);
        assert_eq!(dirs[1].first, 1);
        assert_eq!(dirs[1].last, 2);
    }

    #[test]
    fn fold_dirs_keeps_root_files_untouched() {
        // 路径 "C:\a.txt" 的反斜杠在 index=2，不满足 i>2，保留整条路径作为“目录”。
        let rows = vec![top_row(r"C:\a.txt", 100, 1, 1, 1)];
        let dirs = fold_dirs(&rows, 10);
        assert_eq!(dirs.len(), 1);
        assert_eq!(dirs[0].dir, r"C:\a.txt");
    }

    #[test]
    fn fold_dirs_honors_limit() {
        let rows = (0..5)
            .map(|i| top_row(&format!(r"C:\d{i}\f.log"), i * 100, 1, 1, 1))
            .collect::<Vec<_>>();
        let dirs = fold_dirs(&rows, 2);
        assert_eq!(dirs.len(), 2);
        assert_eq!(dirs[0].dir, r"C:\d4");
        assert_eq!(dirs[1].dir, r"C:\d3");
    }

    #[test]
    fn file_rows_json_keeps_raw_integers() {
        let rows = [top_row(r"C:\x\y.bin", 1024, 3, 1700000000, 1700000060)];
        let v = file_rows_json(&rows);
        let o = v[0].as_object().expect("行应为对象");
        assert_eq!(o["bytes"], 1024);
        assert_eq!(o["ops"], 3);
        assert_eq!(o["first"], 1700000000);
        assert_eq!(o["path"], r"C:\x\y.bin");
    }

    #[test]
    fn process_rows_json_shape() {
        let rows = [ProcessRow {
            process: "chrome.exe".to_string(),
            bytes: 1024,
            ops: 3,
            files: 2,
        }];
        let v = process_rows_json(&rows);
        let o = v[0].as_object().expect("行应为对象");
        assert_eq!(o["process"], "chrome.exe");
        assert_eq!(o["files"], 2);
        assert!(o.get("first").is_none(), "process 行不应带文件时间字段");
    }
}