use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::{params, Connection};

pub struct WriteRow {
    pub ts: i64,
    pub path: String,
    pub process: String,
    pub bytes: u64,
    pub ops: u64,
}

pub struct TopRow {
    pub path: String,
    pub total: u64,
    pub ops: u64,
    pub first: i64,
    pub last: i64,
    pub procs: String,
}

pub struct ProcessRow {
    pub process: String,
    pub bytes: u64,
    pub ops: u64,
    pub files: u64,
}

pub struct UsnRow {
    pub ts: i64,
    pub path: String,
    pub bytes: u64,
}

pub struct AlertRow {
    pub id: i64,
    pub ts: i64,
    pub path: String,
    pub process: String,
    pub bytes: u64,
    pub notified: i64,
}

pub struct Totals {
    pub bytes: u64,
    pub ops: u64,
    pub files: u64,
    pub procs: u64,
}

/// 时间窗，左闭右开 [from, to)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Range {
    pub from: i64,
    pub to: i64,
}

/// 聚合粒度：按文件 / 按所在目录 / 不聚合（明细行）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Group {
    File,
    Dir,
    Raw,
}

/// 排序键；不同视图/粒度下只有一部分可用，非法组合由 order_clause 落回默认列。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortKey {
    Bytes,
    Ops,
    Ts,
    TimeEnd,
    Path,
    Process,
    Notified,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Order {
    Asc,
    Desc,
}

/// 一次分页查询的全部输入。`q` 必须是**已转小写**的路径子串（SQL 侧对 path 做 lower()），
/// 空串表示不过滤；`process` 同理。
#[derive(Debug, Clone)]
pub struct PageQuery {
    pub range: Range,
    pub group: Group,
    pub sort: SortKey,
    pub order: Order,
    pub limit: i64,
    pub offset: i64,
    pub q: String,
    pub process: String,
    /// None = 不限；Some(0/1) = 只看未通知/已通知。
    pub notified: Option<i64>,
}

impl PageQuery {
    pub fn new(range: Range) -> Self {
        PageQuery {
            range,
            group: Group::File,
            sort: SortKey::Bytes,
            order: Order::Desc,
            limit: 50,
            offset: 0,
            q: String::new(),
            process: String::new(),
            notified: None,
        }
    }
}

/// 一页结果 + 总数。`truncated` 只在目录聚合的内存折叠触到行数上限时才可能为 true。
#[derive(Debug, Clone)]
pub struct Paged<T> {
    pub total: i64,
    pub limit: i64,
    pub offset: i64,
    pub has_more: bool,
    pub truncated: bool,
    pub rows: Vec<T>,
}

/// 写入按文件/按目录聚合行（目录行复用同一形状：path=目录、t0/t1=窗口内最早/最晚）。
#[derive(Debug, Clone)]
pub struct FileAggRow {
    pub path: String,
    pub bytes: u64,
    pub ops: u64,
    pub t0: i64,
    pub t1: i64,
    pub process: String,
}

#[derive(Debug, Clone)]
pub struct RawWriteRow {
    pub path: String,
    pub bytes: u64,
    pub ops: u64,
    pub ts: i64,
    pub process: String,
}

#[derive(Debug, Clone)]
pub struct UsnAggRow {
    pub path: String,
    pub bytes: u64,
    pub ts: i64,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct DbCounts {
    pub writes: i64,
    pub usn: i64,
    pub alerts: i64,
    pub bytes: u64,
}

pub struct Db(Connection);

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            if !dir.as_os_str().is_empty() {
                std::fs::create_dir_all(dir)
                    .with_context(|| format!("创建目录失败: {}", dir.display()))?;
            }
        }
        let conn = Connection::open(path)
            .with_context(|| format!("打开数据库失败: {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "busy_timeout", 5000)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS writes (
                 ts      INTEGER NOT NULL,
                 path    TEXT NOT NULL,
                 process TEXT NOT NULL,
                 bytes   INTEGER NOT NULL,
                 ops     INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_writes_ts ON writes(ts);
             CREATE TABLE IF NOT EXISTS meta (
                 key   TEXT PRIMARY KEY,
                 value TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS usn_changes (
                 ts    INTEGER NOT NULL,
                 path  TEXT NOT NULL,
                 bytes INTEGER NOT NULL,
                 UNIQUE(ts, path)
             );
             CREATE INDEX IF NOT EXISTS idx_usn_ts ON usn_changes(ts);
             CREATE TABLE IF NOT EXISTS alerts (
                 ts       INTEGER NOT NULL,
                 path     TEXT NOT NULL,
                 process  TEXT NOT NULL,
                 bytes    INTEGER NOT NULL,
                 notified INTEGER NOT NULL DEFAULT 0,
                 UNIQUE(ts, path)
             );",
        )?;
        Ok(Db(conn))
    }

    pub fn insert_writes(&mut self, rows: &[WriteRow]) -> Result<()> {
        let tx = self.0.transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO writes (ts, path, process, bytes, ops) VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;
            for r in rows {
                stmt.execute(params![r.ts, r.path, r.process, r.bytes as i64, r.ops as i64])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn cleanup_before(&mut self, ts: i64) -> Result<u64> {
        let n = self.0.execute("DELETE FROM writes WHERE ts < ?1", params![ts])?;
        self.0
            .execute("DELETE FROM usn_changes WHERE ts < ?1", params![ts])?;
        self.0.execute("DELETE FROM alerts WHERE ts < ?1", params![ts])?;
        Ok(n as u64)
    }

    pub fn top(&self, from_ts: i64, limit: usize, contains: Option<&str>) -> Result<Vec<TopRow>> {
        // ?2 用空串哨兵占位，保证有无关键字都走同一条语句、同一套参数位次。
        let mut stmt = self.0.prepare(
            "SELECT path, SUM(bytes), SUM(ops), MIN(ts), MAX(ts), GROUP_CONCAT(DISTINCT process)
             FROM writes
             WHERE ts >= ?1 AND (?2 = '' OR instr(path, ?2) > 0)
             GROUP BY path ORDER BY SUM(bytes) DESC LIMIT ?3",
        )?;
        let rows = stmt.query_map(
            params![from_ts, contains.unwrap_or(""), limit as i64],
            map_row,
        )?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
    }

    pub fn totals(&self, from_ts: i64) -> Result<Totals> {
        self.totals_in(Range { from: from_ts, to: i64::MAX }, "", "")
    }

    pub fn by_process(&self, from_ts: i64, limit: usize) -> Result<Vec<ProcessRow>> {
        self.by_process_in(Range { from: from_ts, to: i64::MAX }, "", "", limit)
    }

    pub fn by_hour(&self, from_ts: i64) -> Result<Vec<(i64, u64)>> {
        self.by_hour_in(Range { from: from_ts, to: i64::MAX }, "", "")
    }

    /// 窗口内总量。`q` 需已转小写（SQL 侧 lower(path)），空串 = 不过滤。
    pub fn totals_in(&self, r: Range, q: &str, process: &str) -> Result<Totals> {
        self.0
            .query_row(
                &format!(
                    "SELECT COALESCE(SUM(bytes), 0), COALESCE(SUM(ops), 0),
                            COUNT(DISTINCT path), COUNT(DISTINCT process)
                     FROM writes WHERE {W_WHERE}"
                ),
                params![r.from, r.to, q, process],
                |row| {
                    Ok(Totals {
                        bytes: row.get::<_, i64>(0)? as u64,
                        ops: row.get::<_, i64>(1)? as u64,
                        files: row.get::<_, i64>(2)? as u64,
                        procs: row.get::<_, i64>(3)? as u64,
                    })
                },
            )
            .map_err(Into::into)
    }

    /// 窗口内最大单文件（按 path 求和后的第一名），无数据时返回 None。
    pub fn max_file_in(&self, r: Range, q: &str, process: &str) -> Result<Option<(String, u64)>> {
        let mut stmt = self.0.prepare(&format!(
            "SELECT path, SUM(bytes) FROM writes WHERE {W_WHERE}
             GROUP BY path ORDER BY SUM(bytes) DESC LIMIT 1"
        ))?;
        let mut rows =
            stmt.query_map(params![r.from, r.to, q, process], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? as u64))
            })?;
        match rows.next() {
            Some(row) => Ok(Some(row?)),
            None => Ok(None),
        }
    }

    pub fn by_process_in(
        &self,
        r: Range,
        q: &str,
        process: &str,
        limit: usize,
    ) -> Result<Vec<ProcessRow>> {
        let mut stmt = self.0.prepare(&format!(
            "SELECT process, SUM(bytes), SUM(ops), COUNT(DISTINCT path)
             FROM writes WHERE {W_WHERE}
             GROUP BY process ORDER BY SUM(bytes) DESC LIMIT ?5"
        ))?;
        let rows = stmt.query_map(params![r.from, r.to, q, process, limit as i64], |row| {
            Ok(ProcessRow {
                process: row.get(0)?,
                bytes: row.get::<_, i64>(1)? as u64,
                ops: row.get::<_, i64>(2)? as u64,
                files: row.get::<_, i64>(3)? as u64,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
    }

    pub fn by_hour_in(&self, r: Range, q: &str, process: &str) -> Result<Vec<(i64, u64)>> {
        let mut stmt = self.0.prepare(&format!(
            "SELECT ts / 3600 * 3600 AS h, SUM(bytes)
             FROM writes WHERE {W_WHERE} GROUP BY h ORDER BY h"
        ))?;
        let rows = stmt.query_map(params![r.from, r.to, q, process], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)? as u64))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
    }

    // ---------- Web 分页查询层 ----------
    //
    // v1 不加新索引：instr() 用不上 B-tree，实测 ≈520 行/小时 → 30 天 ~37 万行，
    // instr 全扫 + GROUP BY 约 100–400ms 可接受。若日后变慢：
    //   进程过滤慢 → CREATE INDEX idx_writes_proc(process, ts)；
    //   聚合 p95 > 1s → 覆盖索引 (ts, path, bytes, ops)；
    //   再慢 → 落一张小时级 rollup 表。
    //
    // `offset` 越界**不做服务端钳制**：返回空页 + 真实 total，由前端回到第 1 页并改写 URL，
    // 这样分享链接不会悄悄落到另一页。

    /// writes 按文件聚合分页（与报告、CLI 同口径：GROUP BY path 后求和）。
    pub fn writes_by_file(&self, q: &PageQuery) -> Result<Paged<FileAggRow>> {
        let rows = self.writes_file_rows(q, q.limit, q.offset)?;
        let total = self.writes_file_total(q)?;
        Ok(Paged::build(q, total, rows, false))
    }

    /// writes 按所在目录聚合：全窗口文件行折到目录再排序分页。
    pub fn writes_by_dir(&self, q: &PageQuery) -> Result<Paged<FileAggRow>> {
        let src = self.writes_file_rows(q, DIR_FOLD_MAX, 0)?;
        let truncated = src.len() as i64 >= DIR_FOLD_MAX;
        let mut rows = fold_writes_dirs(src);
        sort_in_memory(&mut rows, q.sort, q.order);
        Ok(Paged::build(q, rows.len() as i64, slice_page(rows, q), truncated))
    }

    /// writes 分钟明细（一行一次落盘桶）。
    pub fn writes_raw(&self, q: &PageQuery) -> Result<Paged<RawWriteRow>> {
        let order = order_sql(raw_writes_col(q.sort), q.order);
        let mut stmt = self.0.prepare(&format!(
            "SELECT path, bytes, ops, ts, process FROM writes WHERE {W_WHERE}
             {order} LIMIT ?5 OFFSET ?6"
        ))?;
        let rows = stmt
            .query_map(
                params![q.range.from, q.range.to, q.q, q.process, q.limit, q.offset],
                map_raw_write,
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let total: i64 = self.0.query_row(
            &format!("SELECT COUNT(*) FROM writes WHERE {W_WHERE}"),
            params![q.range.from, q.range.to, q.q, q.process],
            |row| row.get(0),
        )?;
        Ok(Paged::build(q, total, rows, false))
    }

    /// usn 原始行：UNIQUE(ts, path) 本身就是行级。
    pub fn usn_rows(&self, q: &PageQuery) -> Result<Paged<UsnAggRow>> {
        let order = order_sql(usn_col(q.sort), q.order);
        let mut stmt = self.0.prepare(&format!(
            "SELECT path, bytes, ts FROM usn_changes WHERE {U_WHERE}
             {order} LIMIT ?4 OFFSET ?5"
        ))?;
        let rows = stmt
            .query_map(
                params![q.range.from, q.range.to, q.q, q.limit, q.offset],
                map_usn,
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let total: i64 = self.0.query_row(
            &format!("SELECT COUNT(*) FROM usn_changes WHERE {U_WHERE}"),
            params![q.range.from, q.range.to, q.q],
            |row| row.get(0),
        )?;
        Ok(Paged::build(q, total, rows, false))
    }

    /// usn 按目录聚合（大小/路径都是回填时的当前值，只作观测口）。
    pub fn usn_by_dir(&self, q: &PageQuery) -> Result<Paged<UsnAggRow>> {
        let mut stmt = self.0.prepare(&format!(
            "SELECT path, bytes, ts FROM usn_changes WHERE {U_WHERE} LIMIT ?4"
        ))?;
        let src = stmt
            .query_map(params![q.range.from, q.range.to, q.q, DIR_FOLD_MAX], map_usn)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let truncated = src.len() as i64 >= DIR_FOLD_MAX;
        let mut rows = fold_usn_dirs(src);
        sort_in_memory(&mut rows, q.sort, q.order);
        Ok(Paged::build(q, rows.len() as i64, slice_page(rows, q), truncated))
    }

    /// 告警行级分页；`notified` 三态见 A_WHERE 的 ?5 哨兵。
    pub fn page_alerts(&self, q: &PageQuery) -> Result<Paged<AlertRow>> {
        let order = order_sql(alert_col(q.sort), q.order);
        let notified = q.notified.unwrap_or(-1);
        let mut stmt = self.0.prepare(&format!(
            "SELECT rowid, ts, path, process, bytes, notified FROM alerts WHERE {A_WHERE}
             {order} LIMIT ?6 OFFSET ?7"
        ))?;
        let rows = stmt
            .query_map(
                params![q.range.from, q.range.to, q.q, q.process, notified, q.limit, q.offset],
                map_alert,
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let total: i64 = self.0.query_row(
            &format!("SELECT COUNT(*) FROM alerts WHERE {A_WHERE}"),
            params![q.range.from, q.range.to, q.q, q.process, notified],
            |row| row.get(0),
        )?;
        Ok(Paged::build(q, total, rows, false))
    }

    pub fn counts(&self) -> Result<DbCounts> {
        self.0
            .query_row(
                "SELECT (SELECT COUNT(*) FROM writes), (SELECT COUNT(*) FROM usn_changes),
                        (SELECT COUNT(*) FROM alerts),
                        (SELECT COALESCE(SUM(bytes), 0) FROM writes)",
                [],
                |row| {
                    Ok(DbCounts {
                        writes: row.get(0)?,
                        usn: row.get(1)?,
                        alerts: row.get(2)?,
                        bytes: row.get::<_, i64>(3)? as u64,
                    })
                },
            )
            .map_err(Into::into)
    }

    fn writes_file_rows(&self, q: &PageQuery, limit: i64, offset: i64) -> Result<Vec<FileAggRow>> {
        let order = order_sql(file_writes_col(q.sort), q.order);
        let mut stmt = self.0.prepare(&format!(
            "SELECT path, SUM(bytes) AS bytes, SUM(ops) AS ops, MIN(ts) AS t0, MAX(ts) AS t1,
                    GROUP_CONCAT(DISTINCT process) AS process
             FROM writes WHERE {W_WHERE} GROUP BY path
             {order} LIMIT ?5 OFFSET ?6"
        ))?;
        let rows = stmt
            .query_map(
                params![q.range.from, q.range.to, q.q, q.process, limit, offset],
                map_file_agg,
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    fn writes_file_total(&self, q: &PageQuery) -> Result<i64> {
        self.0
            .query_row(
                &format!("SELECT COUNT(DISTINCT path) FROM writes WHERE {W_WHERE}"),
                params![q.range.from, q.range.to, q.q, q.process],
                |row| row.get(0),
            )
            .map_err(Into::into)
    }

    pub fn meta_get(&self, key: &str) -> Result<Option<String>> {
        let mut stmt = self.0.prepare("SELECT value FROM meta WHERE key = ?1")?;
        let mut rows = stmt.query(params![key])?;
        match rows.next()? {
            Some(r) => Ok(Some(r.get(0)?)),
            None => Ok(None),
        }
    }

    pub fn meta_set(&mut self, key: &str, value: &str) -> Result<()> {
        self.0.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    /// 写入回填记录并同事务更新书签，保证“已处理到哪”与数据一致。
    pub fn insert_usn_changes(
        &mut self,
        rows: &[UsnRow],
        last_usn: u64,
        journal_id: u64,
    ) -> Result<()> {
        let tx = self.0.transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT OR IGNORE INTO usn_changes (ts, path, bytes) VALUES (?1, ?2, ?3)",
            )?;
            for r in rows {
                stmt.execute(params![r.ts, r.path, r.bytes as i64])?;
            }
            tx.execute(
                "INSERT INTO meta (key, value) VALUES ('last_usn', ?1)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![last_usn.to_string()],
            )?;
            tx.execute(
                "INSERT INTO meta (key, value) VALUES ('journal_id', ?1)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![journal_id.to_string()],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn usn_changes(&self, from_ts: i64) -> Result<Vec<UsnRow>> {
        let mut stmt = self
            .0
            .prepare("SELECT ts, path, bytes FROM usn_changes WHERE ts >= ?1 ORDER BY ts")?;
        let rows = stmt.query_map(params![from_ts], |r| {
            Ok(UsnRow {
                ts: r.get(0)?,
                path: r.get(1)?,
                bytes: r.get::<_, i64>(2)? as u64,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
    }

    /// 扫描写入表中“同一分钟桶累计达到阈值”的组合并记入 alerts。
    /// GROUP BY ts,path 使跨两次落盘的同一分钟桶也能正确求和；
    /// UNIQUE(ts,path) + OR IGNORE 保证重复扫描幂等。
    pub fn scan_alerts(&mut self, from_ts: i64, threshold: u64) -> Result<u64> {
        let n = self.0.execute(
            "INSERT OR IGNORE INTO alerts (ts, path, process, bytes)
             SELECT ts, path, GROUP_CONCAT(DISTINCT process), SUM(bytes)
             FROM writes WHERE ts >= ?1
             GROUP BY ts, path
             HAVING SUM(bytes) >= ?2",
            params![from_ts, threshold as i64],
        )?;
        Ok(n as u64)
    }

    pub fn pending_alerts(&self) -> Result<Vec<AlertRow>> {
        let mut stmt = self.0.prepare(
            "SELECT rowid, ts, path, process, bytes, notified
             FROM alerts WHERE notified = 0 ORDER BY ts",
        )?;
        let rows = stmt.query_map([], map_alert)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
    }

    pub fn alerts(&self, from_ts: i64) -> Result<Vec<AlertRow>> {
        let mut stmt = self.0.prepare(
            "SELECT rowid, ts, path, process, bytes, notified
             FROM alerts WHERE ts >= ?1 ORDER BY bytes DESC",
        )?;
        let rows = stmt.query_map(params![from_ts], map_alert)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
    }

    pub fn mark_notified(&mut self, ids: &[i64]) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        let marks = vec!["?"; ids.len()].join(",");
        let sql = format!("UPDATE alerts SET notified = 1 WHERE rowid IN ({marks})");
        let mut stmt = self.0.prepare(&sql)?;
        stmt.execute(rusqlite::params_from_iter(ids.iter().copied()))?;
        Ok(())
    }
}

impl<T> Paged<T> {
    fn build(q: &PageQuery, total: i64, rows: Vec<T>, truncated: bool) -> Self {
        let has_more = q.offset + (rows.len() as i64) < total;
        Paged {
            total,
            limit: q.limit,
            offset: q.offset,
            has_more,
            truncated,
            rows,
        }
    }
}

/// 目录折叠一次最多读的文件行数（30 天窗口内不同路径远小于此，触顶时 total 会偏小）。
const DIR_FOLD_MAX: i64 = 50_000;

/// 三个视图共用的过滤器，参数位次固定：?1 from ?2 to ?3 小写路径子串 ?4 进程。
/// 空串是“不过滤”哨兵，这样有无筛选都只有一条语句。
const W_WHERE: &str = "ts >= ?1 AND ts < ?2 AND (?3 = '' OR instr(lower(path), ?3) > 0) \
                       AND (?4 = '' OR process = ?4)";
/// usn 表没有进程列，过滤器到 ?3 为止。
const U_WHERE: &str = "ts >= ?1 AND ts < ?2 AND (?3 = '' OR instr(lower(path), ?3) > 0)";
/// alerts 的 process 是 GROUP_CONCAT 多值串，只能按子串命中；?5 = -1 表示 notified 不限。
const A_WHERE: &str = "ts >= ?1 AND ts < ?2 AND (?3 = '' OR instr(lower(path), ?3) > 0) \
                       AND (?4 = '' OR instr(process, ?4) > 0) AND (?5 < 0 OR notified = ?5)";

/// ORDER BY 只可能由静态表里的列别名和 ASC/DESC 拼成，用户输入进不来。
fn order_sql(col: &'static str, order: Order) -> String {
    let dir = match order {
        Order::Asc => "ASC",
        Order::Desc => "DESC",
    };
    format!("ORDER BY {col} {dir}, path ASC")
}

fn file_writes_col(sort: SortKey) -> &'static str {
    match sort {
        SortKey::Ops => "ops",
        SortKey::Ts => "t0",
        SortKey::TimeEnd => "t1",
        SortKey::Path => "path",
        SortKey::Process => "process",
        SortKey::Bytes | SortKey::Notified => "bytes",
    }
}

fn raw_writes_col(sort: SortKey) -> &'static str {
    match sort {
        SortKey::Ops => "ops",
        SortKey::Ts | SortKey::TimeEnd => "ts",
        SortKey::Path => "path",
        SortKey::Process => "process",
        SortKey::Bytes | SortKey::Notified => "bytes",
    }
}

fn usn_col(sort: SortKey) -> &'static str {
    match sort {
        SortKey::Path => "path",
        SortKey::Ts | SortKey::TimeEnd => "ts",
        SortKey::Bytes | SortKey::Ops | SortKey::Process | SortKey::Notified => "bytes",
    }
}

fn alert_col(sort: SortKey) -> &'static str {
    match sort {
        SortKey::Path => "path",
        SortKey::Process => "process",
        SortKey::Notified => "notified",
        SortKey::Ts | SortKey::TimeEnd => "ts",
        SortKey::Bytes | SortKey::Ops => "bytes",
    }
}

/// 内存排序用的取值接口：目录聚合折叠后只能在这里排序，SQL 侧够不着。
trait Sortable {
    fn bytes_key(&self) -> u64;
    fn ops_key(&self) -> u64;
    fn ts_key(&self) -> i64;
    fn end_key(&self) -> i64;
    fn path_key(&self) -> &str;
    fn proc_key(&self) -> &str;
    fn notified_key(&self) -> i64;
}

impl Sortable for FileAggRow {
    fn bytes_key(&self) -> u64 { self.bytes }
    fn ops_key(&self) -> u64 { self.ops }
    fn ts_key(&self) -> i64 { self.t0 }
    fn end_key(&self) -> i64 { self.t1 }
    fn path_key(&self) -> &str { &self.path }
    fn proc_key(&self) -> &str { &self.process }
    fn notified_key(&self) -> i64 { 0 }
}

impl Sortable for UsnAggRow {
    fn bytes_key(&self) -> u64 { self.bytes }
    fn ops_key(&self) -> u64 { 0 }
    fn ts_key(&self) -> i64 { self.ts }
    fn end_key(&self) -> i64 { self.ts }
    fn path_key(&self) -> &str { &self.path }
    fn proc_key(&self) -> &str { "" }
    fn notified_key(&self) -> i64 { 0 }
}

impl Sortable for AlertRow {
    fn bytes_key(&self) -> u64 { self.bytes }
    fn ops_key(&self) -> u64 { 0 }
    fn ts_key(&self) -> i64 { self.ts }
    fn end_key(&self) -> i64 { self.ts }
    fn path_key(&self) -> &str { &self.path }
    fn proc_key(&self) -> &str { &self.process }
    fn notified_key(&self) -> i64 { self.notified }
}

/// 主键相等时一律按 path 升序收尾，保证翻页稳定（与 SQL 侧的 tie-break 同口径）。
fn sort_in_memory<T: Sortable>(rows: &mut [T], sort: SortKey, order: Order) {
    rows.sort_by(|a, b| {
        let ord = match sort {
            SortKey::Bytes => a.bytes_key().cmp(&b.bytes_key()),
            SortKey::Ops => a.ops_key().cmp(&b.ops_key()),
            SortKey::Ts => a.ts_key().cmp(&b.ts_key()),
            SortKey::TimeEnd => a.end_key().cmp(&b.end_key()),
            SortKey::Path => a.path_key().cmp(b.path_key()),
            SortKey::Process => a.proc_key().cmp(b.proc_key()),
            SortKey::Notified => a.notified_key().cmp(&b.notified_key()),
        };
        let ord = match order {
            Order::Asc => ord,
            Order::Desc => ord.reverse(),
        };
        ord.then_with(|| a.path_key().cmp(b.path_key()))
    });
}

/// 越界 offset 得到空页（不是钳制到最后一页）。
fn slice_page<T>(rows: Vec<T>, q: &PageQuery) -> Vec<T> {
    rows.into_iter()
        .skip(q.offset.max(0) as usize)
        .take(q.limit.max(0) as usize)
        .collect()
}

/// 目录口径同 top.rs：最后一个反斜杠之前；盘符根下的文件归到 "C:\"。
fn parent_dir(path: &str) -> &str {
    match path.rfind('\\') {
        Some(i) if i > 2 => &path[..i],
        Some(2) => &path[..3],
        _ => path,
    }
}

fn fold_writes_dirs(rows: Vec<FileAggRow>) -> Vec<FileAggRow> {
    let mut map: BTreeMap<String, FileAggRow> = BTreeMap::new();
    let mut procs: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for r in rows {
        let dir = parent_dir(&r.path).to_string();
        let e = map.entry(dir.clone()).or_insert(FileAggRow {
            path: dir.clone(),
            bytes: 0,
            ops: 0,
            t0: r.t0,
            t1: r.t1,
            process: String::new(),
        });
        e.bytes += r.bytes;
        e.ops += r.ops;
        e.t0 = e.t0.min(r.t0);
        e.t1 = e.t1.max(r.t1);
        let set = procs.entry(dir).or_default();
        for p in r.process.split(',') {
            if !p.is_empty() {
                set.insert(p.to_string());
            }
        }
    }
    for (dir, e) in map.iter_mut() {
        e.process = procs
            .get(dir)
            .map(|s| s.iter().cloned().collect::<Vec<_>>().join(","))
            .unwrap_or_default();
    }
    map.into_values().collect()
}

fn fold_usn_dirs(rows: Vec<UsnAggRow>) -> Vec<UsnAggRow> {
    let mut map: BTreeMap<String, UsnAggRow> = BTreeMap::new();
    for r in rows {
        let dir = parent_dir(&r.path).to_string();
        let e = map.entry(dir.clone()).or_insert(UsnAggRow {
            path: dir,
            bytes: 0,
            ts: r.ts,
        });
        e.bytes += r.bytes;
        e.ts = e.ts.max(r.ts);
    }
    map.into_values().collect()
}

/// 小时序列补空：与报告 hour_chart 同一口径，返回 [from, to) 覆盖的每一个整点。
pub fn fill_hours(rows: &[(i64, u64)], r: Range) -> Vec<(i64, u64)> {
    let mut map: BTreeMap<i64, u64> = BTreeMap::new();
    for (h, b) in rows {
        *map.entry(*h).or_insert(0) += *b;
    }
    let start = r.from.div_euclid(3600) * 3600;
    let end = (r.to - 1).div_euclid(3600) * 3600;
    let mut out = Vec::new();
    let mut h = start;
    while h <= end {
        out.push((h, map.remove(&h).unwrap_or(0)));
        h += 3600;
    }
    out
}

/// GROUP_CONCAT(DISTINCT …) 给的是扫描顺序，换窗口/过滤条件就可能变，
/// 前端"首个进程 +N"的展示和按进程排序会因此抖动。这里统一成字典序去重。
/// 进程名不含逗号（Windows 文件名禁止），拆分是安全的。
fn sorted_procs(joined: String) -> String {
    if !joined.contains(',') {
        return joined;
    }
    let mut parts: Vec<&str> = joined.split(',').collect();
    parts.sort_unstable();
    parts.dedup();
    parts.join(",")
}

fn map_file_agg(row: &rusqlite::Row) -> rusqlite::Result<FileAggRow> {
    Ok(FileAggRow {
        path: row.get(0)?,
        bytes: row.get::<_, i64>(1)? as u64,
        ops: row.get::<_, i64>(2)? as u64,
        t0: row.get(3)?,
        t1: row.get(4)?,
        process: sorted_procs(row.get(5)?),
    })
}

fn map_raw_write(row: &rusqlite::Row) -> rusqlite::Result<RawWriteRow> {
    Ok(RawWriteRow {
        path: row.get(0)?,
        bytes: row.get::<_, i64>(1)? as u64,
        ops: row.get::<_, i64>(2)? as u64,
        ts: row.get(3)?,
        process: row.get(4)?,
    })
}

fn map_usn(row: &rusqlite::Row) -> rusqlite::Result<UsnAggRow> {
    Ok(UsnAggRow {
        path: row.get(0)?,
        bytes: row.get::<_, i64>(1)? as u64,
        ts: row.get(2)?,
    })
}

fn map_alert(r: &rusqlite::Row) -> rusqlite::Result<AlertRow> {
    Ok(AlertRow {
        id: r.get(0)?,
        ts: r.get(1)?,
        path: r.get(2)?,
        process: sorted_procs(r.get(3)?),
        bytes: r.get::<_, i64>(4)? as u64,
        notified: r.get(5)?,
    })
}

fn map_row(r: &rusqlite::Row) -> rusqlite::Result<TopRow> {
    Ok(TopRow {
        path: r.get(0)?,
        total: r.get::<_, i64>(1)? as u64,
        ops: r.get::<_, i64>(2)? as u64,
        first: r.get(3)?,
        last: r.get(4)?,
        procs: r.get(5)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_db(name: &str) -> (Db, std::path::PathBuf) {
        let dir = std::env::temp_dir().join("diskwatch-tests");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        let _ = std::fs::remove_file(&path);
        (Db::open(&path).unwrap(), path)
    }

    fn row(ts: i64, path: &str, proc: &str, bytes: u64, ops: u64) -> WriteRow {
        WriteRow {
            ts,
            path: path.to_string(),
            process: proc.to_string(),
            bytes,
            ops,
        }
    }

    #[test]
    fn insert_and_top_aggregates() {
        let (mut db, _p) = tmp_db("agg.db");
        db.insert_writes(&[
            row(1000, r"C:\a.log", "notepad.exe", 100, 1),
            row(1060, r"C:\a.log", "notepad.exe", 200, 2),
            row(1000, r"C:\big.bin", "svchost.exe", 5000, 1),
        ])
        .unwrap();

        let rows = db.top(0, 10, None).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].path, r"C:\big.bin");
        assert_eq!(rows[0].total, 5000);
        assert_eq!(rows[1].path, r"C:\a.log");
        assert_eq!(rows[1].total, 300);
        assert_eq!(rows[1].ops, 3);
        assert_eq!(rows[1].procs, "notepad.exe");
    }

    #[test]
    fn top_filters_by_time_and_substring() {
        let (mut db, _p) = tmp_db("filter.db");
        db.insert_writes(&[
            row(100, r"C:\old.log", "x.exe", 50, 1),
            row(2000, r"C:\keep.log", "y.exe", 60, 1),
            row(2000, r"D:\other.log", "z.exe", 70, 1),
        ])
        .unwrap();

        let rows = db.top(1000, 10, None).unwrap();
        assert_eq!(rows.len(), 2);

        let rows = db.top(0, 10, Some("keep")).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].path, r"C:\keep.log");
    }

    #[test]
    fn cleanup_removes_old_rows() {
        let (mut db, _p) = tmp_db("cleanup.db");
        db.insert_writes(&[row(100, r"C:\old", "x.exe", 1, 1), row(5000, r"C:\new", "x.exe", 1, 1)])
            .unwrap();
        assert_eq!(db.cleanup_before(1000).unwrap(), 1);
        assert_eq!(db.top(0, 10, None).unwrap().len(), 1);
    }

    #[test]
    fn scan_alerts_merges_minute_bucket_across_flushes() {
        const MB: u64 = 1024 * 1024;
        let (mut db, _p) = tmp_db("alerts-merge.db");
        // 同一分钟桶被两次落盘切开：60MB + 60MB 应合并为 120MB 越限
        db.insert_writes(&[row(6000, r"C:\big.bin", "a.exe", 60 * MB, 1)])
            .unwrap();
        assert_eq!(db.scan_alerts(0, 100 * MB).unwrap(), 0);
        db.insert_writes(&[row(6000, r"C:\big.bin", "a.exe", 60 * MB, 1)])
            .unwrap();
        assert_eq!(db.scan_alerts(0, 100 * MB).unwrap(), 1);
        let rows = db.alerts(0).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].bytes, 120 * MB);
        assert_eq!(rows[0].path, r"C:\big.bin");
    }

    #[test]
    fn scan_alerts_is_idempotent() {
        const MB: u64 = 1024 * 1024;
        let (mut db, _p) = tmp_db("alerts-dup.db");
        db.insert_writes(&[row(6000, r"C:\big.bin", "a.exe", 150 * MB, 1)])
            .unwrap();
        assert_eq!(db.scan_alerts(0, 100 * MB).unwrap(), 1);
        assert_eq!(db.scan_alerts(0, 100 * MB).unwrap(), 0);
        assert_eq!(db.alerts(0).unwrap().len(), 1);
    }

    #[test]
    fn pending_and_mark_notified_flow() {
        const MB: u64 = 1024 * 1024;
        let (mut db, _p) = tmp_db("alerts-notify.db");
        db.insert_writes(&[row(6000, r"C:\big.bin", "a.exe", 150 * MB, 1)])
            .unwrap();
        db.scan_alerts(0, 100 * MB).unwrap();
        let pending = db.pending_alerts().unwrap();
        assert_eq!(pending.len(), 1);
        db.mark_notified(&[pending[0].id]).unwrap();
        assert!(db.pending_alerts().unwrap().is_empty());
    }

    #[test]
    fn meta_roundtrip() {
        let (mut db, _p) = tmp_db("meta.db");
        assert_eq!(db.meta_get("last_usn").unwrap(), None);
        db.meta_set("last_usn", "12345").unwrap();
        assert_eq!(db.meta_get("last_usn").unwrap(), Some("12345".to_string()));
        db.meta_set("last_usn", "67890").unwrap();
        assert_eq!(db.meta_get("last_usn").unwrap(), Some("67890".to_string()));
    }

    #[test]
    fn usn_changes_insert_updates_bookmark_atomically() {
        let (mut db, _p) = tmp_db("usn.db");
        db.insert_usn_changes(
            &[
                UsnRow { ts: 100, path: r"C:\x.bin".into(), bytes: 500 },
                UsnRow { ts: 200, path: r"C:\y.bin".into(), bytes: 700 },
            ],
            4242,
            0xABCD,
        )
        .unwrap();
        let rows = db.usn_changes(0).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1].bytes, 700);
        assert_eq!(db.meta_get("last_usn").unwrap(), Some("4242".to_string()));
        assert_eq!(db.meta_get("journal_id").unwrap(), Some("43981".to_string()));
    }

    fn q_range(from: i64, to: i64) -> PageQuery {
        PageQuery::new(Range { from, to })
    }

    fn seeded(name: &str) -> Db {
        let (mut db, _p) = tmp_db(name);
        db.insert_writes(&[
            // a.log 两次共 200，b.bin 单次 150：按聚合值排序时 a.log 在前，
            // 若 SQL 把 ORDER BY 的别名解析成原始 bytes 列，b.bin 会跑到第一。
            row(1_000, r"C:\tmp\a.log", "notepad.exe", 100, 5),
            row(1_060, r"C:\tmp\a.log", "chrome.exe", 100, 1),
            row(1_120, r"C:\bin\b.bin", "svc.exe", 150, 9),
            row(1_180, r"C:\pagefile.sys", "System", 50, 4),
            row(2_000, r"C:\tmp\报表.xlsx", "excel.exe", 900, 2),
        ])
        .unwrap();
        db
    }

    #[test]
    fn writes_by_file_pages_with_true_total() {
        let db = seeded("page-file.db");
        let q = q_range(0, i64::MAX);
        let p1 = db.writes_by_file(&q).unwrap();
        assert_eq!(p1.total, 4);
        assert_eq!(p1.rows[0].path, r"C:\tmp\报表.xlsx");
        assert_eq!(p1.rows[0].bytes, 900);
        assert_eq!(p1.rows[1].path, r"C:\tmp\a.log");
        assert_eq!(p1.rows[1].bytes, 200, "聚合后 SUM(bytes) 决定顺序，不是单行 bytes");
        assert_eq!(p1.rows[1].ops, 6);
        assert_eq!(p1.rows[1].t0, 1_000);
        assert_eq!(p1.rows[1].t1, 1_060);
        assert_eq!(p1.rows[1].process, "chrome.exe,notepad.exe");

        let q2 = PageQuery { limit: 2, ..q_range(0, i64::MAX) };
        let page1 = db.writes_by_file(&q2).unwrap();
        assert_eq!(page1.rows.len(), 2);
        assert!(page1.has_more);
        let q3 = PageQuery { offset: 2, ..q2 };
        let page2 = db.writes_by_file(&q3).unwrap();
        assert_eq!(page2.rows.len(), 2);
        assert!(!page2.has_more);
        assert_eq!(
            page2.rows[0].path,
            r"C:\bin\b.bin",
            "第二页从第一名之后的那条接着排"
        );
        assert_eq!(page2.rows[1].path, r"C:\pagefile.sys");
    }

    #[test]
    fn writes_by_file_sort_ops_uses_aggregate() {
        let db = seeded("page-file-ops.db");
        let q = PageQuery { sort: SortKey::Ops, ..q_range(0, i64::MAX) };
        let rows = db.writes_by_file(&q).unwrap().rows;
        assert_eq!(rows[0].path, r"C:\bin\b.bin", "SUM(ops)=9 最大，不是单行 ops");
        assert_eq!(rows[0].ops, 9);
    }

    #[test]
    fn window_is_left_closed_right_open() {
        let db = seeded("page-window.db");
        let q = q_range(1_000, 1_120);
        let paths: Vec<String> = db
            .writes_by_file(&q)
            .unwrap()
            .rows
            .into_iter()
            .map(|r| r.path)
            .collect();
        assert_eq!(paths, vec![r"C:\tmp\a.log".to_string()], "from 含、to 不含");
        assert_eq!(db.writes_by_file(&q).unwrap().rows[0].t1, 1_060);

        let incl = q_range(1_000, 1_121);
        let paths: Vec<String> = db
            .writes_by_file(&incl)
            .unwrap()
            .rows
            .into_iter()
            .map(|r| r.path)
            .collect();
        assert!(paths.contains(&r"C:\bin\b.bin".to_string()), "to 之内即命中");
    }

    #[test]
    fn q_is_case_insensitive_and_matches_chinese() {
        let db = seeded("page-q.db");
        let up = PageQuery { q: "报表".into(), ..q_range(0, i64::MAX) };
        assert_eq!(db.writes_by_file(&up).unwrap().total, 1);

        // q 由调用方预先小写；库里存的是小写路径，用大写关键字也应命中。
        let mut db2_db = seeded("page-q2.db");
        db2_db
            .insert_writes(&[row(3_000, r"C:\Logs\Win.log", "w.exe", 10, 1)])
            .unwrap();
        let mixed = PageQuery { q: "win.log".into(), ..q_range(0, i64::MAX) };
        assert_eq!(db2_db.writes_by_file(&mixed).unwrap().total, 1);
        let ascii_upper = PageQuery { q: "WIN.LOG".into(), ..q_range(0, i64::MAX) };
        assert_eq!(db2_db.writes_by_file(&ascii_upper).unwrap().total, 0);
    }

    #[test]
    fn process_filter_is_exact_and_scopes_aggregates() {
        let db = seeded("page-proc.db");
        let q = PageQuery { process: "chrome.exe".into(), ..q_range(0, i64::MAX) };
        let p = db.writes_by_file(&q).unwrap();
        assert_eq!(p.total, 1);
        assert_eq!(p.rows[0].path, r"C:\tmp\a.log");
        assert_eq!(p.rows[0].bytes, 100, "只统计该进程的写入");
        assert_eq!(p.rows[0].process, "chrome.exe");
    }

    #[test]
    fn out_of_range_offset_returns_empty_page_with_total() {
        let db = seeded("page-offset.db");
        let q = PageQuery { offset: 9_999, ..q_range(0, i64::MAX) };
        let p = db.writes_by_file(&q).unwrap();
        assert!(p.rows.is_empty(), "越界不钳制，返回空页");
        assert_eq!(p.total, 4, "总数仍是真实值");
        assert!(!p.has_more);
    }

    #[test]
    fn writes_by_dir_folds_files_into_directories() {
        let db = seeded("page-dir.db");
        let q = PageQuery { group: Group::Dir, ..q_range(0, i64::MAX) };
        let p = db.writes_by_dir(&q).unwrap();
        assert_eq!(p.total, 3, "C:\\tmp / C:\\bin / C:\\ 之外没有别的目录");
        assert!(!p.truncated);
        let tmp = p.rows.iter().find(|r| r.path == r"C:\tmp").unwrap();
        assert_eq!(tmp.bytes, 1_100);
        assert_eq!(tmp.ops, 8);
        assert_eq!(tmp.t0, 1_000);
        assert_eq!(tmp.t1, 2_000);
        assert_eq!(tmp.process, "chrome.exe,excel.exe,notepad.exe");
        let mut dirs: Vec<&str> = p.rows.iter().map(|r| r.path.as_str()).collect();
        dirs.sort();
        assert_eq!(dirs, vec![r"C:\", r"C:\bin", r"C:\tmp"]);
    }

    #[test]
    fn writes_by_dir_pages_and_sorts() {
        let db = seeded("page-dir2.db");
        let q = PageQuery { group: Group::Dir, sort: SortKey::Ops, order: Order::Desc, limit: 2, ..q_range(0, i64::MAX) };
        let p = db.writes_by_dir(&q).unwrap();
        assert_eq!(p.total, 3);
        assert_eq!(p.rows[0].path, r"C:\bin", "按 ops 降序：C:\\bin=9 > C:\\tmp=8 > C:\\=4");
        assert_eq!(p.rows[0].ops, 9);
        assert_eq!(p.rows[1].path, r"C:\tmp");
        assert!(p.has_more);
        let asc = PageQuery { order: Order::Asc, ..q };
        assert_eq!(db.writes_by_dir(&asc).unwrap().rows[0].path, r"C:\");
    }

    #[test]
    fn parent_dir_rule_matches_cli() {
        assert_eq!(parent_dir(r"C:\a\b.log"), r"C:\a");
        assert_eq!(parent_dir(r"C:\pagefile.sys"), r"C:\");
        assert_eq!(parent_dir(r"D:\al\tools\x.txt"), r"D:\al\tools");
        assert_eq!(parent_dir("norelative"), "norelative");
        assert_eq!(parent_dir(r"C:\中文目录\报表.txt"), r"C:\中文目录");
    }

    #[test]
    fn writes_raw_rows_are_per_bucket() {
        let db = seeded("page-raw.db");
        let q = PageQuery { group: Group::Raw, sort: SortKey::Ts, order: Order::Desc, ..q_range(0, i64::MAX) };
        let p = db.writes_raw(&q).unwrap();
        assert_eq!(p.total, 5, "明细行数 = 落盘桶行数");
        assert_eq!(p.rows[0].path, r"C:\tmp\报表.xlsx");
        assert_eq!(p.rows[0].ts, 2_000);
        assert_eq!(p.rows[0].bytes, 900);
        let asc = PageQuery { order: Order::Asc, ..q };
        assert_eq!(db.writes_raw(&asc).unwrap().rows[0].ts, 1_000);
    }

    #[test]
    fn usn_rows_and_dir_fold() {
        let (mut db, _p) = tmp_db("page-usn.db");
        db.insert_usn_changes(
            &[
                UsnRow { ts: 100, path: r"C:\dir\x.bin".into(), bytes: 500 },
                UsnRow { ts: 200, path: r"C:\dir\y.bin".into(), bytes: 700 },
                UsnRow { ts: 300, path: r"C:\z.bin".into(), bytes: 900 },
            ],
            42,
            7,
        )
        .unwrap();
        let q = q_range(0, i64::MAX);
        let rows = db.usn_rows(&q).unwrap();
        assert_eq!(rows.total, 3);
        assert_eq!(rows.rows[0].bytes, 900);
        assert_eq!(rows.rows[0].path, r"C:\z.bin");

        let dq = PageQuery { group: Group::Dir, ..q };
        let dirs = db.usn_by_dir(&dq).unwrap();
        assert_eq!(dirs.total, 2);
        assert_eq!(dirs.rows[0].path, r"C:\dir");
        assert_eq!(dirs.rows[0].bytes, 1_200);
        assert_eq!(dirs.rows[0].ts, 200, "目录行 ts 取窗口内最晚");

        let wq = PageQuery { q: "y.bin".into(), ..q_range(0, i64::MAX) };
        assert_eq!(db.usn_rows(&wq).unwrap().total, 1);
    }

    #[test]
    fn alerts_notified_tri_state_and_process_substring() {
        let (mut db, _p) = tmp_db("page-alerts.db");
        let mb = 1024 * 1024;
        db.insert_writes(&[
            row(6_000, r"C:\big\a.bin", "a.exe,b.exe", 200 * mb, 1),
            row(6_060, r"C:\big\b.bin", "c.exe", 150 * mb, 1),
        ])
        .unwrap();
        db.scan_alerts(0, 100 * mb).unwrap();
        let pending = db.pending_alerts().unwrap();
        db.mark_notified(&[pending[0].id]).unwrap();

        // 故意让"最大"与"最新"落在不同行上：ORDER BY 的列一旦接错，这两条就会互相打脸。
        let all = db.page_alerts(&q_range(0, i64::MAX)).unwrap();
        assert_eq!(all.total, 2);
        assert_eq!(all.rows[0].bytes, 200 * mb);
        assert_eq!(all.rows[0].path, r"C:\big\a.bin", "默认按写入量降序");
        assert_eq!(all.rows[0].notified, 1, "mark 掉的正是这条最早的");
        let by_ts = PageQuery { sort: SortKey::Ts, ..q_range(0, i64::MAX) };
        assert_eq!(
            db.page_alerts(&by_ts).unwrap().rows[0].path,
            r"C:\big\b.bin",
            "按时间降序应是较新那条"
        );
        let only_new = PageQuery { notified: Some(1), ..q_range(0, i64::MAX) };
        assert_eq!(db.page_alerts(&only_new).unwrap().total, 1);
        let only_old = PageQuery { notified: Some(0), ..q_range(0, i64::MAX) };
        assert_eq!(db.page_alerts(&only_old).unwrap().total, 1);
        // process 是多值串，按子串命中
        let by_proc = PageQuery { process: "b.exe".into(), ..q_range(0, i64::MAX) };
        let hit = db.page_alerts(&by_proc).unwrap();
        assert_eq!(hit.total, 1);
        assert_eq!(hit.rows[0].path, r"C:\big\a.bin");
        let by_sort = PageQuery { sort: SortKey::Notified, ..q_range(0, i64::MAX) };
        assert_eq!(db.page_alerts(&by_sort).unwrap().rows.len(), 2);
    }

    #[test]
    fn every_sort_key_and_order_is_accepted() {
        let db = seeded("page-sort.db");
        let keys = [
            SortKey::Bytes,
            SortKey::Ops,
            SortKey::Ts,
            SortKey::TimeEnd,
            SortKey::Path,
            SortKey::Process,
            SortKey::Notified,
        ];
        for (i, k) in keys.iter().enumerate() {
            for order in [Order::Asc, Order::Desc] {
                let q = PageQuery { sort: *k, order, limit: 3, offset: i as i64, ..q_range(0, i64::MAX) };
                assert_eq!(db.writes_by_file(&q).unwrap().total, 4);
                db.writes_by_file(&q).unwrap();
                db.writes_by_dir(&q).unwrap();
                db.writes_raw(&q).unwrap();
                db.page_alerts(&q).unwrap();
                db.usn_rows(&q).unwrap();
                db.usn_by_dir(&q).unwrap();
            }
        }
    }

    #[test]
    fn report_forwarders_match_windowed_variants() {
        let db = seeded("page-forward.db");
        let open = Range { from: 1_050, to: i64::MAX };
        let fwd = db.totals(1_050).unwrap();
        let win = db.totals_in(open, "", "").unwrap();
        assert_eq!(
            (fwd.bytes, fwd.ops, fwd.files, fwd.procs),
            (win.bytes, win.ops, win.files, win.procs),
            "totals 转发后逐字段一致"
        );
        assert_eq!(
            db.by_hour(1_050).unwrap(),
            db.by_hour_in(open, "", "").unwrap()
        );
        let fwd = db.by_process(1_050, 2).unwrap();
        let win = db.by_process_in(open, "", "", 2).unwrap();
        assert_eq!(
            (fwd[0].process.clone(), fwd[0].bytes, fwd[0].ops, fwd[0].files),
            (win[0].process.clone(), win[0].bytes, win[0].ops, win[0].files)
        );
        assert_eq!(fwd.len(), 2, "by_process 的 LIMIT 是绑定进去的");
    }

    #[test]
    fn max_file_counts_and_fill_hours() {
        let mut db = seeded("page-misc.db");
        let all = q_range(0, i64::MAX);
        let top = db.max_file_in(all.range, "", "").unwrap();
        assert_eq!(top, Some((r"C:\tmp\报表.xlsx".to_string(), 900)));
        let filtered = db.max_file_in(all.range, "a.log", "").unwrap();
        assert_eq!(filtered, Some((r"C:\tmp\a.log".to_string(), 200)));
        assert_eq!(db.max_file_in(all.range, "zzz", "").unwrap(), None);

        let c = db.counts().unwrap();
        assert_eq!((c.writes, c.usn, c.alerts), (5, 0, 0));
        assert_eq!(c.bytes, 1_300);

        db.insert_writes(&[row(4_000, r"C:\other.log", "o.exe", 350, 1)])
            .unwrap();
        let hours = db.by_hour_in(all.range, "", "").unwrap();
        assert_eq!(hours, vec![(0, 1_300), (3_600, 350)]);
        let filled = fill_hours(&hours, Range { from: 900, to: 11_000 });
        assert_eq!(
            filled,
            vec![(0, 1_300), (3_600, 350), (7_200, 0), (10_800, 0)],
            "缺口小时补 0，柱图横轴连续"
        );
        assert_eq!(fill_hours(&[], Range { from: 0, to: 1 }).len(), 1);
    }
}
