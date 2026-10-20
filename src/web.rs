// 四期：内置 Web 界面。只监听 127.0.0.1、只读 GET、无鉴权——同机任意进程都能读到数据，
// 这是本机诊断工具的既定取舍，不做端口对外暴露。
//
// tiny_http 0.12.0 已核对源码，三处与常见假设不同，改动前请先看：
//   1) 没有 `exit_now()`。停机靠 `recv_timeout(1s)` 轮询停止位，随后 drop Server ——
//      Server::drop 会向自己 connect 一次唤醒 accept 线程，无需额外唤醒线程。
//   2) `Request::url()` 返回含 query 的完整 URI（没有 path()/uri()）。
//   3) `Response` 没有 `html()` 构造器，Content-Type 用 `with_header` 显式带 charset；
//      `add_header` 对同名 Content-Type 是覆盖语义，不会出现重复头。
// 另外 `Server::num_connections()` 是 `unimplemented!()`，绝不调用。
//
// 并发模型：Server 内部每连接一个池线程负责读请求，请求排队后由本线程**串行**处理。
// 因此没有可靠的"在途请求数"可查，也不做在途限流：慢查询只会堵住它后面的请求。
// 也正因为只有一条处理路径，Db 连接由 serve 独占持有、查询全是 &self，不需要 Mutex 包装。

use std::collections::HashMap;
use std::io::Cursor;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use chrono::Local;
use tiny_http::{Header, Method, Request, Response, Server};

use crate::db::{self, AlertRow, Db, FileAggRow, Group, Order, PageQuery, Paged, Range, RawWriteRow, SortKey, UsnAggRow};
use crate::export;
use crate::footprint::{self, Usage};
use crate::log;

/// 服务态硬编码端口（binPath 参数化留五期）。
pub const DEFAULT_PORT: u16 = 8421;

const POLL: Duration = Duration::from_secs(1);
/// 现值缓存存活期：自动刷新会反复量同一批目录，而目录占用秒级内不会变。
const FOOTPRINT_TTL: Duration = Duration::from_secs(60);
/// 单个请求最多量多少个路径。目录要递归，翻页到 500 行时全量扫描会把串行队列堵死；
/// 超出的行返回未测，页面显示为「—」。
const FOOTPRINT_MAX_ROWS: usize = 80;

type Body = Response<Cursor<Vec<u8>>>;

/// 在调用线程上阻塞提供 Web 服务，直到停止位置位。
pub fn serve(db_path: &Path, port: u16, stop: Arc<AtomicBool>) -> Result<()> {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let server = Server::http(addr).map_err(|e| anyhow!("监听 {addr} 失败: {e}"))?;
    let db = Db::open(db_path)?;
    log::info(format!(
        "Web 已监听 http://127.0.0.1:{port}/ · 数据 {}",
        db_path.display()
    ));

    let ctx = Ctx {
        port,
        started: Instant::now(),
        db,
        foot: footprint::Cache::new(FOOTPRINT_TTL),
    };
    loop {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        match server.recv_timeout(POLL) {
            Ok(Some(req)) => route(&ctx, req),
            Ok(None) => continue,
            Err(e) => log::warn(format!("Web 接收请求失败: {e}")),
        }
    }
    // drop(server) 唤醒 accept 线程，端口随之释放。
    log::info("Web 已停止监听");
    Ok(())
}

struct Ctx {
    port: u16,
    started: Instant,
    db: Db,
    /// 当前磁盘占用缓存；只在 usage=1 的请求里被填，未开启时连扫都不扫。
    foot: footprint::Cache,
}

/// 取一页排行的现值，与 rows 按序号对齐。`usage=false` 或未测到的尾部一律 Unmeasured，
/// JSON 里挂 null（见 footprint::attach_usage），前端渲染成「—」而不是 0。
fn usage_of(ctx: &Ctx, paths: &[String], usage: bool) -> Vec<Usage> {
    if !usage || paths.is_empty() {
        return Vec::new();
    }
    let head = &paths[..paths.len().min(FOOTPRINT_MAX_ROWS)];
    let mut us = ctx.foot.of_paths(head, footprint::Budget::WEB);
    us.resize(paths.len(), Usage::default());
    us
}

fn route(ctx: &Ctx, req: Request) {
    let url = req.url().to_owned();
    let (path, query) = match url.split_once('?') {
        Some((p, q)) => (p, q),
        None => (url.as_str(), ""),
    };

    if !host_is_allowed(&req) {
        return reply(req, error(400, "bad_host", "Host 头不合法"));
    }
    if req.method() != &Method::Get {
        let resp = error(405, "method_not_allowed", "只支持 GET")
            .with_header(hdr(&b"Allow"[..], &b"GET"[..]));
        return reply(req, resp);
    }

    if path == "/api/ping" {
        return reply(
            req,
            json(
                200,
                serde_json::json!({
                    "ok": true,
                    "version": env!("CARGO_PKG_VERSION"),
                    "port": ctx.port,
                    "uptime_secs": ctx.started.elapsed().as_secs(),
                }),
            ),
        );
    }
    if path.starts_with("/api/") {
        let params = parse_query_str(query);
        return reply(req, api(ctx, path, &params));
    }

    let resp = match path {
        "/favicon.ico" => Response::from_data(Vec::new()).with_status_code(204),
        _ => match static_asset(path) {
            Some((body, mime)) => Response::from_data(body.as_bytes().to_vec())
                .with_header(hdr(&b"Content-Type"[..], mime.as_bytes()))
                .with_header(hdr(&b"Cache-Control"[..], &b"no-cache"[..])),
            None => error(404, "not_found", "未知路径"),
        },
    };
    reply(req, resp);
}

fn reply(req: Request, resp: Body) {
    if let Err(e) = req.respond(resp) {
        log::warn(format!("Web 响应写回失败: {e}"));
    }
}

fn static_asset(path: &str) -> Option<(&'static str, &'static str)> {
    let (body, mime) = match path {
        "/" | "/index.html" => (crate::assets::INDEX_HTML, "text/html; charset=utf-8"),
        "/glass.css" => (crate::assets::GLASS_CSS, "text/css; charset=utf-8"),
        "/web.css" => (crate::assets::WEB_CSS, "text/css; charset=utf-8"),
        "/app.js" => (crate::assets::APP_JS, "text/javascript; charset=utf-8"),
        _ => return None,
    };
    Some((body, mime))
}

// ---------- /api/*：只读查询，响应必须逐键对齐 assets/app.js 的渲染层 ----------

type Params = HashMap<String, String>;

/// 超过 40 天的窗口一律拒绝：数据本身只留 30 天，更长只会让 instr 全扫白花时间在空区。
const MAX_WINDOW_SECS: i64 = 40 * 86_400;
const MAX_OFFSET: i64 = 500_000;
const MAX_Q_BYTES: usize = 512;
const MAX_PROC_BYTES: usize = 260;

#[derive(Debug)]
enum ApiError {
    Bad(String),
    Internal(String),
}

fn bad(msg: impl Into<String>) -> ApiError {
    ApiError::Bad(msg.into())
}

fn internal(e: impl std::fmt::Display) -> ApiError {
    ApiError::Internal(e.to_string())
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Writes,
    Usn,
    Alerts,
    /// overview / processes：只要时间窗 + 关键字，不碰排序分页。
    Window,
}

struct Args {
    range: Range,
    last: Option<String>,
    group: Group,
    sort: SortKey,
    order: Order,
    limit: i64,
    offset: i64,
    q: String,
    process: String,
    notified: Option<i64>,
    /// usage=1 才测量当前磁盘占用：默认关闭，避免每次自动刷新都去扫盘。
    usage: bool,
}

fn api(ctx: &Ctx, path: &str, p: &Params) -> Body {
    if let Some(slug) = path.strip_prefix("/api/export/") {
        return match export::View::parse(slug) {
            Some(view) => respond(path, api_export(ctx, view, p)),
            None => error(404, "not_found", &format!("未知接口 {path}")),
        };
    }
    let r = match path {
        "/api/overview" => api_overview(ctx, p),
        "/api/writes" => api_writes(ctx, p),
        "/api/usn" => api_usn(ctx, p),
        "/api/alerts" => api_alerts(ctx, p),
        "/api/processes" => api_processes(ctx, p),
        "/api/status" => api_status(ctx),
        other => return error(404, "not_found", &format!("未知接口 {other}")),
    };
    respond(path, r)
}

fn respond(path: &str, r: Result<Body, ApiError>) -> Body {
    match r {
        Ok(body) => body,
        Err(ApiError::Bad(msg)) => error(400, "bad_request", &msg),
        Err(ApiError::Internal(msg)) => {
            // 库里真实的错只进日志，响应只给类别，细节不外泄给页面。
            log::warn(format!("Web 查询失败 {path}: {msg}"));
            error(500, "internal", "查询失败，详见 diskwatch.log")
        }
    }
}

/// 导出当前视图的全量 CSV。参数校验完全复用 Args::parse，
/// 排序/分页字段被忽略：导出给 Excel 的是整个窗口，不是屏幕上的 50 行。
fn api_export(ctx: &Ctx, view: export::View, p: &Params) -> Result<Body, ApiError> {
    let kind = match view {
        export::View::Writes => Kind::Writes,
        export::View::Usn => Kind::Usn,
        export::View::Alerts => Kind::Alerts,
    };
    if kind == Kind::Usn && !p.get("process").map(|s| s.is_empty()).unwrap_or(true) {
        return Err(bad("离线补录数据没有进程维度，不支持 process 参数"));
    }
    let a = Args::parse(p, kind)?;
    let buf =
        export::build(&ctx.db, view, a.range, &a.q, &a.process, a.group, a.usage).map_err(internal)?;
    let name = export::filename(view, a.range);
    // 文件名由服务端按视图+窗口日期生成，不含用户输入，无需转义。
    let disposition = format!("attachment; filename=\"{name}\"");
    Ok(Response::from_data(buf)
        .with_header(hdr(&b"Content-Type"[..], &b"text/csv; charset=utf-8"[..]))
        .with_header(hdr(
            &b"Content-Disposition"[..],
            disposition.as_bytes(),
        )))
}

fn api_writes(ctx: &Ctx, p: &Params) -> Result<Body, ApiError> {
    let a = Args::parse(p, Kind::Writes)?;
    let q = a.page_query();
    let (page, rows) = match a.group {
        Group::File => {
            let r = ctx.db.writes_by_file(&q).map_err(internal)?;
            let us = usage_of(ctx, &paths_of(&r.rows), a.usage);
            (page_json(&r), file_json(&r.rows, &us))
        }
        Group::Dir => {
            let r = ctx.db.writes_by_dir(&q).map_err(internal)?;
            let us = usage_of(ctx, &paths_of(&r.rows), a.usage);
            (page_json(&r), file_json(&r.rows, &us))
        }
        Group::Raw => {
            // 分钟明细同一路径会重复出现，逐行再扫一次只是把重复摊到每次扫描上
            let r = ctx.db.writes_raw(&q).map_err(internal)?;
            (page_json(&r), raw_json(&r.rows))
        }
    };
    Ok(envelope(a.echo_json(), page, rows))
}

fn api_usn(ctx: &Ctx, p: &Params) -> Result<Body, ApiError> {
    // 回填数据来自 USN 日志，只有"路径 + 大小 + 时间"三个维度，进程未知，不接受进程过滤。
    if !p.get("process").map(|s| s.is_empty()).unwrap_or(true) {
        return Err(bad("离线补录数据没有进程维度，不支持 process 参数"));
    }
    let a = Args::parse(p, Kind::Usn)?;
    let q = a.page_query();
    let r = if a.group == Group::Dir {
        ctx.db.usn_by_dir(&q).map_err(internal)?
    } else {
        ctx.db.usn_rows(&q).map_err(internal)?
    };
    Ok(envelope(a.echo_json(), page_json(&r), usn_json(&r.rows)))
}

fn api_alerts(ctx: &Ctx, p: &Params) -> Result<Body, ApiError> {
    let a = Args::parse(p, Kind::Alerts)?;
    let r = ctx.db.page_alerts(&a.page_query()).map_err(internal)?;
    Ok(envelope(a.echo_json(), page_json(&r), alert_json(&r.rows)))
}

fn api_overview(ctx: &Ctx, p: &Params) -> Result<Body, ApiError> {
    let a = Args::parse(p, Kind::Window)?;
    let r = a.range;
    let totals = ctx.db.totals_in(r, &a.q, &a.process).map_err(internal)?;
    let max = ctx.db.max_file_in(r, &a.q, &a.process).map_err(internal)?;
    let hours = ctx.db.by_hour_in(r, &a.q, &a.process).map_err(internal)?;
    let procs = ctx
        .db
        .by_process_in(r, &a.q, &a.process, 10)
        .map_err(internal)?;
    let mut dq = a.page_query();
    dq.group = Group::Dir;
    dq.limit = 8;
    dq.sort = SortKey::Bytes;
    dq.order = Order::Desc;
    let dirs = ctx.db.writes_by_dir(&dq).map_err(internal)?;
    // 目录卡片是"该去清哪里"的第一眼依据，光有写入量会把已删除的临时文件也排进前三
    let dir_usage = usage_of(ctx, &paths_of(&dirs.rows), a.usage);
    let hourly: Vec<serde_json::Value> = db::fill_hours(&hours, r)
        .into_iter()
        .map(|(h, b)| serde_json::json!([h, b]))
        .collect();
    Ok(json(
        200,
        serde_json::json!({
            "ok": true,
            "query": a.echo_json(),
            "totals": {
                "bytes": totals.bytes,
                "ops": totals.ops,
                "files": totals.files,
                "procs": totals.procs,
                "max_file": max.as_ref().map(|m| m.1).unwrap_or(0),
                "max_file_path": max.as_ref().map(|m| m.0.clone()).unwrap_or_default(),
            },
            "hourly": hourly,
            "procs": procs.iter().map(|x| serde_json::json!({
                "process": x.process, "bytes": x.bytes, "ops": x.ops, "files": x.files,
            })).collect::<Vec<_>>(),
            "dirs": dirs.rows.iter().enumerate().map(|(i, x)| footprint::attach_usage(
                serde_json::json!({
                    "dir": x.path, "bytes": x.bytes, "ops": x.ops,
                }),
                dir_usage.get(i),
            )).collect::<Vec<_>>(),
        }),
    ))
}

fn api_processes(ctx: &Ctx, p: &Params) -> Result<Body, ApiError> {
    let a = Args::parse(p, Kind::Window)?;
    // 下拉只按窗口列进程：带着 q 过滤会让用户搜过一次之后再也换不了进程。
    let rows = ctx
        .db
        .by_process_in(a.range, "", "", 200)
        .map_err(internal)?;
    let total = rows.len() as i64;
    Ok(envelope(
        a.echo_json(),
        serde_json::json!({
            "total": total, "limit": 200, "offset": 0, "has_more": false,
        }),
        rows.iter()
            .map(|x| {
                serde_json::json!({ "process": x.process, "bytes": x.bytes, "ops": x.ops })
            })
            .collect(),
    ))
}

fn api_status(ctx: &Ctx) -> Result<Body, ApiError> {
    let counts = ctx.db.counts().map_err(internal)?;
    let cfg = meta_json(&ctx.db, "config")?;
    let flush_secs = cfg
        .as_ref()
        .and_then(|v| v.get("flush_secs"))
        .and_then(|v| v.as_i64())
        .unwrap_or(30);
    let last_flush = meta_i64(&ctx.db, "last_flush")?;
    let now = Local::now().timestamp();
    Ok(json(
        200,
        serde_json::json!({
            "ok": true,
            "version": env!("CARGO_PKG_VERSION"),
            "port": ctx.port,
            "uptime_secs": ctx.started.elapsed().as_secs(),
            "collector": {
                "last_flush": last_flush,
                "stale": last_flush.map(|ts| now - ts > 3 * flush_secs).unwrap_or(false),
                "flush_secs": flush_secs,
            },
            "usn": {
                "last_backfill": meta_i64(&ctx.db, "last_backfill")?,
                "journal_id": meta_i64(&ctx.db, "journal_id")?,
            },
            "db": {
                "writes": counts.writes,
                "usn": counts.usn,
                "alerts": counts.alerts,
                "bytes": counts.bytes,
            },
            "config": cfg.unwrap_or(serde_json::Value::Null),
        }),
    ))
}

fn meta_i64(db: &Db, key: &str) -> Result<Option<i64>, ApiError> {
    Ok(db
        .meta_get(key)
        .map_err(internal)?
        .and_then(|s| s.parse::<i64>().ok()))
}

fn meta_json(db: &Db, key: &str) -> Result<Option<serde_json::Value>, ApiError> {
    Ok(db
        .meta_get(key)
        .map_err(internal)?
        .and_then(|s| serde_json::from_str(&s).ok()))
}

impl Args {
    fn parse(p: &Params, kind: Kind) -> Result<Args, ApiError> {
        let (range, last) = parse_range(p)?;
        let raw_q = p.get("q").map(|s| s.as_str()).unwrap_or("");
        if raw_q.len() > MAX_Q_BYTES {
            return Err(bad("搜索关键字过长（不超过 512 字节）"));
        }
        // SQLite 的 lower() 只管 ASCII，中文与大小写必须在这一层就归一。
        let q = raw_q.to_lowercase();
        let process = match p.get("process") {
            Some(s) if s.len() > MAX_PROC_BYTES => return Err(bad("进程名过长")),
            Some(s) => s.clone(),
            None => String::new(),
        };
        let group = match kind {
            Kind::Writes => match p.get("group").map(|s| s.as_str()).unwrap_or("file") {
                "file" => Group::File,
                "dir" => Group::Dir,
                "none" => Group::Raw,
                other => return Err(bad(format!("group 只支持 file / dir / none，收到 {other}"))),
            },
            Kind::Usn => match p.get("group").map(|s| s.as_str()).unwrap_or("file") {
                "file" => Group::File,
                "dir" => Group::Dir,
                other => return Err(bad(format!("离线补录的 group 只支持 file / dir，收到 {other}"))),
            },
            _ => Group::File,
        };
        // 行级视图（告警、分钟明细）默认最新在前；聚合视图默认写入量在前。
        let row_level = kind == Kind::Alerts || (kind == Kind::Writes && group == Group::Raw);
        let (def_sort, def_order) = if row_level {
            (SortKey::Ts, Order::Desc)
        } else {
            (SortKey::Bytes, Order::Desc)
        };
        let sort = match p.get("sort").map(|s| s.as_str()) {
            None | Some("") => def_sort,
            Some("bytes") => SortKey::Bytes,
            Some("ops") => SortKey::Ops,
            Some("ts") => SortKey::Ts,
            Some("time_end") => SortKey::TimeEnd,
            Some("path") => SortKey::Path,
            Some("process") => SortKey::Process,
            Some("notified") => SortKey::Notified,
            Some(other) => return Err(bad(format!("未知排序键 {other}"))),
        };
        let order = match p.get("order").map(|s| s.as_str()) {
            None | Some("") => def_order,
            Some("asc") => Order::Asc,
            Some("desc") => Order::Desc,
            Some(other) => return Err(bad(format!("order 只支持 asc / desc，收到 {other}"))),
        };
        let limit = int_of(p, "limit", 50)?.clamp(1, 500);
        let offset = int_of(p, "offset", 0)?.clamp(0, MAX_OFFSET);
        let notified = match p.get("notified").map(|s| s.as_str()) {
            None | Some("") => None,
            Some("0") => Some(0),
            Some("1") => Some(1),
            Some(other) => return Err(bad(format!("notified 只支持 0 / 1，收到 {other}"))),
        };
        let usage = match p.get("usage").map(|s| s.as_str()) {
            None | Some("") | Some("0") => false,
            Some("1") => true,
            Some(other) => return Err(bad(format!("usage 只支持 0 / 1，收到 {other}"))),
        };
        Ok(Args {
            range,
            last,
            group,
            sort,
            order,
            limit,
            offset,
            q,
            process,
            notified,
            usage,
        })
    }

    fn page_query(&self) -> PageQuery {
        let mut q = PageQuery::new(self.range);
        q.group = self.group;
        q.sort = self.sort;
        q.order = self.order;
        q.limit = self.limit;
        q.offset = self.offset;
        q.q = self.q.clone();
        q.process = self.process.clone();
        q.notified = self.notified;
        q
    }

    fn echo_json(&self) -> serde_json::Value {
        let q = if self.q.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::Value::String(self.q.clone())
        };
        let process = if self.process.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::Value::String(self.process.clone())
        };
        serde_json::json!({
            "from": self.range.from,
            "to": self.range.to,
            "last": self.last,
            "q": q,
            "process": process,
            "group": group_label(self.group),
            "usage": self.usage,
        })
    }
}

fn group_label(g: Group) -> &'static str {
    match g {
        Group::File => "file",
        Group::Dir => "dir",
        Group::Raw => "none",
    }
}

fn parse_range(p: &Params) -> Result<(Range, Option<String>), ApiError> {
    let (range, last) = match (
        p.get("from").filter(|s| !s.is_empty()),
        p.get("to").filter(|s| !s.is_empty()),
    ) {
        (Some(f), Some(t)) => {
            let f = f.parse::<i64>().map_err(|_| bad("from 必须是 epoch 秒"))?;
            let t = t.parse::<i64>().map_err(|_| bad("to 必须是 epoch 秒"))?;
            if f < 0 || t <= f {
                return Err(bad("时间窗需满足 0 ≤ from < to"));
            }
            (Range { from: f, to: t }, None)
        }
        (None, None) => {
            let last = p.get("last").filter(|s| !s.is_empty()).map(|s| s.as_str()).unwrap_or("1h");
            let dur = humantime::parse_duration(last)
                .map_err(|_| bad(format!("无法解析时间段 {last}（示例: 30m / 1h / 2d）")))?;
            let now = Local::now().timestamp();
            let secs = dur.as_secs() as i64;
            if secs <= 0 {
                return Err(bad("时间段必须为正"));
            }
            (Range { from: now - secs, to: now }, Some(last.to_string()))
        }
        _ => return Err(bad("from 与 to 必须成对出现")),
    };
    if range.to - range.from > MAX_WINDOW_SECS {
        return Err(bad(format!(
            "时间窗最长 {} 天，请缩小范围",
            MAX_WINDOW_SECS / 86_400
        )));
    }
    Ok((range, last))
}

fn int_of(p: &Params, key: &str, default: i64) -> Result<i64, ApiError> {
    match p.get(key).map(|s| s.as_str()) {
        None | Some("") => Ok(default),
        Some(v) => v
            .parse::<i64>()
            .map_err(|_| bad(format!("{key} 必须是整数，收到 {v}"))),
    }
}

/// query string → 参数表。键后无值按空串处理，同名后者覆盖前者。
fn parse_query_str(s: &str) -> Params {
    let mut m = Params::new();
    for pair in s.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = match pair.split_once('=') {
            Some((k, v)) => (decode_component(k), decode_component(v)),
            None => (decode_component(pair), String::new()),
        };
        m.insert(k, v);
    }
    m
}

/// 字节级百分号解码：中文关键字是三个 `%XX` 一组，必须先在字节层拼回再整体按 UTF-8 解释，
/// 逐字符解码会把多字节序列拆成乱码。`+` 按表单惯例当空格（URLSearchParams 就是这么编的）；
/// 非法转义原样保留，绝不 panic。
fn decode_component(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hi = (bytes[i + 1] as char).to_digit(16);
                let lo = (bytes[i + 2] as char).to_digit(16);
                match (hi, lo) {
                    (Some(h), Some(l)) => {
                        out.push((h * 16 + l) as u8);
                        i += 3;
                    }
                    _ => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn envelope(query: serde_json::Value, page: serde_json::Value, rows: serde_json::Value) -> Body {
    json(
        200,
        serde_json::json!({ "ok": true, "query": query, "page": page, "rows": rows }),
    )
}

/// `truncated` 是目录折叠触到行数上限的标记，前端目前只多读不少。
fn page_json<T>(p: &Paged<T>) -> serde_json::Value {
    serde_json::json!({
        "total": p.total,
        "limit": p.limit,
        "offset": p.offset,
        "has_more": p.has_more,
        "truncated": p.truncated,
    })
}

fn paths_of(rows: &[FileAggRow]) -> Vec<String> {
    rows.iter().map(|r| r.path.clone()).collect()
}

fn file_json(rows: &[FileAggRow], us: &[Usage]) -> serde_json::Value {
    rows.iter()
        .enumerate()
        .map(|(i, r)| {
            footprint::attach_usage(
                serde_json::json!({
                    "path": r.path, "bytes": r.bytes, "ops": r.ops,
                    "t0": r.t0, "t1": r.t1, "process": r.process,
                }),
                us.get(i),
            )
        })
        .collect()
}

fn raw_json(rows: &[RawWriteRow]) -> serde_json::Value {
    rows.iter()
        .map(|r| {
            serde_json::json!({
                "path": r.path, "bytes": r.bytes, "ops": r.ops, "ts": r.ts, "process": r.process,
            })
        })
        .collect()
}

fn usn_json(rows: &[UsnAggRow]) -> serde_json::Value {
    rows.iter()
        .map(|r| serde_json::json!({ "path": r.path, "bytes": r.bytes, "ts": r.ts }))
        .collect()
}

fn alert_json(rows: &[AlertRow]) -> serde_json::Value {
    rows.iter()
        .map(|r| {
            serde_json::json!({
                "ts": r.ts, "bytes": r.bytes, "process": r.process,
                "path": r.path, "notified": r.notified,
            })
        })
        .collect()
}

fn json(status: u16, value: serde_json::Value) -> Body {
    Response::from_data(value.to_string().into_bytes())
        .with_status_code(status)
        .with_header(hdr(&b"Content-Type"[..], &b"application/json; charset=utf-8"[..]))
}

fn error(status: u16, code: &str, message: &str) -> Body {
    json(
        status,
        serde_json::json!({
            "ok": false,
            "error": { "code": code, "message": message },
        }),
    )
}

fn hdr(name: &[u8], value: &[u8]) -> Header {
    Header::from_bytes(name, value).expect("静态 ASCII 头部常量")
}

/// 防 DNS 重绑定：Host 只允许 127.0.0.1 / localhost（端口不参与比对）。
fn host_is_allowed(req: &Request) -> bool {
    match req
        .headers()
        .iter()
        .find(|h| h.field.equiv("Host"))
        .map(|h| h.value.as_str())
    {
        Some(host) => host_is_local(host),
        None => false,
    }
}

fn host_is_local(host: &str) -> bool {
    let host = host.trim();
    let name = match host.rsplit_once(':') {
        Some((name, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => name,
        _ => host,
    };
    name.eq_ignore_ascii_case("localhost") || name == "127.0.0.1"
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{UsnRow, WriteRow};
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::path::PathBuf;

    fn tmp_db(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("diskwatch-web-tests");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{}", path.display(), suffix));
        }
        path
    }

    /// 起服务并等端口可连；返回停止位与句柄，句柄 join 即已完成收尾。
    fn spawn(port: u16, db_path: &Path) -> (Arc<AtomicBool>, std::thread::JoinHandle<Result<()>>) {
        let stop = Arc::new(AtomicBool::new(false));
        let db_path = db_path.to_path_buf();
        let stop2 = stop.clone();
        let handle = std::thread::spawn(move || serve(&db_path, port, stop2));
        for _ in 0..100 {
            if TcpStream::connect((Ipv4Addr::LOCALHOST, port)).is_ok() {
                return (stop, handle);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("端口 {port} 未监听");
    }

    fn http_get(port: u16, path: &str, host: &str) -> String {
        let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).expect("连接测试端口");
        stream
            .write_all(
                format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n")
                    .as_bytes(),
            )
            .expect("发送请求");
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).expect("读取响应");
        String::from_utf8_lossy(&buf).into_owned()
    }

    fn code(resp: &str) -> u16 {
        resp.split_whitespace()
            .nth(1)
            .unwrap_or_else(|| panic!("状态行异常: {resp}"))
            .parse()
            .expect("状态码是数字")
    }

    fn body(resp: &str) -> serde_json::Value {
        let (_, b) = resp
            .split_once("\r\n\r\n")
            .unwrap_or_else(|| panic!("响应没有头部结束符: {resp}"));
        serde_json::from_str(b).unwrap_or_else(|e| panic!("响应体不是 JSON: {e} · {b}"))
    }

    fn get_json(port: u16, path: &str) -> serde_json::Value {
        let resp = http_get(port, path, "127.0.0.1");
        assert_eq!(code(&resp), 200, "响应异常: {resp}");
        body(&resp)
    }

    /// 导出响应是 text/csv，`body()` 的 JSON 解析用不上，这里直接拆头与体。
    fn csv_raw(port: u16, path: &str) -> (String, String) {
        let resp = http_get(port, path, "127.0.0.1");
        assert_eq!(code(&resp), 200, "导出响应异常: {resp}");
        let (head, b) = resp
            .split_once("\r\n\r\n")
            .unwrap_or_else(|| panic!("响应没有头部结束符: {resp}"));
        (head.to_string(), b.to_string())
    }

    /// CSV 数据行数：去掉 BOM、去掉表头；种子数据里没有含换行的路径，按行计是准的。
    fn csv_rows(port: u16, path: &str) -> (String, usize) {
        let (head, text) = csv_raw(port, path);
        let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
        let n = text
            .lines()
            .filter(|l| !l.trim().is_empty())
            .count()
            .saturating_sub(1);
        (head, n)
    }

    #[test]
    fn host_accepts_loopback_forms() {
        assert!(host_is_local("127.0.0.1"));
        assert!(host_is_local("127.0.0.1:8421"));
        assert!(host_is_local("localhost"));
        assert!(host_is_local("LOCALHOST:8421"));
    }

    #[test]
    fn host_rejects_rebinding() {
        assert!(!host_is_local("evil.example.com"));
        assert!(!host_is_local("evil.example.com:8421"));
        assert!(!host_is_local("192.168.1.10:8421"));
        assert!(!host_is_local(""));
    }

    #[test]
    fn decode_component_handles_multibyte_and_forms() {
        assert_eq!(decode_component("%E6%8A%A5%E8%A1%A8"), "报表");
        assert_eq!(decode_component("C%3A%5Ctmp%5Ca.log"), r"C:\tmp\a.log");
        assert_eq!(decode_component("a+b"), "a b", "URLSearchParams 用 + 表空格");
        assert_eq!(decode_component("100%25"), "100%");
        assert_eq!(decode_component("%zz%"), "%zz%", "非法转义原样保留");
        assert!(
            decode_component("%E6%8A").contains('\u{FFFD}'),
            "半截多字节序列不能 panic"
        );
    }

    #[test]
    fn parse_query_str_splits_and_decodes() {
        let p = parse_query_str("last=1h&q=%E6%8A%A5%E8%A1%A8&flag&empty=&x=1&x=2");
        assert_eq!(p.get("last").map(|s| s.as_str()), Some("1h"));
        assert_eq!(p.get("q").map(|s| s.as_str()), Some("报表"));
        assert_eq!(p.get("flag").map(|s| s.as_str()), Some(""));
        assert_eq!(p.get("empty").map(|s| s.as_str()), Some(""));
        assert_eq!(p.get("x").map(|s| s.as_str()), Some("2"), "同名后者覆盖");
        assert!(!p.contains_key("nope"));
    }

    #[test]
    fn parse_range_covers_last_and_explicit_window() {
        let (r, last) = parse_range(&parse_query_str("from=1000&to=2000")).unwrap();
        assert_eq!((r.from, r.to), (1000, 2000));
        assert!(last.is_none());

        let (r, last) = parse_range(&parse_query_str("last=30m")).unwrap();
        assert_eq!(r.to - r.from, 1800);
        assert_eq!(last.as_deref(), Some("30m"));

        assert_eq!(
            parse_range(&parse_query_str("last=")).unwrap().0.to
                - parse_range(&parse_query_str("last=")).unwrap().0.from,
            3600,
            "空的 last 走默认 1h"
        );
        assert!(parse_range(&parse_query_str("from=1")).is_err(), "from/to 必须成对");
        assert!(parse_range(&parse_query_str("from=2000&to=1000")).is_err());
        assert!(parse_range(&parse_query_str("from=-1&to=100")).is_err());
        assert!(parse_range(&parse_query_str("last=999d")).is_err(), "窗口最长 40 天");
        assert!(parse_range(&parse_query_str("last=abc")).is_err());
    }

    #[test]
    fn args_whitelist_group_sort_order_and_clamps() {
        let a = Args::parse(
            &parse_query_str("group=none&sort=time_end&order=asc&limit=9999&offset=-5&q=MiXeD"),
            Kind::Writes,
        )
        .unwrap();
        assert_eq!(a.group, Group::Raw);
        assert_eq!(a.sort, SortKey::TimeEnd);
        assert_eq!(a.order, Order::Asc);
        assert_eq!(a.limit, 500);
        assert_eq!(a.offset, 0);
        assert_eq!(a.q, "mixed", "小写在这层归一，SQLite 的 lower() 只管 ASCII");

        assert!(Args::parse(&parse_query_str("group=dir"), Kind::Usn).is_ok());
        assert!(Args::parse(&parse_query_str("group=none"), Kind::Usn).is_err());
        assert!(Args::parse(&parse_query_str("sort=nope"), Kind::Writes).is_err());
        assert!(Args::parse(&parse_query_str("order=DESC"), Kind::Writes).is_err());
        assert!(Args::parse(&parse_query_str("notified=2"), Kind::Alerts).is_err());
        assert_eq!(
            Args::parse(&parse_query_str("notified=0"), Kind::Alerts)
                .unwrap()
                .notified,
            Some(0)
        );
        assert_eq!(
            Args::parse(&parse_query_str(""), Kind::Alerts)
                .unwrap()
                .notified,
            None
        );
        assert_eq!(
            Args::parse(&parse_query_str("group=none"), Kind::Writes)
                .unwrap()
                .sort,
            SortKey::Ts,
            "分钟明细默认最新在前"
        );
        assert_eq!(
            Args::parse(&parse_query_str(""), Kind::Writes).unwrap().sort,
            SortKey::Bytes
        );
        let long_q = format!("q={}", "a".repeat(600));
        assert!(Args::parse(&parse_query_str(&long_q), Kind::Writes).is_err());
    }

    /// 停机回归：tiny_http 没有 exit_now()，只能靠 recv_timeout 轮询停止位后 drop Server。
    #[test]
    fn serve_answers_and_releases_port_on_stop() {
        let port = 18421;
        let (stop, served) = spawn(port, &tmp_db("stop.db"));

        let ping = http_get(port, "/api/ping", "127.0.0.1:18421");
        assert!(ping.starts_with("HTTP/1.1 200 OK"), "响应异常: {ping}");
        assert!(ping.contains("\"ok\":true"), "响应异常: {ping}");
        assert!(ping.contains("application/json"));

        let asset = http_get(port, "/index.html", "localhost");
        assert!(asset.starts_with("HTTP/1.1 200 OK"), "响应异常: {asset}");
        assert!(asset.contains("Cache-Control: no-cache"), "响应异常: {asset}");

        let missing = http_get(port, "/nope", "127.0.0.1");
        assert!(missing.starts_with("HTTP/1.1 404"), "响应异常: {missing}");

        let rebinding = http_get(port, "/api/ping", "evil.example.com");
        assert!(rebinding.starts_with("HTTP/1.1 400"), "响应异常: {rebinding}");

        stop.store(true, Ordering::SeqCst);
        let waited = Instant::now();
        served.join().unwrap().expect("serve 返回错误");
        assert!(waited.elapsed() < Duration::from_secs(3), "停机耗时过久");
        assert!(
            TcpStream::connect((Ipv4Addr::LOCALHOST, port)).is_err(),
            "停止后端口未释放"
        );
    }

    /// 服务态"端口被占只影响界面"的前提是 bind 真的会失败。Windows 的监听默认带
    /// SO_REUSEADDR，第二次 bind 有可能静默成功（那样谁都能顶掉这个界面，且
    /// service.rs 的告警分支永远走不到），所以这条必须钉死。18423 独立端口，
    /// 不与并行的 18421 / 18422 相撞。
    #[test]
    fn binding_a_busy_port_fails_instead_of_hijacking_it() {
        let port = 18423;
        let db = tmp_db("busy.db");
        let (stop, served) = spawn(port, &db);

        let err = serve(&db, port, Arc::new(AtomicBool::new(true)))
            .expect_err("端口已被占用，第二次 bind 不应成功");
        assert!(
            err.to_string().contains("监听"),
            "错误应指明监听失败: {err}"
        );

        stop.store(true, Ordering::SeqCst);
        served.join().unwrap().expect("serve 返回错误");
    }

    /// 六个端点跑真 socket + 真库，逐键核对前端契约。
    #[test]
    fn api_endpoints_answer_on_socket() {
        let port = 18422;
        let db_path = tmp_db("api.db");
        let now = Local::now().timestamp();
        {
            let mut db = Db::open(&db_path).unwrap();
            let w = |ts: i64, path: &str, process: &str, bytes: u64, ops: u64| WriteRow {
                ts,
                path: path.into(),
                process: process.into(),
                bytes,
                ops,
            };
            db.insert_writes(&[
                w(now - 300, r"C:\tmp\报表.xlsx", "excel.exe", 900, 2),
                w(now - 240, r"C:\tmp\a.log", "notepad.exe", 100, 5),
                w(now - 120, r"C:\tmp\a.log", "chrome.exe", 100, 1),
                w(now - 60, r"C:\bin\b.bin", "svc.exe", 150, 9),
            ])
            .unwrap();
            db.insert_usn_changes(
                &[UsnRow {
                    ts: now - 90,
                    path: r"C:\usn\x.bin".into(),
                    bytes: 700,
                }],
                42,
                7,
            )
            .unwrap();
            db.scan_alerts(0, 100).unwrap();
            db.meta_set("last_flush", &(now - 5).to_string()).unwrap();
            db.meta_set(
                "config",
                r#"{"drive":"C:","min_bytes":1048576,"flush_secs":30,"keep_days":30,"alert_bytes":100}"#,
            )
            .unwrap();
        }
        let (stop, served) = spawn(port, &db_path);

        // 按文件聚合：total 是不同路径数，进程串已排序。
        let j = get_json(port, "/api/writes?last=1h&limit=2");
        assert_eq!(j["page"]["total"], 3);
        assert_eq!(j["page"]["limit"], 2);
        assert_eq!(j["page"]["has_more"], true);
        assert_eq!(j["rows"][0]["path"].as_str(), Some(r"C:\tmp\报表.xlsx"));
        assert_eq!(j["rows"][0]["bytes"], 900);
        assert_eq!(j["rows"][1]["process"].as_str(), Some("chrome.exe,notepad.exe"));
        assert_eq!(j["rows"][1]["ops"], 6, "跨落盘求和");
        assert!(j["query"]["from"].is_i64() && j["query"]["to"].is_i64());
        assert_eq!(j["query"]["group"].as_str(), Some("file"));

        // 中文关键字：百分号必须在字节层拼回 UTF-8 才可能命中。
        let j = get_json(port, "/api/writes?last=1h&q=%E6%8A%A5%E8%A1%A8");
        assert_eq!(j["page"]["total"], 1);
        assert_eq!(j["rows"][0]["path"].as_str(), Some(r"C:\tmp\报表.xlsx"));
        assert_eq!(j["query"]["q"].as_str(), Some("报表"));

        let j = get_json(port, "/api/writes?last=1h&group=dir");
        assert_eq!(j["page"]["total"], 2);
        assert_eq!(j["rows"][0]["path"].as_str(), Some(r"C:\tmp"));
        assert_eq!(j["rows"][0]["bytes"], 1100);
        assert_eq!(j["rows"][0]["ops"], 8);
        assert_eq!(j["page"]["truncated"], false);

        // usage=1 才扫盘：字段必须齐（具体值取决于本机磁盘上有没有这些样例路径）
        let j = get_json(port, "/api/writes?last=1h&group=dir&usage=1");
        assert_eq!(j["query"]["usage"], true);
        let st = j["rows"][0]["usage_state"].as_str().unwrap_or("");
        assert!(matches!(st, "exact" | "partial" | "missing" | "denied"), "got {st}");
        assert!(j["rows"][0]["usage_bytes"].is_i64());
        // 默认不测：字段为 null 而不是 0，0 会被页面和脚本读成"这个目录不占空间"
        let j = get_json(port, "/api/writes?last=1h&group=dir");
        assert_eq!(j["query"]["usage"], false);
        assert_eq!(j["rows"][0]["usage_bytes"], serde_json::Value::Null);
        assert_eq!(j["rows"][0]["usage_state"], serde_json::Value::Null);
        let j = get_json(port, "/api/writes?last=1h&group=none&usage=1");
        assert_eq!(
            j["rows"][0]["usage_state"],
            serde_json::Value::Null,
            "分钟明细没有现值列"
        );
        assert!(Args::parse(&parse_query_str("usage=2"), Kind::Writes).is_err());

        let j = get_json(port, "/api/writes?last=1h&group=none&sort=ts&order=asc");
        assert_eq!(j["page"]["total"], 4, "明细是落盘桶行数");
        assert_eq!(j["rows"][0]["ts"], now - 300);
        assert!(j["rows"][0].get("t0").is_none(), "明细没有 t0/t1");

        let j = get_json(port, "/api/writes?last=1h&process=chrome.exe");
        assert_eq!(j["page"]["total"], 1);
        assert_eq!(j["rows"][0]["bytes"], 100);

        let j = get_json(port, "/api/usn?last=24h");
        assert_eq!(j["page"]["total"], 1);
        assert_eq!(j["rows"][0]["path"].as_str(), Some(r"C:\usn\x.bin"));
        assert_eq!(j["rows"][0]["bytes"], 700);
        let j = get_json(port, "/api/usn?last=24h&group=dir");
        assert_eq!(j["rows"][0]["path"].as_str(), Some(r"C:\usn"));
        let resp = http_get(port, "/api/usn?last=24h&process=excel.exe", "127.0.0.1");
        assert_eq!(code(&resp), 400);
        assert_eq!(body(&resp)["error"]["code"].as_str(), Some("bad_request"));

        let j = get_json(port, "/api/alerts?last=1h&sort=bytes&order=desc");
        assert_eq!(j["page"]["total"], 4);
        assert_eq!(j["rows"][0]["bytes"], 900);
        assert_eq!(j["rows"][0]["notified"], 0);
        assert_eq!(j["rows"][0]["process"].as_str(), Some("excel.exe"));

        let j = get_json(port, "/api/overview?last=1h");
        assert_eq!(j["totals"]["bytes"], 1250);
        assert_eq!(j["totals"]["files"], 3);
        assert_eq!(j["totals"]["procs"], 4);
        assert_eq!(j["totals"]["max_file"], 900);
        assert_eq!(
            j["totals"]["max_file_path"].as_str(),
            Some(r"C:\tmp\报表.xlsx")
        );
        let hourly = j["hourly"].as_array().unwrap();
        assert!(!hourly.is_empty());
        let sum: i64 = hourly.iter().map(|h| h[1].as_i64().unwrap()).sum();
        assert_eq!(sum, 1250, "柱图各桶之和必须等于总写入量");
        assert!(hourly.windows(2).all(|w| w[0][0].as_i64() < w[1][0].as_i64()), "横轴严格升序");
        assert_eq!(j["procs"][0]["process"].as_str(), Some("excel.exe"));
        assert_eq!(j["procs"][0]["files"], 1);
        assert_eq!(j["dirs"][0]["dir"].as_str(), Some(r"C:\tmp"));
        assert_eq!(j["dirs"][0]["bytes"], 1100);
        assert_eq!(
            j["dirs"][0]["usage_state"],
            serde_json::Value::Null,
            "overview 默认不扫盘"
        );

        // 目录卡片的现值：写入量第一的目录可能早就被回收了，卡片必须说清现在还占不占
        let j = get_json(port, "/api/overview?last=1h&usage=1");
        let st = j["dirs"][0]["usage_state"].as_str().unwrap_or("");
        assert!(matches!(st, "exact" | "partial" | "missing" | "denied"), "got {st}");

        // 关键字贯穿 overview 的每个部件，统计卡才不会和表格打架。
        let j = get_json(port, "/api/overview?last=1h&q=a.log");
        assert_eq!(j["totals"]["bytes"], 200);
        assert_eq!(j["totals"]["files"], 1);
        assert_eq!(j["procs"].as_array().unwrap().len(), 2);
        assert_eq!(j["dirs"].as_array().unwrap()[0]["dir"].as_str(), Some(r"C:\tmp"));

        let j = get_json(port, "/api/processes?last=1h");
        assert_eq!(j["rows"].as_array().unwrap().len(), 4);
        assert_eq!(j["rows"][0]["process"].as_str(), Some("excel.exe"));
        assert_eq!(j["page"]["has_more"], false);

        let j = get_json(port, "/api/status");
        assert_eq!(j["collector"]["last_flush"], now - 5);
        assert_eq!(j["collector"]["stale"], false);
        assert_eq!(j["collector"]["flush_secs"], 30);
        assert_eq!(j["usn"]["journal_id"], 7);
        assert_eq!(j["db"]["writes"], 4);
        assert_eq!(j["db"]["usn"], 1);
        assert_eq!(j["db"]["alerts"], 4);
        assert_eq!(j["db"]["bytes"], 1250);
        assert_eq!(j["config"]["drive"].as_str(), Some("C:"));
        assert_eq!(j["version"].as_str(), Some(env!("CARGO_PKG_VERSION")));

        for (path, why) in [
            ("/api/writes?group=nope", "未知 group"),
            ("/api/writes?sort=nope", "未知排序键"),
            ("/api/writes?last=999d", "窗口过长"),
            ("/api/writes?from=100", "from/to 不成对"),
            ("/api/writes?limit=abc", "limit 非数字"),
            ("/api/nope", "未知接口"),
        ] {
            let resp = http_get(port, path, "127.0.0.1");
            let want = if path == "/api/nope" { 404 } else { 400 };
            assert_eq!(code(&resp), want, "{why} 应回 {want}: {resp}");
            assert_eq!(body(&resp)["ok"], false, "{why} 错误体必须带 ok:false");
        }

        // ---------- CSV 导出 ----------
        let (head, rows) = csv_rows(port, "/api/export/writes?last=1h");
        let h = head.to_lowercase();
        assert!(h.contains("content-type: text/csv"), "导出必须声明 CSV: {head}");
        assert!(h.contains("attachment; filename="), "必须触发下载: {head}");
        assert!(
            h.contains("diskwatch-writes-"),
            "文件名要带视图名和日期: {head}"
        );
        assert_eq!(rows, 3, "按文件聚合 = 3 个不同路径");

        // 分页与排序参数必须被忽略：导出的是整个窗口，不是屏幕上那一页。
        let (_, rows) = csv_rows(port, "/api/export/writes?last=1h&limit=1&offset=9&sort=path");
        assert_eq!(rows, 3, "limit/offset/sort 不得缩小导出范围");

        let (_, text) = csv_raw(port, "/api/export/writes?last=1h");
        assert!(
            text.starts_with('\u{feff}'),
            "Excel 需要 UTF-8 BOM 才不认成 ANSI"
        );
        assert!(text.contains("时间范围起,时间范围止,文件路径,字节数,操作次数,进程"));
        assert!(
            text.contains(r"C:\tmp\报表.xlsx"),
            "中文路径必须原样进 CSV: {text}"
        );

        let (_, rows) = csv_rows(port, "/api/export/writes?last=1h&group=dir");
        assert_eq!(rows, 2, "目录粒度折成 C:\\tmp 与 C:\\bin");
        let (_, rows) = csv_rows(port, "/api/export/writes?last=1h&group=none");
        assert_eq!(rows, 4, "分钟明细是一行一个落盘桶");
        let (_, rows) = csv_rows(port, "/api/export/writes?last=1h&q=%E6%8A%A5%E8%A1%A8");
        assert_eq!(rows, 1, "搜索关键字必须贯穿导出");

        let (_, rows) = csv_rows(port, "/api/export/usn?last=24h");
        assert_eq!(rows, 1);
        let (_, rows) = csv_rows(port, "/api/export/alerts?last=1h");
        assert_eq!(rows, 4);

        let resp = http_get(port, "/api/export/usn?last=24h&process=excel.exe", "127.0.0.1");
        assert_eq!(code(&resp), 400, "离线补录没有进程维度");
        let resp = http_get(port, "/api/export/nope", "127.0.0.1");
        assert_eq!(code(&resp), 404, "未知导出视图应回 404");

        stop.store(true, Ordering::SeqCst);
        served.join().unwrap().expect("serve 返回错误");
    }
}
