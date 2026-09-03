use std::collections::HashMap;

use windows::core::{HSTRING, PWSTR};
use windows::Win32::Foundation::{CloseHandle, BOOL};
use windows::Win32::Storage::FileSystem::QueryDosDeviceW;
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};

const FILETIME_UNIX_DIFF: i64 = 116_444_736_000_000_000;

pub fn device_prefix(drive: &str) -> anyhow::Result<String> {
    let dev = HSTRING::from(format!("{}:", drive.trim().to_uppercase()));
    let mut buf = vec![0u16; 512];
    let n = unsafe { QueryDosDeviceW(&dev, Some(&mut buf)) };
    if n == 0 {
        anyhow::bail!("QueryDosDevice 失败，盘符 {} 可能不存在", drive);
    }
    let n = n as usize;
    let end = buf[..n.min(buf.len())]
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(n.min(buf.len()));
    Ok(String::from_utf16_lossy(&buf[..end]))
}

pub fn to_display(path: &str, prefix: &str, drive: &str) -> String {
    match path.strip_prefix(prefix) {
        Some(rest) => format!("{}:{}", drive.to_uppercase(), rest),
        None => path.to_string(),
    }
}

pub fn filetime_to_unix(ft: i64) -> i64 {
    (ft - FILETIME_UNIX_DIFF) / 10_000_000
}

pub fn check_admin() -> anyhow::Result<()> {
    let out = std::process::Command::new("net")
        .arg("session")
        .output()
        .map_err(|e| anyhow::anyhow!("执行权限检查失败: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        anyhow::bail!(
            "需要管理员权限才能访问卷/ETW。请以管理员身份打开终端后再运行，例如：\n  diskwatch collect"
        )
    }
}

pub fn minute_bucket(unix_secs: i64) -> i64 {
    unix_secs / 60 * 60
}

pub fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

pub struct ProcNames {
    cache: HashMap<u32, String>,
}

impl ProcNames {
    pub fn new() -> Self {
        Self {
            cache: HashMap::new(),
        }
    }

    pub fn name(&mut self, pid: u32) -> String {
        if let Some(n) = self.cache.get(&pid) {
            return n.clone();
        }
        // 0/4 是 Idle/System，无法用常规句柄查询映像名；
        // 缓存懒写（lazy writer）常归属 System
        let name = match pid {
            0 => "Idle".to_string(),
            4 => "System".to_string(),
            _ => query_process_name(pid).unwrap_or_else(|| format!("pid:{pid}")),
        };
        self.cache.insert(pid, name.clone());
        name
    }
}

fn query_process_name(pid: u32) -> Option<String> {
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, BOOL(0), pid).ok()?;
        let mut buf = vec![0u16; 1024];
        let mut size = buf.len() as u32;
        let r = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut size);
        let _ = CloseHandle(h);
        r.ok()?;
        let full = String::from_utf16_lossy(&buf[..size as usize]);
        Some(
            full.rsplit(['\\', '/'])
                .next()
                .unwrap_or(&full)
                .to_string(),
        )
    }
}
