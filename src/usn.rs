use std::collections::HashMap;
use std::ffi::c_void;

use anyhow::{Context, Result};
use windows::core::HSTRING;
use windows::Win32::Foundation::{CloseHandle, GENERIC_READ, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, GetFileInformationByHandle, GetFinalPathNameByHandleW, OpenFileById,
    ExtendedFileIdType, FileIdType, FILE_FLAGS_AND_ATTRIBUTES, FILE_ID_128, FILE_ID_DESCRIPTOR,
    FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    VOLUME_NAME_NT,
};
use windows::Win32::System::IO::DeviceIoControl;
use windows::Win32::System::Ioctl::{
    FSCTL_QUERY_USN_JOURNAL, FSCTL_READ_USN_JOURNAL, READ_USN_JOURNAL_DATA_V1, USN_JOURNAL_DATA_V0,
};

use crate::db::{Db, UsnRow};
use crate::log;
use crate::util;

// 数据类改动：覆盖写入/扩展/截断/新建/重命名新名
const REASON_MASK: u32 = 0x0000_0001 | 0x0000_0002 | 0x0000_0004 | 0x0000_0100 | 0x0000_2000;
const BUF_U64S: usize = 32768; // 256KB，8 字节对齐
const RECORD_CAP: u64 = 1_000_000;
// 内核前向扫描窗口有限（本机实测约 230~265K USN）：窗口内无匹配记录时
// 直接返回 ERROR_INVALID_PARAMETER(87) 而非空结果。遇到时按小步跳跃重试。
const ERROR_INVALID_PARAMETER: u32 = 87;
const SKIP_STEP: i64 = 65536;
const SKIP_MAX: u32 = 1024; // 连续跳跃上限，约 64MB USN 空间
const EXCLUDE_SUBSTR: [&str; 5] = [
    "pagefile.sys",
    "hiberfil.sys",
    "swapfile.sys",
    "\\$extend\\",
    "\\system volume information\\",
];

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub enum FileRef {
    V2(u64),
    V3([u8; 16]),
}

pub struct BackfillStats {
    pub journal_id: u64,
    pub first_usn: i64,
    pub next_usn: i64,
    pub mode: String,
    pub records_read: u64,
    pub skipped_v4: u64,
    pub filtered_reason: u64,
    pub skipped_usns: u64,
    pub unique_files: u64,
    pub unresolvable: u64,
    pub excluded: u64,
    pub below_floor: u64,
    pub inserted: u64,
}

pub fn open_volume(drive: &str) -> Result<HANDLE> {
    let path = HSTRING::from(format!(r"\\.\{}:", drive.trim().to_uppercase()));
    let h = unsafe {
        CreateFileW(
            &path,
            GENERIC_READ.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES(0),
            HANDLE::default(),
        )
    }
    .with_context(|| format!("打开卷 \\\\.\\{}: 失败（需要管理员权限）", drive))?;
    Ok(h)
}

pub fn query_journal(vol: HANDLE) -> Result<USN_JOURNAL_DATA_V0> {
    let mut data = unsafe { std::mem::zeroed::<USN_JOURNAL_DATA_V0>() };
    let mut ret = 0u32;
    unsafe {
        DeviceIoControl(
            vol,
            FSCTL_QUERY_USN_JOURNAL,
            None,
            0,
            Some(&mut data as *mut _ as *mut c_void),
            std::mem::size_of::<USN_JOURNAL_DATA_V0>() as u32,
            Some(&mut ret),
            None,
        )
    }
    .context("FSCTL_QUERY_USN_JOURNAL 失败（该卷可能未启用 USN 日志）")?;
    Ok(data)
}

fn win32_code(e: &windows::core::Error) -> u32 {
    (e.code().0 as u32) & 0xFFFF
}

/// 日志滚动/删除/未激活等“日志不可用”错误码
fn journal_unavailable(code: u32) -> bool {
    matches!(code, 19 | 38 | 1178 | 1179)
}

fn u16le(b: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([b[off], b[off + 1]])
}

fn u32le(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

fn u64le(b: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(b[off..off + 8].try_into().unwrap())
}

fn i64le(b: &[u8], off: usize) -> i64 {
    i64::from_le_bytes(b[off..off + 8].try_into().unwrap())
}

struct ReadOut {
    recs: HashMap<FileRef, i64>,
    records_read: u64,
    skipped_v4: u64,
    filtered_reason: u64,
    skipped_usns: u64,
    skips: u32,
    truncated: bool,
}

/// 本机日志实测 V2/V3 记录混存；NTFS 对跨版本区间（如 min2 max3）返回
/// ERROR_INVALID_PARAMETER，但单版本读取会跳过其它版本的记录。
/// 因此分两条通道读取后合并。
#[derive(Clone, Copy)]
enum ReadMode {
    V3Only,
    V2Only,
}

/// 从 start 读到 stop（NextUsn 快照），按 FileRef 去重保留最新时间戳。
/// 返回最终读到的 USN 位置（下轮起点/书签）。
fn read_journal_range(
    vol: HANDLE,
    journal: &USN_JOURNAL_DATA_V0,
    start: i64,
    stop: i64,
    now: i64,
    mode: ReadMode,
    out: &mut ReadOut,
) -> Result<i64> {
    let mut buf: Vec<u64> = vec![0; BUF_U64S];
    let mut cur = start;
    while cur < stop {
        if out.records_read >= RECORD_CAP {
            out.truncated = true;
            break;
        }
        let input = READ_USN_JOURNAL_DATA_V1 {
            StartUsn: cur,
            ReasonMask: REASON_MASK,
            ReturnOnlyOnClose: 0,
            Timeout: 0,
            BytesToWaitFor: 0,
            UsnJournalID: journal.UsnJournalID,
            MinMajorVersion: 3,
            MaxMajorVersion: 3,
        };
        // V2Only：只传 40 字节（V0 布局），内核按旧语义只返回 V2 记录
        let in_size = match mode {
            ReadMode::V3Only => std::mem::size_of::<READ_USN_JOURNAL_DATA_V1>(),
            ReadMode::V2Only => 40,
        };
        let mut ret = 0u32;
        let r = unsafe {
            DeviceIoControl(
                vol,
                FSCTL_READ_USN_JOURNAL,
                Some(&input as *const _ as *const c_void),
                in_size as u32,
                Some(buf.as_mut_ptr() as *mut c_void),
                (BUF_U64S * 8) as u32,
                Some(&mut ret),
                None,
            )
        };
        if let Err(e) = r {
            let code = win32_code(&e);
            if journal_unavailable(code) {
                anyhow::bail!("USN 日志不可用（错误码 {code}），可能已被删除或滚动");
            }
            if code == ERROR_INVALID_PARAMETER && out.skips < SKIP_MAX {
                // 阻塞点：该起点处内核返回 INVALID_PARAMETER。跳过一段再试。
                out.skips += 1;
                let jumped = SKIP_STEP;
                cur += jumped;
                out.skipped_usns += jumped as u64;
                log::warn(format!(
                    "读取在 StartUsn={} 遇 87，跳过 {} 重试（{}/{}）",
                    cur - jumped,
                    jumped,
                    out.skips,
                    SKIP_MAX
                ));
                continue;
            }
            let raw = e.code().0 as u32;
            return Err(e).with_context(|| {
                format!("FSCTL_READ_USN_JOURNAL 失败（win32={code} hr=0x{raw:08x}，StartUsn={cur}）")
            });
        }
        let n = ret as usize;
        if n <= 8 {
            break; // 快照范围内暂无更多记录
        }
        let bytes = unsafe { std::slice::from_raw_parts(buf.as_ptr() as *const u8, n) };
        // 输出缓冲的前 8 字节是内核给出的“续读点”（下一起始 USN），
        // 不是首条记录的 USN；直接用它推进游标，切勿再叠加记录步长。
        let base_usn = i64le(bytes, 0);
        let mut off = 8usize;
        while off + 8 <= n {
            let reclen = u32le(bytes, off) as usize;
            if reclen < 60 || off + reclen > n {
                break; // 防御：记录不完整
            }
            let major = u16le(bytes, off + 4);
            out.records_read += 1;
            match major {
                2 => {
                    // V2: FileRef@8(u64) Usn@24 TimeStamp@32 Reason@40 FnLen@56 FnOff@58
                    let reason = u32le(bytes, off + 40);
                    if reason & REASON_MASK != 0 {
                        let id = FileRef::V2(u64le(bytes, off + 8));
                        let ts = clamp_ts(i64le(bytes, off + 32), now);
                        let old = out.recs.get_mut(&id);
                        match old {
                            Some(t) if *t >= ts => {}
                            Some(t) => *t = ts,
                            None => {
                                out.recs.insert(id, ts);
                            }
                        }
                    } else {
                        out.filtered_reason += 1;
                    }
                }
                3 => {
                    // V3: FileRef@8(128bit) Usn@40 TimeStamp@48 Reason@56 FnLen@72 FnOff@74
                    let reason = u32le(bytes, off + 56);
                    if reason & REASON_MASK != 0 {
                        let mut idb = [0u8; 16];
                        idb.copy_from_slice(&bytes[off + 8..off + 24]);
                        // NTFS 上 128 位 ID 的高 8 字节为零时即 64 位引用号，
                        // 归一化成 V2 键以便两条通道互相去重
                        let id = if idb[8..].iter().all(|&b| b == 0) {
                            FileRef::V2(u64::from_le_bytes(idb[..8].try_into().unwrap()))
                        } else {
                            FileRef::V3(idb)
                        };
                        let ts = clamp_ts(i64le(bytes, off + 48), now);
                        match out.recs.get_mut(&id) {
                            Some(t) if *t >= ts => {}
                            Some(t) => *t = ts,
                            None => {
                                out.recs.insert(id, ts);
                            }
                        }
                    } else {
                        out.filtered_reason += 1;
                    }
                }
                _ => out.skipped_v4 += 1,
            }
            let stride = (reclen + 7) & !7;
            off += stride;
        }
        if base_usn <= cur {
            break; // 防御：续读点未前进，避免死循环
        }
        cur = base_usn;
    }
    Ok(cur)
}

fn clamp_ts(filetime: i64, now: i64) -> i64 {
    if filetime <= 0 {
        return now;
    }
    let unix = util::filetime_to_unix(filetime);
    if unix > now || unix < 0 {
        now
    } else {
        unix
    }
}

/// 通过卷句柄 + 文件 ID 打开文件，取当前大小与当前路径。
fn resolve_file(vol: HANDLE, id: &FileRef, prefix: &str, drive: &str) -> Option<(String, u64)> {
    unsafe {
        let mut desc = std::mem::zeroed::<FILE_ID_DESCRIPTOR>();
        desc.dwSize = std::mem::size_of::<FILE_ID_DESCRIPTOR>() as u32;
        match id {
            FileRef::V2(v) => {
                desc.Type = FileIdType;
                desc.Anonymous.FileId = *v as i64;
            }
            FileRef::V3(b) => {
                desc.Type = ExtendedFileIdType;
                desc.Anonymous.ExtendedFileId = FILE_ID_128 { Identifier: *b };
            }
        }
        let h = OpenFileById(
            vol,
            &desc,
            FILE_READ_ATTRIBUTES.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            FILE_FLAGS_AND_ATTRIBUTES(0),
        )
        .ok()?;
        let mut info = std::mem::zeroed::<windows::Win32::Storage::FileSystem::BY_HANDLE_FILE_INFORMATION>();
        let size_ok = GetFileInformationByHandle(h, &mut info).is_ok();
        let mut wbuf = vec![0u16; 1024];
        let n = GetFinalPathNameByHandleW(h, &mut wbuf, VOLUME_NAME_NT);
        let _ = CloseHandle(h);
        if n == 0 || n as usize >= wbuf.len() {
            return None;
        }
        if !size_ok {
            return None;
        }
        let size = ((info.nFileSizeHigh as u64) << 32) | info.nFileSizeLow as u64;
        let dev = String::from_utf16_lossy(&wbuf[..n as usize]);
        Some((util::to_display(&dev, prefix, drive), size))
    }
}

/// 读取书签 → 读取增量区间 → 解析去重 → 按文件解析路径与大小 → 入库并推进书签。
/// 全程 best-effort：内部问题只记日志，不向外抛（除打不开卷外）。
pub fn backfill(db: &mut Db, drive: &str, min_bytes: u64) -> Result<Option<BackfillStats>> {
    let now = chrono::Local::now().timestamp();
    let vol = open_volume(drive)?;
    let r = backfill_inner(db, drive, min_bytes, vol, now);
    let _ = unsafe { CloseHandle(vol) };
    // Ok(None) 表示"这盘没有 USN 日志，压根没跑"，记时间就成了谎话。
    if let Ok(Some(_)) = &r {
        if let Err(e) = db.meta_set("last_backfill", &now.to_string()) {
            log::warn(format!("记录回填时间失败：{e}"));
        }
    }
    r
}

fn backfill_inner(
    db: &mut Db,
    drive: &str,
    min_bytes: u64,
    vol: HANDLE,
    now: i64,
) -> Result<Option<BackfillStats>> {
    let journal = match query_journal(vol) {
        Ok(j) => j,
        Err(e) => {
            log::warn(format!("跳过回填：{e}"));
            return Ok(None);
        }
    };
    let mut stats = BackfillStats {
        journal_id: journal.UsnJournalID,
        first_usn: journal.FirstUsn,
        next_usn: journal.NextUsn,
        mode: String::new(),
        records_read: 0,
        skipped_v4: 0,
        filtered_reason: 0,
        skipped_usns: 0,
        unique_files: 0,
        unresolvable: 0,
        excluded: 0,
        below_floor: 0,
        inserted: 0,
    };

    let last = db
        .meta_get("last_usn")?
        .and_then(|s| s.parse::<i64>().ok());
    let jid = db
        .meta_get("journal_id")?
        .and_then(|s| s.parse::<u64>().ok());

    // 决定起点与模式
    let start: Option<i64> = match (last, jid) {
        (None, _) => {
            stats.mode = "baseline".into();
            None
        }
        (Some(_), None) => {
            stats.mode = "baseline".into();
            None
        }
        (Some(_), Some(j)) if j != journal.UsnJournalID => {
            stats.mode = "journal-recreated".into();
            None
        }
        (Some(l), _) if l < journal.FirstUsn => {
            log::warn(format!(
                "USN 日志已滚动（书签 {l} < FirstUsn {}），从 FirstUsn 继续",
                journal.FirstUsn
            ));
            stats.mode = "wrapped".into();
            Some(journal.FirstUsn)
        }
        (Some(l), _) if l >= journal.NextUsn => {
            stats.mode = "no-gap".into();
            None
        }
        (Some(l), _) => {
            stats.mode = "gap".into();
            Some(l)
        }
    };

    let Some(start) = start else {
        db.meta_set("last_usn", &journal.NextUsn.to_string())?;
        db.meta_set("journal_id", &journal.UsnJournalID.to_string())?;
        log::info(format!(
            "回填模式 {}：仅建立基线，书签 = NextUsn {}",
            stats.mode, journal.NextUsn
        ));
        return Ok(Some(stats));
    };

    log::info(format!(
        "回填开始：模式 {}，区间 {}..{}（约 {} 个 USN）",
        stats.mode,
        start,
        journal.NextUsn,
        journal.NextUsn.saturating_sub(start)
    ));

    let mut out = ReadOut {
        recs: HashMap::new(),
        records_read: 0,
        skipped_v4: 0,
        filtered_reason: 0,
        skipped_usns: 0,
        skips: 0,
        truncated: false,
    };
    // V3 通道：失败则降级为仅 V2（本次区间丢失 V3 记录，但避免卡死书签）
    let end_v3 = match read_journal_range(
        vol,
        &journal,
        start,
        journal.NextUsn,
        now,
        ReadMode::V3Only,
        &mut out,
    ) {
        Ok(p) => Some(p),
        Err(e) => {
            log::warn(format!("V3 通道读取失败，本次仅回填 V2 记录：{e}"));
            None
        }
    };
    let end_v2 = match read_journal_range(
        vol,
        &journal,
        start,
        journal.NextUsn,
        now,
        ReadMode::V2Only,
        &mut out,
    ) {
        Ok(p) => p,
        Err(e) => {
            log::warn(format!("回填读取中断，本次不推进书签：{e}"));
            if journal_unavailable_from_err(&e) {
                db.meta_set("last_usn", &journal.NextUsn.to_string())?;
                db.meta_set("journal_id", &journal.UsnJournalID.to_string())?;
                log::warn("日志不可用，重置基线到 NextUsn".to_string());
            }
            return Ok(Some(stats));
        }
    };
    let end_pos = end_v3.unwrap_or(end_v2).max(end_v2);
    stats.records_read = out.records_read;
    stats.skipped_v4 = out.skipped_v4;
    stats.filtered_reason = out.filtered_reason;
    stats.skipped_usns = out.skipped_usns;
    stats.unique_files = out.recs.len() as u64;
    if out.skips > 0 {
        log::warn(format!(
            "回填期间跳过 {} 个阻塞点（共 {} USN），这些区间内的改动未补录",
            out.skips, out.skipped_usns
        ));
    }
    if out.truncated {
        log::warn(format!("回填记录数达到上限 {RECORD_CAP}，已截断"));
    }

    let prefix = util::device_prefix(drive)?;
    let mut rows: Vec<UsnRow> = Vec::new();
    let lower_excl: Vec<String> = EXCLUDE_SUBSTR.iter().map(|s| s.to_string()).collect();
    for (id, ts) in out.recs.iter() {
        let Some((disp, size)) = resolve_file(vol, id, &prefix, drive) else {
            stats.unresolvable += 1;
            continue;
        };
        let lower = disp.to_lowercase();
        if lower_excl.iter().any(|e| lower.contains(e)) {
            stats.excluded += 1;
            continue;
        }
        if size < min_bytes {
            stats.below_floor += 1;
            continue;
        }
        rows.push(UsnRow {
            ts: util::minute_bucket(*ts),
            path: disp,
            bytes: size,
        });
    }

    db.insert_usn_changes(&rows, end_pos.max(start) as u64, journal.UsnJournalID)?;
    stats.inserted = rows.len() as u64;
    log::info(format!(
        "回填完成：读取 {} 条记录（跳过 V4 {}，原因过滤 {}），去重后 {} 个文件，\
         无法解析 {}，排除 {}，低于阈值 {}，入库 {}；书签更新至 {}",
        stats.records_read,
        stats.skipped_v4,
        stats.filtered_reason,
        stats.unique_files,
        stats.unresolvable,
        stats.excluded,
        stats.below_floor,
        stats.inserted,
        end_pos.max(start)
    ));
    Ok(Some(stats))
}

fn journal_unavailable_from_err(e: &anyhow::Error) -> bool {
    e.to_string().contains("日志不可用")
}

/// 优雅停机时调用：把书签推到当前 NextUsn（运行期间 ETW 已覆盖）。
pub fn update_bookmark(db: &mut Db, drive: &str) -> Result<()> {
    let vol = open_volume(drive)?;
    let r = query_journal(vol);
    let _ = unsafe { CloseHandle(vol) };
    let j = r?;
    db.meta_set("last_usn", &j.NextUsn.to_string())?;
    db.meta_set("journal_id", &j.UsnJournalID.to_string())?;
    log::info(format!("书签更新：last_usn = {}", j.NextUsn));
    Ok(())
}
