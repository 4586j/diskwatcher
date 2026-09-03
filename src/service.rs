use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use anyhow::{anyhow, Result};
use windows::core::{HSTRING, PWSTR};
use windows::Win32::Foundation::ERROR_CALL_NOT_IMPLEMENTED;
use windows::Win32::System::Services::{
    RegisterServiceCtrlHandlerExW, SetServiceStatus, StartServiceCtrlDispatcherW,
    SERVICE_ACCEPT_SHUTDOWN, SERVICE_ACCEPT_STOP, SERVICE_CONTROL_INTERROGATE,
    SERVICE_CONTROL_SHUTDOWN, SERVICE_CONTROL_STOP, SERVICE_RUNNING, SERVICE_START_PENDING,
    SERVICE_STATUS, SERVICE_STATUS_CURRENT_STATE, SERVICE_STATUS_HANDLE, SERVICE_STOPPED,
    SERVICE_STOP_PENDING, SERVICE_TABLE_ENTRYW, SERVICE_WIN32_OWN_PROCESS,
};

use crate::collect;
use crate::eventlog;
use crate::log;
use crate::web;

const SERVICE_NAME: &str = "DiskWatch";
const EVENT_ID_SERVICE: u32 = 101;
const EVENT_ID_WEB: u32 = 102;

// 三期硬编码默认值；binPath 传参留待四期
const MIN_BYTES: u64 = 1024 * 1024;
const FLUSH_SECS: u64 = 30;
const KEEP_DAYS: u64 = 30;
const ALERT_BYTES: u64 = 100 * 1024 * 1024;

static STATUS_HANDLE: OnceLock<SERVICE_STATUS_HANDLE> = OnceLock::new();

fn stop_flag() -> Arc<AtomicBool> {
    static STOP: OnceLock<Arc<AtomicBool>> = OnceLock::new();
    STOP.get_or_init(|| Arc::new(AtomicBool::new(false))).clone()
}

fn report(state: SERVICE_STATUS_CURRENT_STATE, accepted: u32, checkpoint: u32, wait_hint: u32) {
    let Some(h) = STATUS_HANDLE.get() else {
        return;
    };
    let st = SERVICE_STATUS {
        dwServiceType: SERVICE_WIN32_OWN_PROCESS,
        dwCurrentState: state,
        dwControlsAccepted: accepted,
        dwWin32ExitCode: 0,
        dwServiceSpecificExitCode: 0,
        dwCheckPoint: checkpoint,
        dwWaitHint: wait_hint,
    };
    let _ = unsafe { SetServiceStatus(*h, &st) };
}

/// 控制处理器：只置停止位立即返回（SCM 要求 30 秒内响应），收尾由主循环完成。
unsafe extern "system" fn control_handler(
    control: u32,
    _event_type: u32,
    _event_data: *mut c_void,
    _context: *mut c_void,
) -> u32 {
    match control {
        SERVICE_CONTROL_STOP | SERVICE_CONTROL_SHUTDOWN => {
            report(SERVICE_STOP_PENDING, 0, 1, 30_000);
            stop_flag().store(true, Ordering::SeqCst);
            0
        }
        SERVICE_CONTROL_INTERROGATE => {
            report(
                SERVICE_RUNNING,
                SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN,
                0,
                0,
            );
            0
        }
        _ => ERROR_CALL_NOT_IMPLEMENTED.0,
    }
}

unsafe extern "system" fn service_main(_argc: u32, _argv: *mut PWSTR) {
    let name = HSTRING::from(SERVICE_NAME);
    let handle = match unsafe { RegisterServiceCtrlHandlerExW(&name, Some(control_handler), None) }
    {
        Ok(h) => h,
        Err(e) => {
            log::error(format!("注册服务控制处理器失败: {e}"));
            return;
        }
    };
    let _ = STATUS_HANDLE.set(handle);
    report(SERVICE_START_PENDING, 0, 1, 60_000);
    eventlog::info(EVENT_ID_SERVICE, "DiskWatch 服务正在启动");
    report(
        SERVICE_RUNNING,
        SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN,
        0,
        0,
    );

    let result = match crate::default_db_path() {
        Ok(db_path) => {
            let web_db = db_path.clone();
            let web_thread = std::thread::spawn(move || {
                if let Err(e) = web::serve(&web_db, web::DEFAULT_PORT, stop_flag()) {
                    // 端口被抢占等启动失败只影响界面，绝不能带走采集。
                    log::warn(format!("Web 服务退出: {e}"));
                    eventlog::warn(
                        EVENT_ID_WEB,
                        &format!("DiskWatch Web 界面不可用，采集不受影响：{e}"),
                    );
                }
            });
            let r = collect::run(
                &db_path,
                "C",
                MIN_BYTES,
                FLUSH_SECS,
                KEEP_DAYS,
                ALERT_BYTES,
                0,
                None,
                stop_flag(),
            );
            // 采集可能因启动失败提前返回，此时停止位并未置起：先置位再 join。
            stop_flag().store(true, Ordering::SeqCst);
            let _ = web_thread.join();
            r
        }
        Err(e) => Err(e),
    };
    match result {
        Ok(()) => {
            log::info("服务正常停止".to_string());
            eventlog::info(EVENT_ID_SERVICE, "DiskWatch 服务已停止");
        }
        Err(e) => {
            log::error(format!("服务运行出错: {e}"));
            eventlog::info(EVENT_ID_SERVICE, &format!("DiskWatch 服务出错退出: {e}"));
        }
    }
    report(SERVICE_STOPPED, 0, 0, 0);
}

pub fn run() -> Result<()> {
    let name = HSTRING::from(SERVICE_NAME);
    let table = [
        SERVICE_TABLE_ENTRYW {
            lpServiceName: PWSTR::from_raw(name.as_ptr() as *mut u16),
            lpServiceProc: Some(service_main),
        },
        SERVICE_TABLE_ENTRYW::default(),
    ];
    unsafe { StartServiceCtrlDispatcherW(table.as_ptr()) }.map_err(|e| {
        // 1063 = ERROR_FAILED_SERVICE_CONTROLLER_CONNECT：控制台直跑 service 子命令
        if (e.code().0 as u32 & 0xFFFF) == 1063 {
            anyhow!(
                "service 子命令只能由 Windows 服务控制管理器启动；\
                 在控制台直接运行会失败。手工测试请用 collect 子命令。"
            )
        } else {
            anyhow!("连接服务调度器失败: {e}")
        }
    })
}
