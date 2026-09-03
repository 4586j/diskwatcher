use windows::core::{HSTRING, PCWSTR};
use windows::Win32::Foundation::PSID;
use windows::Win32::System::EventLog::{
    DeregisterEventSource, RegisterEventSourceW, ReportEventW, EVENTLOG_INFORMATION_TYPE,
    EVENTLOG_WARNING_TYPE, REPORT_EVENT_TYPE,
};

use crate::log;

fn write(kind: REPORT_EVENT_TYPE, event_id: u32, message: &str) {
    unsafe {
        let source = HSTRING::from("DiskWatch");
        let handle = match RegisterEventSourceW(PCWSTR::null(), &source) {
            Ok(h) => h,
            Err(e) => {
                log::warn(format!("注册事件源失败（跳过事件日志）：{e}"));
                return;
            }
        };
        let msg = HSTRING::from(message);
        let strings = [PCWSTR(msg.as_ptr())];
        let r = ReportEventW(
            handle,
            kind,
            0,
            event_id,
            PSID::default(),
            0,
            Some(&strings),
            None,
        );
        let _ = DeregisterEventSource(handle);
        if let Err(e) = r {
            log::warn(format!("写事件日志失败：{e}"));
        }
    }
}

pub fn warn(event_id: u32, message: &str) {
    write(EVENTLOG_WARNING_TYPE, event_id, message);
}

pub fn info(event_id: u32, message: &str) {
    write(EVENTLOG_INFORMATION_TYPE, event_id, message);
}
