mod alert;
mod assets;
mod collect;
mod csv;
mod db;
mod eventlog;
mod export;
mod log;
mod report;
mod service;
mod top;
mod usn;
mod util;
mod web;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "diskwatch",
    version,
    about = "磁盘写入监控：记录什么文件、什么进程、在什么时间写入了多大"
)]
struct Cli {
    /// SQLite 数据库路径（默认 <程序目录>/data/diskwatch.db）
    #[arg(long, global = true)]
    db: Option<PathBuf>,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// 常驻采集文件写入事件（需要管理员权限）
    Collect {
        /// 只监控的盘符
        #[arg(long, default_value = "C")]
        drive: String,
        /// 单个文件在一分钟内累计写入达到该字节数才入库
        #[arg(long, default_value_t = 1024 * 1024)]
        min_bytes: u64,
        /// 聚合落盘间隔（秒）
        #[arg(long, default_value_t = 30)]
        flush_secs: u64,
        /// 数据保留天数
        #[arg(long, default_value_t = 30)]
        keep_days: u64,
        /// 单文件单分钟写入达到该字节数触发告警（0 = 关闭告警）
        #[arg(long, default_value_t = 100 * 1024 * 1024)]
        alert_bytes: u64,
        /// 调试：打印前 N 个事件的解析细节
        #[arg(long, default_value_t = 0)]
        debug_events: u32,
        /// 运行指定秒数后自动退出（用于测试）
        #[arg(long)]
        duration: Option<u64>,
        /// 同时在本机 127.0.0.1:8421 提供 Web 界面
        #[arg(long)]
        web: bool,
    },
    /// 生成 HTML 统计报告
    Report {
        /// 统计最近多少小时
        #[arg(long, default_value_t = 24)]
        hours: u64,
        /// 输出文件路径（默认 <程序目录>/reports/report-日期.html）
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// 手工执行一次 USN 离线回填（观测口，需要管理员权限）
    Backfill {
        /// 回填针对的盘符
        #[arg(long, default_value = "C")]
        drive: String,
        /// 回填尺寸地板（与采集阈值同口径）
        #[arg(long, default_value_t = 1024 * 1024)]
        min_bytes: u64,
    },
    /// 作为 Windows 服务运行（仅由服务控制管理器启动）
    Service,
    /// 只启动 Web 界面（免管理员权限；开发/手工查看通道）
    Web {
        /// 监听端口（固定绑 127.0.0.1）
        #[arg(long, default_value_t = web::DEFAULT_PORT)]
        port: u16,
    },
    /// 查询写入量排行
    Top {
        /// 统计最近时间段，如 30m / 1h / 2d
        #[arg(long, default_value = "1h")]
        last: String,
        /// 聚合维度：file（按文件）/ dir（按所在目录）/ process（按进程）
        #[arg(long, default_value = "file")]
        by: String,
        /// 显示前多少条
        #[arg(long, default_value_t = 30)]
        limit: usize,
        /// 路径过滤子串
        #[arg(long)]
        contains: Option<String>,
        /// 输出 JSON（给脚本/agent 消费，替代文本对齐表；仅 stdout，无其他输出）
        #[arg(long)]
        json: bool,
    },
    /// 导出 CSV（writes / usn / alerts 各一份，Excel 可直接打开）
    Export {
        /// 统计最近时间段，如 30m / 1h / 7d
        #[arg(long, default_value = "24h")]
        last: String,
        /// 输出目录（默认 <程序目录>/exports/）
        #[arg(long)]
        out: Option<PathBuf>,
        /// 路径过滤子串
        #[arg(long)]
        contains: Option<String>,
        /// 进程名精确过滤（离线补录视图无进程维度，会忽略此项）
        #[arg(long)]
        process: Option<String>,
        /// 实时写入视图的聚合维度：file / dir / none（分钟明细）
        #[arg(long, default_value = "file")]
        by: String,
    },
}

fn default_db_path() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("获取程序路径失败")?;
    Ok(exe
        .parent()
        .unwrap_or(Path::new("."))
        .join("data")
        .join("diskwatch.db"))
}

fn default_log_path() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("获取程序路径失败")?;
    Ok(exe
        .parent()
        .unwrap_or(Path::new("."))
        .join("logs")
        .join("collect.log"))
}

fn main() -> Result<()> {
    log::init(&default_log_path()?);
    let cli = Cli::parse();
    let db_path = match cli.db {
        Some(p) => p,
        None => default_db_path()?,
    };
    match cli.cmd {
        Cmd::Collect {
            drive,
            min_bytes,
            flush_secs,
            keep_days,
            alert_bytes,
            debug_events,
            duration,
            web: enable_web,
        } => {
            let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            ctrlc::set_handler({
                let stop = stop.clone();
                move || stop.store(true, std::sync::atomic::Ordering::SeqCst)
            })
            .ok();
            let web_thread = enable_web.then(|| {
                let db_path = db_path.clone();
                let stop_web = stop.clone();
                std::thread::spawn(move || {
                    if let Err(e) = web::serve(&db_path, web::DEFAULT_PORT, stop_web) {
                        log::warn(format!("Web 服务退出: {e}"));
                    }
                })
            });
            let result = collect::run(
                &db_path,
                &drive,
                min_bytes,
                flush_secs,
                keep_days,
                alert_bytes,
                debug_events,
                duration,
                stop.clone(),
            );
            if let Some(handle) = web_thread {
                stop.store(true, std::sync::atomic::Ordering::SeqCst);
                let _ = handle.join();
            }
            result
        }
        Cmd::Web { port } => {
            let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            ctrlc::set_handler({
                let stop = stop.clone();
                move || stop.store(true, std::sync::atomic::Ordering::SeqCst)
            })
            .ok();
            web::serve(&db_path, port, stop)
        }
        Cmd::Report { hours, out } => report::run(&db_path, hours, out),
        Cmd::Backfill { drive, min_bytes } => {
            util::check_admin()?;
            let mut db = db::Db::open(&db_path)?;
            log::info(format!("数据库: {}", db_path.display()));
            if let Some(last) = db.meta_get("last_usn")? {
                log::info(format!(
                    "当前书签: last_usn={} journal_id={}",
                    last,
                    db.meta_get("journal_id")?.unwrap_or_default()
                ));
            } else {
                log::info("当前书签: 无（首次运行将建立基线）".to_string());
            }
            match usn::backfill(&mut db, &drive, min_bytes)? {
                Some(s) => {
                    log::info(format!(
                        "USN 日志: journal_id=0x{:x} FirstUsn={} NextUsn={}",
                        s.journal_id, s.first_usn, s.next_usn
                    ));
                }
                None => log::warn("回填不可用（见上方警告）".to_string()),
            }
            Ok(())
        }
        Cmd::Service => service::run(),
        Cmd::Top {
            last,
            by,
            limit,
            contains,
            json,
        } => top::run(
            &db_path,
            &last,
            &by,
            limit,
            contains.as_deref(),
            json,
        ),
        Cmd::Export {
            last,
            out,
            contains,
            process,
            by,
        } => export::run_cli(
            &db_path,
            &last,
            out,
            contains.as_deref().unwrap_or(""),
            process.as_deref().unwrap_or(""),
            &by,
        ),
    }
}
