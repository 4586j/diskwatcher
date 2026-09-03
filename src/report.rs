use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Local};

use crate::db::{AlertRow, Db, ProcessRow, TopRow, Totals, UsnRow};
use crate::util;

pub fn run(db_path: &Path, hours: u64, out: Option<PathBuf>) -> Result<()> {
    let db = Db::open(db_path)?;
    let now = Local::now();
    let from_ts = now.timestamp() - (hours * 3600) as i64;

    let totals = db.totals(from_ts)?;
    let files = db.top(from_ts, 500, None)?;
    let procs = db.by_process(from_ts, 12)?;
    let hourly = db.by_hour(from_ts)?;
    let alerts = db.alerts(from_ts)?;
    let usn = db.usn_changes(from_ts)?;

    let mut dir_map: HashMap<String, (u64, u64)> = HashMap::new();
    for f in &files {
        let dir = match f.path.rfind('\\') {
            Some(i) if i > 2 => &f.path[..i],
            _ => r"C:\",
        };
        let e = dir_map.entry(dir.to_string()).or_insert((0, 0));
        e.0 += f.total;
        e.1 += f.ops;
    }
    let mut dirs: Vec<(String, u64, u64)> = dir_map
        .into_iter()
        .map(|(d, (b, o))| (d, b, o))
        .collect();
    dirs.sort_by(|a, b| b.1.cmp(&a.1));
    dirs.truncate(12);

    let html = render(
        now, from_ts, &totals, &files, &procs, &dirs, &hourly, &alerts, &usn,
    );

    let out_path = match out {
        Some(p) => p,
        None => {
            let exe = std::env::current_exe().context("获取程序路径失败")?;
            exe.parent()
                .unwrap_or(Path::new("."))
                .join("reports")
                .join(format!("report-{}.html", now.format("%Y-%m-%d")))
        }
    };
    if let Some(dir) = out_path.parent() {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("创建目录失败: {}", dir.display()))?;
    }
    std::fs::write(&out_path, html)
        .with_context(|| format!("写入报告失败: {}", out_path.display()))?;
    println!("[diskwatch] 报告已生成: {}", out_path.display());
    Ok(())
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn fmt_dt(ts: i64) -> String {
    DateTime::from_timestamp(ts, 0)
        .map(|d| d.with_timezone(&Local).format("%m-%d %H:%M").to_string())
        .unwrap_or_else(|| ts.to_string())
}

fn fmt_hm(ts: i64) -> String {
    DateTime::from_timestamp(ts, 0)
        .map(|d| d.with_timezone(&Local).format("%H:%M").to_string())
        .unwrap_or_else(|| ts.to_string())
}

fn bar_row(label: &str, bytes: u64, max: u64, sub: &str, delay_ms: u64) -> String {
    let pct = if max > 0 {
        ((bytes as f64 / max as f64) * 100.0).max(2.0)
    } else {
        0.0
    };
    format!(
        r#"<div class="bar-row" style="animation-delay:{delay_ms}ms">
  <span class="bar-label" title="{le}">{le}</span>
  <span class="bar-track"><span class="bar-fill" style="width:{pct:.1}%;animation-delay:{delay_ms}ms"></span></span>
  <span class="bar-value">{val}<em>{sub}</em></span>
</div>"#,
        le = esc(label),
        val = util::human_bytes(bytes)
    )
}

fn hour_chart(hourly: &[(i64, u64)], from_ts: i64, now_ts: i64) -> String {
    let map: HashMap<i64, u64> = hourly.iter().copied().collect();
    let max = hourly.iter().map(|h| h.1).max().unwrap_or(0);
    let mut cols = String::new();
    let mut h = from_ts / 3600 * 3600;
    let end = now_ts / 3600 * 3600;
    let mut i = 0u64;
    while h <= end {
        let v = map.get(&h).copied().unwrap_or(0);
        let pct = if max > 0 {
            ((v as f64 / max as f64) * 100.0).max(if v > 0 { 4.0 } else { 0.0 })
        } else {
            0.0
        };
        let title = format!("{} — {}", fmt_dt(h), util::human_bytes(v));
        cols.push_str(&format!(
            r#"<div class="chart-col"><span class="chart-bar" style="height:{pct:.1}%;animation-delay:{delay}ms" title="{title}"></span><span class="chart-h">{hh}</span></div>"#,
            delay = i * 30,
            title = esc(&title),
            hh = fmt_hm(h).trim_end_matches(":00")
        ));
        h += 3600;
        i += 1;
    }
    cols
}

fn render(
    now: DateTime<Local>,
    from_ts: i64,
    totals: &Totals,
    files: &[TopRow],
    procs: &[ProcessRow],
    dirs: &[(String, u64, u64)],
    hourly: &[(i64, u64)],
    alerts: &[AlertRow],
    usn: &[UsnRow],
) -> String {
    let mut h = String::with_capacity(64 * 1024);
    h.push_str("<!DOCTYPE html>\n<html lang=\"zh-CN\">\n<head>\n<meta charset=\"UTF-8\">\n<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n<title>磁盘写入报告</title>\n<style>");
    h.push_str(crate::assets::report_style());
    h.push_str("</style>\n</head>\n<body>\n");
    h.push_str(r#"<div class="blobs"><div class="blob b1"></div><div class="blob b2"></div><div class="blob b3"></div></div>
<div class="wrap">
"#);

    h.push_str(&format!(
        r#"<header>
<h1>磁盘写入报告 · C 盘</h1>
<div class="subtitle">统计窗口 {} ~ {} · 报告生成于 {}</div>
</header>
"#,
        fmt_dt(from_ts),
        fmt_dt(now.timestamp()),
        now.format("%Y-%m-%d %H:%M")
    ));

    if totals.bytes == 0 && usn.is_empty() {
        h.push_str(r#"<section class="card"><div class="empty">统计窗口内没有达到入库阈值的写入记录。<br>请确认采集器（diskwatch collect）正在运行。</div></section>
"#);
    } else {
        let max_file = files.first();
        let max_file_val = max_file
            .map(|f| util::human_bytes(f.total))
            .unwrap_or_else(|| "-".to_string());
        let max_file_sub = max_file.map(|f| esc(&f.path)).unwrap_or_default();
        if totals.bytes > 0 {
            h.push_str(&format!(
                r#"<div class="cards">
<div class="card stat" style="animation-delay:0ms"><div class="stat-label">总写入量</div><div class="stat-value" data-count="{bytes}" data-fmt="bytes">0</div><div class="stat-sub">共 {ops} 次写入操作</div></div>
<div class="card stat" style="animation-delay:80ms"><div class="stat-label">写入文件数</div><div class="stat-value" data-count="{files}" data-fmt="int">0</div><div class="stat-sub">达到阈值的不同文件</div></div>
<div class="card stat" style="animation-delay:160ms"><div class="stat-label">活跃进程数</div><div class="stat-value" data-count="{procs}" data-fmt="int">0</div><div class="stat-sub">产生写入的进程</div></div>
<div class="card stat" style="animation-delay:240ms"><div class="stat-label">最大单文件</div><div class="stat-value">{maxv}</div><div class="stat-sub" title="{maxp}">{maxp}</div></div>
</div>
"#,
                bytes = totals.bytes,
                ops = totals.ops,
                files = totals.files,
                procs = totals.procs,
                maxv = max_file_val,
                maxp = max_file_sub
            ));
        }

        if !alerts.is_empty() {
            let mut alert_rows = String::new();
            for (i, a) in alerts.iter().take(30).enumerate() {
                alert_rows.push_str(&format!(
                    r#"<tr style="animation-delay:{delay}ms"><td class="time">{time}</td><td class="num alert-bytes">{bytes}</td><td class="proc" title="{procs}">{procs}</td><td class="path" title="{path}">{path}</td></tr>
"#,
                    delay = 100 + (i as u64) * 30,
                    time = fmt_dt(a.ts),
                    bytes = util::human_bytes(a.bytes),
                    procs = esc(&a.process),
                    path = esc(&a.path)
                ));
            }
            h.push_str(&format!(
                r#"<section class="card alerts" style="animation-delay:100ms">
<h2>大写入告警 <small>单分钟写入达到告警阈值 · {} 条</small></h2>
<table>
<thead><tr><th>时间</th><th>写入量</th><th>进程</th><th>文件路径</th></tr></thead>
<tbody>
{alert_rows}</tbody>
</table>
</section>
"#,
                alerts.len()
            ));
        }

        if totals.bytes > 0 {
            h.push_str(&format!(
                r#"<section class="card" style="animation-delay:120ms">
<h2>写入节奏 <small>按小时汇总</small></h2>
<div class="chart">{}</div>
</section>
"#,
                hour_chart(hourly, from_ts, now.timestamp())
            ));
        }

        if !usn.is_empty() {
            let mut sorted: Vec<&UsnRow> = usn.iter().collect();
            sorted.sort_by(|a, b| b.bytes.cmp(&a.bytes));
            let mut usn_rows = String::new();
            for (i, u) in sorted.iter().take(30).enumerate() {
                usn_rows.push_str(&format!(
                    r#"<tr style="animation-delay:{delay}ms"><td class="time">{time}</td><td class="num">{bytes}</td><td class="path" title="{path}">{path}</td></tr>
"#,
                    delay = 140 + (i as u64) * 30,
                    time = fmt_dt(u.ts),
                    bytes = util::human_bytes(u.bytes),
                    path = esc(&u.path)
                ));
            }
            h.push_str(&format!(
                r#"<section class="card usn" style="animation-delay:140ms">
<h2>离线补录 <small>停机期间改动 · 来自 NTFS USN 日志 · 按大小前 {} 条</small></h2>
<table>
<thead><tr><th>时间</th><th>大小</th><th>文件路径</th></tr></thead>
<tbody>
{usn_rows}</tbody>
</table>
<div class="usn-note">说明：补录数据来自离线期间的 NTFS USN 日志——大小为文件当前大小、路径为当前路径、写入进程未知，与实时采集数据分别统计。</div>
</section>
"#,
                usn.len().min(30)
            ));
        }

        if totals.bytes > 0 {
            let max_proc = procs.iter().map(|p| p.bytes).max().unwrap_or(0);
            let mut proc_rows = String::new();
            for (i, p) in procs.iter().enumerate() {
                proc_rows.push_str(&bar_row(
                    &p.process,
                    p.bytes,
                    max_proc,
                    &format!("{} 次 / {} 个文件", p.ops, p.files),
                    (i as u64) * 50,
                ));
            }
            h.push_str(&format!(
                r#"<section class="card" style="animation-delay:200ms">
<h2>写入最多的进程 <small>Top {}</small></h2>
{proc_rows}
</section>
"#,
                procs.len()
            ));

            let max_dir = dirs.iter().map(|d| d.1).max().unwrap_or(0);
            let mut dir_rows = String::new();
            for (i, (d, b, o)) in dirs.iter().enumerate() {
                dir_rows.push_str(&bar_row(
                    d,
                    *b,
                    max_dir,
                    &format!("{} 次写入", o),
                    (i as u64) * 50,
                ));
            }
            h.push_str(&format!(
                r#"<section class="card" style="animation-delay:280ms">
<h2>写入最多的目录 <small>Top {}</small></h2>
{dir_rows}
</section>
"#,
                dirs.len()
            ));

            let mut rows = String::new();
            for (i, f) in files.iter().take(30).enumerate() {
                let time = if f.last - f.first < 60 {
                    fmt_hm(f.first)
                } else {
                    format!("{}~{}", fmt_hm(f.first), fmt_hm(f.last))
                };
                rows.push_str(&format!(
                    r#"<tr style="animation-delay:{delay}ms"><td class="rank">{rank}</td><td class="num">{bytes}</td><td class="num">{ops}</td><td class="time">{time}</td><td class="proc" title="{procs}">{procs}</td><td class="path" title="{path}">{path}</td></tr>
"#,
                    delay = 300 + (i as u64) * 25,
                    rank = i + 1,
                    bytes = util::human_bytes(f.total),
                    ops = f.ops,
                    procs = esc(&f.procs),
                    path = esc(&f.path)
                ));
            }
            h.push_str(&format!(
                r#"<section class="card" style="animation-delay:360ms">
<h2>文件明细 <small>Top {}</small></h2>
<table>
<thead><tr><th>#</th><th>写入量</th><th>次数</th><th>时间</th><th>进程</th><th>文件路径</th></tr></thead>
<tbody>
{rows}</tbody>
</table>
</section>
"#,
                files.len().min(30)
            ));
        }
    }

    let source_note = if usn.is_empty() {
        "数据来源：ETW 实时采集（Microsoft-Windows-Kernel-File）· 仅统计达到入库阈值的写入"
    } else {
        "数据来源：ETW 实时采集（Microsoft-Windows-Kernel-File）+ NTFS USN 日志离线补录 · 仅统计达到入库阈值的写入"
    };
    h.push_str(&format!(
        r#"<footer>diskwatch · {source_note}</footer>
</div>
<script>{}</script>
</body>
</html>
"#,
        crate::assets::COUNTER_JS
    ));
    h
}
