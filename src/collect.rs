use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use ferrisetw::parser::Parser;
use ferrisetw::provider::Provider;
use ferrisetw::schema::Schema;
use ferrisetw::schema_locator::SchemaLocator;
use ferrisetw::trace::{stop_trace_by_name, TraceProperties, UserTrace};
use ferrisetw::{EventRecord, EventSerializer, EventSerializerOptions};

use crate::alert;
use crate::db::{Db, WriteRow};
use crate::log;
use crate::usn;
use crate::util;

const SESSION_NAME: &str = "diskwatch-collector";
const KERNEL_FILE_GUID: &str = "edd08927-9cc4-4e65-b970-c2560fb5c289";
const KW_FILENAME: u64 = 0x10;
const KW_FILEIO: u64 = 0x20;
const KW_CREATE: u64 = 0x80;
const KW_WRITE: u64 = 0x200;
const FILEKEY_MAP_CAP: usize = 500_000;

struct State {
    prefix: String,
    drive: String,
    procs: util::ProcNames,
    filekeys: HashMap<u64, String>,
    agg: HashMap<(i64, String, String), (u64, u64)>,
    total_events: u64,
    write_events: u64,
    recorded_files: u64,
}

pub fn run(
    db_path: &Path,
    drive: &str,
    min_bytes: u64,
    flush_secs: u64,
    keep_days: u64,
    alert_bytes: u64,
    debug_events: u32,
    duration: Option<u64>,
    stop: Arc<AtomicBool>,
) -> Result<()> {
    util::check_admin()?;

    let mut db = Db::open(db_path)?;
    let prefix = util::device_prefix(drive)?;
    log::info(format!("监控盘符 {}:  设备前缀 {}", drive, prefix));
    log::info(format!("数据库: {}", db_path.display()));
    log::info(format!(
        "入库阈值: 单文件单分钟累计写入 >= {}",
        util::human_bytes(min_bytes)
    ));
    log::info(if alert_bytes > 0 {
        format!(
            "告警阈值: 单文件单分钟累计写入 >= {}",
            util::human_bytes(alert_bytes)
        )
    } else {
        "告警: 已关闭".to_string()
    });

    // 配置快照：Web 的 /api/status 直接读这一份，省掉跨进程传参。
    let cfg = serde_json::json!({
        "drive": drive,
        "min_bytes": min_bytes,
        "flush_secs": flush_secs,
        "keep_days": keep_days,
        "alert_bytes": alert_bytes,
    });
    if let Err(e) = db.meta_set("config", &cfg.to_string()) {
        log::warn(format!("写入配置快照失败：{e}"));
    }

    // 启动回填：补录停机期间的写入（best-effort，失败不阻断采集）
    if let Err(e) = usn::backfill(&mut db, drive, min_bytes) {
        log::warn(format!("启动回填失败（不影响采集）：{e}"));
    }

    let _ = stop_trace_by_name(SESSION_NAME);

    let state = Arc::new(Mutex::new(State {
        prefix,
        drive: drive.to_string(),
        procs: util::ProcNames::new(),
        filekeys: HashMap::new(),
        agg: HashMap::new(),
        total_events: 0,
        write_events: 0,
        recorded_files: 0,
    }));
    let debug_left = Arc::new(AtomicU32::new(debug_events));
    let seen_tasks = Arc::new(Mutex::new(std::collections::HashSet::<String>::new()));

    let provider = Provider::by_guid(KERNEL_FILE_GUID)
        .any(KW_FILENAME | KW_FILEIO | KW_CREATE | KW_WRITE)
        .level(4)
        .add_callback({
            let state = state.clone();
            let debug_left = debug_left.clone();
            let seen_tasks = seen_tasks.clone();
            move |record: &EventRecord, sl: &SchemaLocator| {
                handle_event(record, sl, &state, &debug_left, &seen_tasks);
            }
        })
        .build();

    let props = TraceProperties {
        buffer_size: 64,
        max_buffer: 256,
        ..Default::default()
    };
    let trace = UserTrace::new()
        .named(SESSION_NAME.to_string())
        .set_trace_properties(props)
        .enable(provider)
        .start_and_process()
        .map_err(|e| {
            anyhow::anyhow!("启动 ETW 会话失败: {e:?}（确认以管理员身份运行，且无其它程序占用同名会话）")
        })?;

    log::info(format!("采集中，每 {} 秒落盘一次", flush_secs));

    let flush_every = flush_secs.max(1);
    let mut tick = 0u64;
    loop {
        std::thread::sleep(Duration::from_secs(1));
        tick += 1;
        if tick % flush_every == 0 {
            flush(&state, &mut db, min_bytes);
            scan_alerts(&mut db, alert_bytes);
            heartbeat(&mut db);
        }
        if stop.load(Ordering::SeqCst) || duration.is_some_and(|d| tick >= d) {
            break;
        }
    }

    flush(&state, &mut db, min_bytes);
    scan_alerts(&mut db, alert_bytes);
    heartbeat(&mut db);
    let cutoff = chrono::Local::now().timestamp() - (keep_days * 86400) as i64;
    let removed = db.cleanup_before(cutoff).unwrap_or(0);
    if removed > 0 {
        log::info(format!("清理 {} 天前的旧记录 {} 条", keep_days, removed));
    }
    if let Err(e) = usn::update_bookmark(&mut db, drive) {
        log::warn(format!("更新书签失败：{e}"));
    }

    let _ = trace.stop();
    let st = state.lock().unwrap();
    log::info(format!(
        "结束。共处理 {} 个事件（写入 {} 个），产生 {} 条聚合记录",
        st.total_events, st.write_events, st.recorded_files
    ));
    Ok(())
}

/// 采集心跳。空落盘也要写：长时间没有达到阈值的写入不等于采集死了。
fn heartbeat(db: &mut Db) {
    let now = chrono::Local::now().timestamp().to_string();
    if let Err(e) = db.meta_set("last_flush", &now) {
        log::warn(format!("写采集心跳失败：{e}"));
    }
}

fn scan_alerts(db: &mut Db, alert_bytes: u64) {
    if alert_bytes == 0 {
        return;
    }
    // 回看 10 分钟：覆盖跨两次落盘的分钟桶；扫描幂等，重复不会重复告警
    let from = chrono::Local::now().timestamp() - 600;
    if let Err(e) = alert::scan(db, from, alert_bytes) {
        log::warn(format!("告警扫描失败：{e}"));
    }
}

fn flush(state: &Arc<Mutex<State>>, db: &mut Db, min_bytes: u64) {
    let rows: Vec<WriteRow> = {
        let mut st = state.lock().unwrap();
        let agg = std::mem::take(&mut st.agg);
        agg.into_iter()
            .filter(|(_, (bytes, _))| *bytes >= min_bytes)
            .map(|((ts, path, process), (bytes, ops))| WriteRow {
                ts,
                path,
                process,
                bytes,
                ops,
            })
            .collect()
    };
    if rows.is_empty() {
        return;
    }
    match db.insert_writes(&rows) {
        Ok(()) => {
            let mut st = state.lock().unwrap();
            st.recorded_files += rows.len() as u64;
            log::info(format!("落盘 {} 条聚合记录", rows.len()));
        }
        Err(e) => log::error(format!("写入数据库失败: {e}")),
    }
}

fn handle_event(
    record: &EventRecord,
    sl: &SchemaLocator,
    state: &Arc<Mutex<State>>,
    debug_left: &AtomicU32,
    seen_tasks: &Mutex<std::collections::HashSet<String>>,
) {
    let schema = match sl.event_schema(record) {
        Ok(s) => s,
        Err(_) => return,
    };
    let task = schema.task_name();
    let parser = Parser::create(record, &schema);

    let first_of_task = seen_tasks
        .lock()
        .map(|mut seen| seen.insert(task.clone()))
        .unwrap_or(false);
    if first_of_task
        && debug_left.load(Ordering::Relaxed) > 0
        && debug_left.fetch_sub(1, Ordering::Relaxed) > 0
    {
        debug_dump(record, &task, &schema);
    }

    let mut st = match state.lock() {
        Ok(s) => s,
        Err(_) => return,
    };
    st.total_events += 1;

    match task.as_str() {
        "Write" => {
            st.write_events += 1;
            let bytes: u64 = parser
                .try_parse::<u32>("IOSize")
                .map(u64::from)
                .unwrap_or(0);
            let filekey: Option<u64> = parser.try_parse::<u64>("FileKey").ok();
            let fileobj: Option<u64> = parser.try_parse::<u64>("FileObject").ok();
            let Some(dev_path) = filekey
                .and_then(|k| st.filekeys.get(&k).cloned())
                .or_else(|| fileobj.and_then(|k| st.filekeys.get(&k).cloned()))
            else {
                return;
            };
            if !dev_path.starts_with(&st.prefix) {
                return;
            }
            let disp = util::to_display(&dev_path, &st.prefix, &st.drive);
            let ts = util::minute_bucket(util::filetime_to_unix(record.raw_timestamp()));
            let proc = st.procs.name(record.process_id());
            let e = st.agg.entry((ts, disp, proc)).or_insert((0, 0));
            e.0 += bytes;
            e.1 += 1;
        }
        "NameCreate" | "Create" => {
            let key: Option<u64> = parser
                .try_parse::<u64>("FileKey")
                .ok()
                .or_else(|| parser.try_parse::<u64>("FileObject").ok());
            let name: Option<String> = parser.try_parse::<String>("FileName").ok();
            if let (Some(k), Some(n)) = (key, name) {
                if st.filekeys.len() >= FILEKEY_MAP_CAP {
                    st.filekeys.clear();
                }
                st.filekeys.insert(k, n);
            }
        }
        _ => {}
    }
}

fn debug_dump(record: &EventRecord, task: &str, schema: &Schema) {
    let opts = EventSerializerOptions {
        include_schema: false,
        include_header: false,
        ..Default::default()
    };
    let ser = EventSerializer::new(record, schema, opts);
    match serde_json::to_string(&ser) {
        Ok(json) => log::info(format!("[debug] task={task} pid={} {json}", record.process_id())),
        Err(e) => log::info(format!(
            "[debug] task={task} pid={} 序列化失败: {e}",
            record.process_id()
        )),
    }
}
