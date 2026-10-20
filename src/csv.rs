// CSV 导出：只负责把行序列化成表格，查询与聚合口径全部来自 db.rs。
// 带 UTF-8 BOM 是因为 Excel 只会按 ANSI 猜测无 BOM 的中文；字节数保持原始整数，
// 让 Excel 能直接排序和求和，而不是导入已格式化的 "1.5 GB" 文本。

use std::io::{self, Write};

use chrono::{Local, TimeZone};

use crate::db::{AlertRow, FileAggRow, RawWriteRow, UsnAggRow};
use crate::footprint::{self, Usage};

const BOM: &[u8] = &[0xEF, 0xBB, 0xBF];

/// `us` 与 `rows` 按序号对齐；缺项（未测、或调用方跳过扫描）写空字节数 + 「未测」，
/// 表头恒定不变，Excel 模板不会因为一次 --no-usage 就错位。
pub fn writes_csv(w: &mut dyn Write, rows: &[FileAggRow], us: &[Usage]) -> io::Result<()> {
    w.write_all(BOM)?;
    writeln!(
        w,
        "时间范围起,时间范围止,文件路径,字节数,操作次数,进程,当前占用字节,占用状态"
    )?;
    for (i, r) in rows.iter().enumerate() {
        let u = us.get(i).filter(|u| u.state != footprint::State::Unmeasured);
        let (usage_bytes, usage_state) = match u {
            Some(u) => (u.bytes.to_string(), u.state.label().to_string()),
            None => (String::new(), "未测".to_string()),
        };
        writeln!(
            w,
            "{},{},{},{},{},{},{},{}",
            fmt_ts(r.t0),
            fmt_ts(r.t1),
            esc(&r.path),
            r.bytes,
            r.ops,
            esc(&r.process),
            usage_bytes,
            usage_state
        )?;
    }
    Ok(())
}

pub fn writes_raw_csv(w: &mut dyn Write, rows: &[RawWriteRow]) -> io::Result<()> {
    w.write_all(BOM)?;
    writeln!(w, "时间,文件路径,字节数,操作次数,进程")?;
    for r in rows {
        writeln!(
            w,
            "{},{},{},{},{}",
            fmt_ts(r.ts),
            esc(&r.path),
            r.bytes,
            r.ops,
            esc(&r.process)
        )?;
    }
    Ok(())
}

pub fn usn_csv(w: &mut dyn Write, rows: &[UsnAggRow]) -> io::Result<()> {
    w.write_all(BOM)?;
    writeln!(w, "时间,路径,字节数")?;
    for r in rows {
        writeln!(w, "{},{},{}", fmt_ts(r.ts), esc(&r.path), r.bytes)?;
    }
    Ok(())
}

pub fn alerts_csv(w: &mut dyn Write, rows: &[AlertRow]) -> io::Result<()> {
    w.write_all(BOM)?;
    writeln!(w, "时间,路径,进程,字节数,已通知")?;
    for r in rows {
        writeln!(
            w,
            "{},{},{},{},{}",
            fmt_ts(r.ts),
            esc(&r.path),
            esc(&r.process),
            r.bytes,
            if r.notified != 0 { "是" } else { "否" }
        )?;
    }
    Ok(())
}

fn fmt_ts(ts: i64) -> String {
    Local
        .timestamp_opt(ts, 0)
        .single()
        .map(|dt| dt.format("%Y-%m-%d %H:%M:%S").to_string())
        .unwrap_or_default()
}

fn esc(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(buf: &[u8]) -> String {
        assert!(buf.starts_with(BOM), "Excel 需要 BOM 才认中文");
        String::from_utf8(buf[BOM.len()..].to_vec()).unwrap()
    }

    #[test]
    fn esc_leaves_plain_paths() {
        assert_eq!(esc(r"C:\foo\bar.txt"), r"C:\foo\bar.txt");
        assert_eq!(esc(r"D:\报表\数据.xlsx"), r"D:\报表\数据.xlsx");
    }

    #[test]
    fn esc_quotes_comma_and_newline() {
        assert_eq!(esc("a,b"), "\"a,b\"");
        assert_eq!(esc("a\nb"), "\"a\nb\"");
    }

    #[test]
    fn esc_doubles_inner_quote() {
        assert_eq!(esc(r#"a"b"#), "\"a\"\"b\"");
    }

    #[test]
    fn headers_present_with_no_rows() {
        let mut v = Vec::new();
        writes_csv(&mut v, &[], &[]).unwrap();
        assert_eq!(body(&v).lines().count(), 1);
        assert!(body(&v).starts_with(
            "时间范围起,时间范围止,文件路径,字节数,操作次数,进程,当前占用字节,占用状态"
        ));

        let mut v = Vec::new();
        writes_raw_csv(&mut v, &[]).unwrap();
        assert!(body(&v).starts_with("时间,文件路径,字节数,操作次数,进程"));

        let mut v = Vec::new();
        usn_csv(&mut v, &[]).unwrap();
        assert!(body(&v).starts_with("时间,路径,字节数"));

        let mut v = Vec::new();
        alerts_csv(&mut v, &[]).unwrap();
        assert!(body(&v).starts_with("时间,路径,进程,字节数,已通知"));
    }

    #[test]
    fn writes_row_keeps_bytes_as_raw_integer() {
        let rows = [FileAggRow {
            path: r"D:\报表\数据.xlsx".to_string(),
            bytes: 1024,
            ops: 3,
            t0: 1_700_000_000,
            t1: 1_700_000_060,
            process: "excel.exe".to_string(),
        }];
        let us = [Usage {
            bytes: 512,
            files: 1,
            state: footprint::State::Exact,
        }];
        let mut v = Vec::new();
        writes_csv(&mut v, &rows, &us).unwrap();
        let text = body(&v);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[1].contains(r"D:\报表\数据.xlsx"));
        assert!(lines[1].contains(",1024,3,"), "字节数不能预格式化");
        assert!(lines[1].ends_with(",512,完整"), "现值也要原始字节数");
        assert!(lines[1].starts_with("2023-11-"), "时间要是本地时区可读串");
    }

    #[test]
    fn unmeasured_row_leaves_usage_blank_not_zero() {
        let rows = [FileAggRow {
            path: r"C:\a.log".to_string(),
            bytes: 1024,
            ops: 3,
            t0: 1,
            t1: 1,
            process: "a.exe".to_string(),
        }];
        let mut v = Vec::new();
        writes_csv(&mut v, &rows, &[]).unwrap();
        assert!(body(&v).lines().nth(1).unwrap().ends_with(",未测"));

        // 已删除是真值，不是未测：这两格必须能区分开
        let deleted = [Usage {
            bytes: 0,
            files: 0,
            state: footprint::State::Missing,
        }];
        let mut v = Vec::new();
        writes_csv(&mut v, &rows, &deleted).unwrap();
        assert!(body(&v).lines().nth(1).unwrap().ends_with(",0,已删除"));
    }

    #[test]
    fn alerts_maps_notified_to_chinese_flag() {
        let rows = [AlertRow {
            id: 1,
            ts: 1_700_000_000,
            path: r"C:\a,b.log".to_string(),
            process: "a.exe,b.exe".to_string(),
            bytes: 9,
            notified: 1,
        }];
        let mut v = Vec::new();
        alerts_csv(&mut v, &rows).unwrap();
        let text = body(&v);
        assert!(text.contains("\"C:\\a,b.log\""), "含逗号的路径必须加引号");
        assert!(text.ends_with(",是\n"));
    }
}
