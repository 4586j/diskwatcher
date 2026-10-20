// 导出编排：把「视图 + 时间窗 + 筛选 + 聚合粒度」变成一段 CSV 字节。
// Web 端点和 CLI 都走这里，避免两条路径各写一套口径而慢慢漂移。

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Local, TimeZone};

use crate::csv;
use crate::db::{self, Db, FileAggRow, Group, Range};
use crate::footprint::{self, Usage};
use crate::log;

/// 现值扫描的行数上限：目录要递归，导出几万行明细时全量扫描会把一次导出变成几分钟的磁盘忙。
/// 只扫写入量最大的这些行，尾部零头标「未测」。
const MAX_USAGE_ROWS: usize = 300;

/// 与 rows 按序号对齐的现值表；`usage=false` 时返回空表（整列未测）。
/// 折叠出的目录行按路径序而非字节序排列，所以这里自己按写入量挑前 N 个。
fn measure(rows: &[FileAggRow], usage: bool) -> Vec<Usage> {
    if !usage || rows.is_empty() {
        return Vec::new();
    }
    let mut idx: Vec<usize> = (0..rows.len()).collect();
    idx.sort_by_key(|&i| std::cmp::Reverse(rows[i].bytes));
    idx.truncate(MAX_USAGE_ROWS);
    let paths: Vec<String> = idx.iter().map(|&i| rows[i].path.clone()).collect();
    let mut out = vec![Usage::default(); rows.len()];
    for (slot, u) in idx.iter().zip(footprint::of_paths(&paths, footprint::Budget::CLI)) {
        out[*slot] = u;
    }
    out
}

/// `diskwatch export` 的实现：一次写出三份 CSV。
/// CLI 没有浏览器多文件下载的限制，所以三类数据全部导出，而不是只导当前视图。
pub fn run_cli(
    db_path: &Path,
    last: &str,
    out: Option<PathBuf>,
    contains: &str,
    process: &str,
    by: &str,
    usage: bool,
) -> Result<()> {
    let group = match by {
        "file" => Group::File,
        "dir" => Group::Dir,
        "none" => Group::Raw,
        other => anyhow::bail!("--by 只支持 file / dir / none，收到: {other}"),
    };
    let dur = humantime::parse_duration(last)
        .with_context(|| format!("无法解析时间段: {last}（示例: 30m / 1h / 7d）"))?;
    let now = Local::now().timestamp();
    let range = Range {
        from: now - dur.as_secs() as i64,
        to: now,
    };
    let q = contains.to_lowercase();

    let dir = match out {
        Some(p) => p,
        None => {
            let exe = std::env::current_exe().context("获取程序路径失败")?;
            exe.parent()
                .unwrap_or(Path::new("."))
                .join("exports")
        }
    };
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("创建目录失败: {}", dir.display()))?;

    let db = Db::open(db_path)?;
    let day = |ts: i64| {
        local(ts)
            .map(|dt| dt.format("%Y-%m-%d %H:%M:%S").to_string())
            .unwrap_or_else(|| ts.to_string())
    };
    log::info(format!(
        "导出窗口 {} ~ {}，筛选 q={:?} process={:?}",
        day(range.from),
        day(range.to),
        q,
        process
    ));

    for view in [View::Writes, View::Usn, View::Alerts] {
        let proc = if view.supports_process() { process } else { "" };
        let grp = if view == View::Writes { group } else { Group::File };
        let buf = build(&db, view, range, &q, proc, grp, usage)?;
        let path = dir.join(filename(view, range));
        std::fs::write(&path, &buf)
            .with_context(|| format!("写入失败: {}", path.display()))?;
        // 表头不算数据行；BOM 与换行在字节层，行数按解码后的文本数。
        let rows = String::from_utf8_lossy(&buf[3..])
            .lines()
            .filter(|l| !l.is_empty())
            .count()
            .saturating_sub(1);
        println!("[diskwatch] {} -> {}（{} 行）", view.slug(), path.display(), rows);
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Writes,
    Usn,
    Alerts,
}

impl View {
    pub fn parse(s: &str) -> Option<View> {
        match s {
            "writes" => Some(View::Writes),
            "usn" => Some(View::Usn),
            "alerts" => Some(View::Alerts),
            _ => None,
        }
    }

    pub fn slug(self) -> &'static str {
        match self {
            View::Writes => "writes",
            View::Usn => "usn",
            View::Alerts => "alerts",
        }
    }

    /// 离线补录没有进程维度，进程参数必须在这一层就挡掉，否则会静默导出空表。
    pub fn supports_process(self) -> bool {
        self != View::Usn
    }
}

pub fn build(
    db: &Db,
    view: View,
    range: Range,
    q: &str,
    process: &str,
    group: Group,
    usage: bool,
) -> Result<Vec<u8>> {
    let mut buf: Vec<u8> = Vec::new();
    match view {
        View::Writes => match group {
            // 分钟明细不做现值：同一路径会重复出现，逐个再扫一遍磁盘只是把重复摊到每次扫描上。
            Group::Raw => csv::writes_raw_csv(&mut buf, &db.all_writes_raw(range, q, process)?)?,
            Group::Dir => {
                let rows = db::fold_writes_dirs(db.all_writes(range, q, process)?);
                csv::writes_csv(&mut buf, &rows, &measure(&rows, usage))?;
            }
            Group::File => {
                let rows = db.all_writes(range, q, process)?;
                csv::writes_csv(&mut buf, &rows, &measure(&rows, usage))?;
            }
        },
        View::Usn => {
            let rows = db.all_usn(range, q)?;
            // 回填行本身就是文件级快照，"按文件"与"不聚合"同义。
            if group == Group::Dir {
                csv::usn_csv(&mut buf, &db::fold_usn_dirs(rows))?;
            } else {
                csv::usn_csv(&mut buf, &rows)?;
            }
        }
        View::Alerts => csv::alerts_csv(&mut buf, &db.all_alerts(range, q, process)?)?,
    }
    Ok(buf)
}

/// 文件名带窗口日期，重复导出不会互相覆盖，也便于在 Downloads 里认出是哪一次。
pub fn filename(view: View, range: Range) -> String {
    let day = local(range.to)
        .map(|dt| dt.format("%Y%m%d").to_string())
        .unwrap_or_else(|| "export".to_string());
    format!("diskwatch-{}-{}.csv", view.slug(), day)
}

/// 夏令时/歧义时间戳返回 None，调用方回落到原始 epoch 而不是 panic。
fn local(ts: i64) -> Option<DateTime<Local>> {
    Local.timestamp_opt(ts, 0).single()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{UsnRow, WriteRow};

    fn tmp_db(name: &str) -> (Db, std::path::PathBuf) {
        let dir = std::env::temp_dir().join("diskwatch-tests");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        let _ = std::fs::remove_file(&path);
        (Db::open(&path).unwrap(), path)
    }

    fn text(buf: &[u8]) -> String {
        String::from_utf8(buf[3..].to_vec()).unwrap()
    }

    #[test]
    fn view_parse_and_slug_round_trip() {
        for s in ["writes", "usn", "alerts"] {
            assert_eq!(View::parse(s).unwrap().slug(), s);
        }
        assert_eq!(View::parse("nope"), None);
        assert!(!View::Usn.supports_process());
        assert!(View::Writes.supports_process());
    }

    #[test]
    fn empty_window_exports_header_only() {
        let (db, _p) = tmp_db("export-empty.db");
        let buf = build(&db, View::Writes, Range { from: 0, to: 10 }, "", "", Group::File, true).unwrap();
        assert_eq!(text(&buf).lines().count(), 1);
    }

    #[test]
    fn all_three_views_export_seeded_rows() {
        let (mut db, _p) = tmp_db("export-all.db");
        db.insert_writes(&[
            WriteRow { ts: 1_000, path: r"C:\tmp\a.log".into(), process: "a.exe".into(), bytes: 500, ops: 2 },
            WriteRow { ts: 2_000, path: r"C:\tmp\b.log".into(), process: "b.exe".into(), bytes: 600, ops: 3 },
            WriteRow { ts: 1_500, path: r"C:\bin\c.log".into(), process: "a.exe".into(), bytes: 700, ops: 1 },
        ])
        .unwrap();
        db.insert_usn_changes(
            &[UsnRow { ts: 100, path: r"C:\u\x.bin".into(), bytes: 50 }],
            1,
            1,
        )
        .unwrap();
        let r = Range { from: 0, to: 10_000 };

        let f = build(&db, View::Writes, r, "", "", Group::File, true).unwrap();
        assert_eq!(text(&f).lines().count(), 4, "三个文件各一行");

        let d = build(&db, View::Writes, r, "", "", Group::Dir, true).unwrap();
        assert_eq!(text(&d).lines().count(), 3, "折成 C:\\tmp 与 C:\\bin 两行");

        let raw = build(&db, View::Writes, r, "", "", Group::Raw, true).unwrap();
        assert_eq!(text(&raw).lines().count(), 4);
        assert!(text(&raw).starts_with("时间,文件路径,字节数,操作次数,进程"));

        let u = build(&db, View::Usn, r, "", "", Group::File, true).unwrap();
        assert_eq!(text(&u).lines().count(), 2);

        let a = build(&db, View::Alerts, r, "", "", Group::File, true).unwrap();
        assert_eq!(text(&a).lines().count(), 1, "未触发告警时只有表头");
    }

    #[test]
    fn filters_apply_to_export() {
        let (mut db, _p) = tmp_db("export-filter.db");
        db.insert_writes(&[
            WriteRow { ts: 1_000, path: r"C:\tmp\报表.xlsx".into(), process: "excel.exe".into(), bytes: 500, ops: 2 },
            WriteRow { ts: 2_000, path: r"C:\tmp\other.log".into(), process: "svc.exe".into(), bytes: 600, ops: 3 },
        ])
        .unwrap();
        let r = Range { from: 0, to: 10_000 };

        let by_q = build(&db, View::Writes, r, "报表", "", Group::File, true).unwrap();
        assert_eq!(text(&by_q).lines().count(), 2);
        assert!(text(&by_q).contains("报表"));

        let by_proc = build(&db, View::Writes, r, "", "svc.exe", Group::File, true).unwrap();
        assert_eq!(text(&by_proc).lines().count(), 2);
        assert!(text(&by_proc).contains("other.log"));

        let outside = build(&db, View::Writes, Range { from: 5_000, to: 10_000 }, "", "", Group::File, true).unwrap();
        assert_eq!(text(&outside).lines().count(), 1, "窗口外不应带出行");
    }

    #[test]
    fn export_carries_current_footprint_when_measured() {
        let dir = std::env::temp_dir().join("diskwatch-tests\\usage-real");
        std::fs::create_dir_all(&dir).unwrap();
        let live = dir.join("still-here.bin");
        std::fs::write(&live, vec![9u8; 4096]).unwrap();
        let live = live.display().to_string();

        let (mut db, _p) = tmp_db("export-usage.db");
        db.insert_writes(&[
            WriteRow { ts: 1_000, path: live.clone(), process: "a.exe".into(), bytes: 90_000, ops: 2 },
            // 写完就被回收掉的临时文件：写入量再大，现值也只是「已删除」
            WriteRow { ts: 2_000, path: dir.join("gone.bin").display().to_string(), process: "a.exe".into(), bytes: 900_000, ops: 3 },
        ])
        .unwrap();
        let r = Range { from: 0, to: 10_000 };

        let with = text(&build(&db, View::Writes, r, "", "", Group::File, true).unwrap());
        let live_row = with.lines().find(|l| l.contains(&live)).unwrap();
        assert!(live_row.ends_with(",4096,完整"), "got {live_row}");
        let gone_row = with.lines().find(|l| l.contains("gone.bin")).unwrap();
        assert!(gone_row.ends_with(",0,已删除"), "got {gone_row}");

        let without = text(&build(&db, View::Writes, r, "", "", Group::File, false).unwrap());
        assert!(without.lines().nth(1).unwrap().ends_with(",未测"));
        assert!(without.lines().nth(2).unwrap().ends_with(",未测"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn filename_uses_view_and_window_date() {
        let name = filename(View::Writes, Range { from: 0, to: 1_700_000_000 });
        assert!(name.starts_with("diskwatch-writes-202311"), "got {name}");
        assert!(name.ends_with(".csv"));
    }
}
