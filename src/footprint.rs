// 现值口径：writes / usn_changes 记的是"某分钟内写过多少字节"，临时文件和安装包被回收之后
// 仍会留在排行里，所以写入量最大的目录未必真的占着磁盘。这里按需回磁盘量一次"现在还剩多少"，
// 两个口径并排才能区分「一直在长的日志」和「写完就删的过路货」。
//
// 扫描必须自带预算：排行里的目录可能是 Edge 的 User Data（几十万文件）。预算用满时结果是下界，
// 用 State::Partial 明说，绝不把下界当精确值展示。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum State {
    /// 读到了，且没触及预算
    Exact,
    /// 条目/深度/时限用满，bytes 只是下界
    Partial,
    /// 路径已不存在——写入后被删掉了（临时文件、安装器包的典型结局）
    Missing,
    /// 无权限读取，需要管理员
    Denied,
    /// 根本没测：调用方显式跳过，或这行排在现值预算之外
    #[default]
    Unmeasured,
}

impl State {
    pub fn label(self) -> &'static str {
        match self {
            State::Exact => "完整",
            State::Partial => "部分",
            State::Missing => "已删除",
            State::Denied => "需管理员",
            State::Unmeasured => "未测",
        }
    }

    pub fn slug(self) -> &'static str {
        match self {
            State::Exact => "exact",
            State::Partial => "partial",
            State::Missing => "missing",
            State::Denied => "denied",
            State::Unmeasured => "unmeasured",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    pub bytes: u64,
    pub files: u64,
    pub state: State,
}

impl Usage {
    /// 排行表里的现值单元格：精确值直接给数，下界加 ≈，无值给原因而不是 0——
    /// 0 会被读成"这个目录已经空了"，而实际可能是没权限或压根没测。
    pub fn cell(&self) -> String {
        match self.state {
            State::Exact => crate::util::human_bytes(self.bytes),
            State::Partial if self.bytes == 0 => "≈0 B".to_string(),
            State::Partial => format!("≈{}", crate::util::human_bytes(self.bytes)),
            State::Missing => "已删除".to_string(),
            State::Denied => "需管理员".to_string(),
            State::Unmeasured => "—".to_string(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Budget {
    /// 单个目录最多枚举多少条目
    pub max_entries: u64,
    /// 单个目录最多下潜多少层
    pub max_depth: u32,
    /// 整批（一次 of_paths）最多花多少毫秒，不是单条目录的配额
    pub max_millis: u64,
}

impl Budget {
    /// CLI 一次性查询：整批时限给得宽，宁可多扫一会儿也别动辄给下界
    pub const CLI: Budget = Budget {
        max_entries: 3_000_000,
        max_depth: 32,
        max_millis: 2500,
    };
    /// Web 交互：请求由单线程串行处理（见 web.rs 头注释），时限必须小，
    /// 否则一次自动刷新就能把排在它后面的所有请求堵住。
    pub const WEB: Budget = Budget {
        max_entries: 400_000,
        max_depth: 32,
        max_millis: 500,
    };
}

/// 一批扫描共享的截止时间。预算按批算而不是按目录算：
/// 30 个目录各给 1.5s，最坏 45s，那和没有预算没有区别。
#[derive(Clone, Copy)]
struct Run {
    b: Budget,
    deadline: Instant,
}

impl Run {
    fn start(b: Budget) -> Self {
        Self {
            b,
            deadline: Instant::now() + Duration::from_millis(b.max_millis),
        }
    }

    fn of(&self, path: &str) -> Usage {
        let left = self.deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Usage::default();
        }
        of_path_within(path, self.b, left)
    }
}

/// 单个路径的现值：文件按 stat，目录递归。传排行里的路径即可，两种维度都吃得下。
/// 生产代码一律走 of_paths（整批共享时限），这里只服务单路径断言。
fn of_path_within(path: &str, b: Budget, limit: Duration) -> Usage {
    let p = Path::new(path);
    let md = match std::fs::metadata(p) {
        Ok(md) => md,
        Err(e) => return Usage {
            bytes: 0,
            files: 0,
            state: state_of_stat_err(&e),
        },
    };
    // fold_dirs 会把盘符根下的文件（C:\a.txt）整条当"目录"输出，所以这里可能收到文件路径
    if md.is_dir() {
        walk(p, b, limit)
    } else if md.is_file() {
        Usage {
            bytes: md.len(),
            files: 1,
            state: State::Exact,
        }
    } else {
        Usage {
            bytes: 0,
            files: 0,
            state: State::Missing,
        }
    }
}

/// 并行取多个路径的现值，整批共享 Budget::max_millis。
/// 排行一次要量几十到几百个目录，串行会被单个大目录拖垮总耗时。
pub fn of_paths(paths: &[String], b: Budget) -> Vec<Usage> {
    let n = paths.len();
    if n == 0 {
        return Vec::new();
    }
    let cpus = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    let workers = cpus.clamp(1, 8).min(n);
    let run = Run::start(b);
    let next = AtomicUsize::new(0);
    let picked = std::thread::scope(|s| {
        let handles: Vec<_> = (0..workers)
            .map(|_| {
                s.spawn(|| {
                    let mut local: Vec<(usize, Usage)> = Vec::new();
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        if i >= n {
                            break;
                        }
                        local.push((i, run.of(&paths[i])));
                    }
                    local
                })
            })
            .collect();
        let mut out: Vec<Option<Usage>> = vec![None; n];
        for h in handles {
            match h.join() {
                Ok(local) => {
                    for (i, u) in local {
                        out[i] = Some(u);
                    }
                }
                // 扫描线程理论上不会 panic；真发生了也不能让整张排行消失，缺的格子按未测处理
                Err(_) => {}
            }
        }
        out
    });
    picked
        .into_iter()
        .map(|u| u.unwrap_or_default())
        .collect()
}

/// 带存活期的现值缓存，只给 Web 用：自动刷新会把同一批目录反复扫一遍。
/// CLI 不缓存——它是一次性进程，且"现在还剩多少"要的就是当前值。
pub struct Cache {
    ttl: Duration,
    map: std::sync::Mutex<std::collections::HashMap<String, (Instant, Usage)>>,
}

const CACHE_MAX: usize = 4096;
/// 预算内扫不完的目录（典型是 C:\Windows\WinSxS，16 万个子目录）每次重扫都只会撞上同一道时限、
/// 得到同一个下界，60 秒一刷等于每分钟白开几万个目录句柄。这类结果存得更久。
const PARTIAL_TTL_FACTOR: u32 = 15;

impl Cache {
    pub fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            map: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    fn ttl_for(&self, u: &Usage) -> Duration {
        if u.state == State::Partial {
            self.ttl * PARTIAL_TTL_FACTOR
        } else {
            self.ttl
        }
    }

    pub fn of_paths(&self, paths: &[String], b: Budget) -> Vec<Usage> {
        let mut out = Vec::with_capacity(paths.len());
        let mut missing: Vec<usize> = Vec::new();
        {
            let guard = self.lock();
            for p in paths {
                match guard.get(p) {
                    Some((at, u)) if at.elapsed() < self.ttl_for(u) => out.push(*u),
                    _ => {
                        out.push(Usage::default());
                        missing.push(out.len() - 1);
                    }
                }
            }
        }
        if missing.is_empty() {
            return out;
        }
        let jobs: Vec<String> = missing.iter().map(|&i| paths[i].clone()).collect();
        let fresh = of_paths(&jobs, b);
        {
            let mut guard = self.lock();
            if guard.len() + fresh.len() > CACHE_MAX {
                // 换一批筛选条件就会积累一堆永不复用的键；宁可重新扫，也不让内存无限长
                guard.clear();
            }
            for (i, u) in missing.iter().zip(fresh.iter()) {
                out[*i] = *u;
                // 整批时限先到、根本没轮到的路径不是结果，缓存它会让「—」顶着不重试
                if u.state != State::Unmeasured {
                    guard.insert(paths[*i].clone(), (Instant::now(), *u));
                }
            }
        }
        out
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, std::collections::HashMap<String, (Instant, Usage)>> {
        // 别的线程 panic 不会留下"半条目录记录"这种有意义状态，中毒的锁继续用即可
        self.map.lock().unwrap_or_else(|p| p.into_inner())
    }
}

/// 往一行排行 JSON 里并排挂上现值。CLI 的 --json 与 Web API 共用这一个实现，
/// 否则两处对"没测到"的表达迟早分叉。未测时三字段一律 null 而不是 0：
/// 0 会让脚本把"没扫"读成"不占磁盘"。
pub fn attach_usage(mut v: serde_json::Value, u: Option<&Usage>) -> serde_json::Value {
    let Some(u) = u.filter(|u| u.state != State::Unmeasured) else {
        for k in ["usage_bytes", "usage_files", "usage_state"] {
            v[k] = serde_json::Value::Null;
        }
        return v;
    };
    v["usage_bytes"] = serde_json::Value::from(u.bytes);
    v["usage_files"] = serde_json::Value::from(u.files);
    v["usage_state"] = serde_json::Value::from(u.state.slug());
    v
}

fn state_of_stat_err(e: &std::io::Error) -> State {
    match e.kind() {
        std::io::ErrorKind::NotFound => State::Missing,
        // Windows 把路径非法/语法错误也报成 NotFound 之外的 kind，一并归到"不在盘上"
        std::io::ErrorKind::InvalidFilename => State::Missing,
        _ => State::Denied,
    }
}

fn walk(root: &Path, b: Budget, limit: Duration) -> Usage {
    let start = Instant::now();
    let mut bytes = 0u64;
    let mut files = 0u64;
    let mut scanned = 0u64;
    let mut state = State::Exact;
    let mut stack: Vec<(PathBuf, u32)> = vec![(root.to_path_buf(), 0)];

    while let Some((dir, depth)) = stack.pop() {
        if start.elapsed() >= limit || scanned > b.max_entries {
            state = State::Partial;
            break;
        }
        let rd = match std::fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(e) => {
                if depth == 0 {
                    // 根目录 stat 成功但列不出来＝遍历权限被拒；此时给 0 才是可信答案
                    return Usage {
                        bytes: 0,
                        files: 0,
                        state: state_of_stat_err(&e),
                    };
                }
                state = State::Partial;
                continue;
            }
        };
        for entry in rd.flatten() {
            scanned += 1;
            // 条目预算要在枚举途中就断：等整个目录列完再判，大目录的预算等于没生效
            if scanned > b.max_entries {
                state = State::Partial;
                break;
            }
            let Ok(ft) = entry.file_type() else { continue };
            // 符号链接与重解析点不跟随：Junction 环会让扫描永不结束
            if ft.is_symlink() {
                continue;
            }
            if ft.is_dir() {
                if depth + 1 <= b.max_depth {
                    stack.push((entry.path(), depth + 1));
                } else {
                    state = State::Partial;
                }
                continue;
            }
            if let Ok(md) = entry.metadata() {
                bytes += md.len();
                files += 1;
            }
        }
    }
    Usage { bytes, files, state }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_tree(name: &str, files: &[(&str, usize)]) -> PathBuf {
        let root = std::env::temp_dir().join(format!("diskwatch-footprint-{}", name));
        let _ = std::fs::remove_dir_all(&root);
        for (rel, size) in files {
            let p = root.join(rel.replace('\\', "/"));
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, vec![7u8; *size]).unwrap();
        }
        root
    }

    fn full() -> Budget {
        Budget {
            max_entries: 100_000,
            max_depth: 32,
            max_millis: 5_000,
        }
    }

    /// 单路径取现值：与 of_paths 同一条实现，只是自己占满整批预算。
    fn one(path: &str, b: Budget) -> Usage {
        of_path_within(path, b, Duration::from_millis(b.max_millis))
    }

    #[test]
    fn dir_sums_recursively_across_subdirs() {
        let root = tmp_tree(
            "recur",
            &[("a.bin", 100), (r"sub\b.bin", 200), (r"sub\deep\c.bin", 300)],
        );
        let u = one(&root.display().to_string(), full());
        assert_eq!(u.state, State::Exact);
        assert_eq!(u.bytes, 600);
        assert_eq!(u.files, 3);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn file_path_is_stated_not_walked() {
        let root = tmp_tree("afile", &[("x.bin", 42)]);
        let file = root.join("x.bin");
        let u = one(&file.display().to_string(), full());
        assert_eq!((u.bytes, u.files, u.state), (42, 1, State::Exact));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn missing_path_reports_missing_not_zero() {
        let u = one(r"C:\diskwatch-definitely-not-here\nope.bin", full());
        assert_eq!(u.state, State::Missing);
        assert_eq!(u.cell(), "已删除");
    }

    #[test]
    fn entry_budget_degrades_to_partial() {
        let root = tmp_tree("budget", &[("a.bin", 10), ("b.bin", 10), ("c.bin", 10)]);
        let tight = Budget {
            max_entries: 1,
            max_depth: 32,
            max_millis: 5_000,
        };
        let u = one(&root.display().to_string(), tight);
        assert_eq!(u.state, State::Partial);
        assert!(u.bytes < 30, "下界必须小于真值");
        assert!(u.cell().starts_with('≈'));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn exhausted_batch_budget_marks_rest_unmeasured() {
        let root = tmp_tree("expired", &[("a.bin", 100)]);
        let p = root.display().to_string();
        // 时限为 0：一个目录都不该扫，且绝不能伪造成 0 字节"完整"
        let zero = Budget {
            max_entries: 100_000,
            max_depth: 32,
            max_millis: 0,
        };
        let out = of_paths(&[p], zero);
        assert_eq!(out[0].state, State::Unmeasured);
        assert_eq!(out[0].cell(), "—");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn depth_budget_degrades_to_partial() {
        let root = tmp_tree("depth", &[(r"sub\deep\c.bin", 300), ("a.bin", 100)]);
        let shallow = Budget {
            max_entries: 100_000,
            max_depth: 0,
            max_millis: 5_000,
        };
        let u = one(&root.display().to_string(), shallow);
        assert_eq!(u.state, State::Partial);
        assert_eq!(u.bytes, 100);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn of_paths_keeps_input_order() {
        let root = tmp_tree("multi", &[("a.bin", 100), ("b.bin", 200)]);
        let p = root.display().to_string();
        let out = of_paths(&[p.clone(), p.clone()], full());
        assert_eq!(out.len(), 2);
        assert_eq!((out[0].bytes, out[1].bytes), (300, 300));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn partial_results_outlive_exact_ttl() {
        let root = tmp_tree("partial-ttl", &[("a.bin", 10), ("b.bin", 10), ("c.bin", 10)]);
        let p = root.display().to_string();
        let tight = Budget {
            max_entries: 1,
            max_depth: 32,
            max_millis: 5_000,
        };
        let c = Cache::new(Duration::from_millis(20));
        let first = c.of_paths(&[p.clone()], tight)[0];
        assert_eq!(first.state, State::Partial);
        std::fs::write(root.join("d.bin"), vec![1u8; 999]).unwrap();
        // 已过普通存活期，但下界结果按 PARTIAL_TTL_FACTOR 延长：不应重扫
        std::thread::sleep(Duration::from_millis(40));
        assert_eq!(c.of_paths(&[p], tight)[0], first);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn unmeasured_is_never_cached() {
        let root = tmp_tree("no-cache-unmeasured", &[("a.bin", 100)]);
        let p = root.display().to_string();
        let c = Cache::new(Duration::from_secs(60));
        let zero = Budget {
            max_entries: 100_000,
            max_depth: 32,
            max_millis: 0,
        };
        assert_eq!(c.of_paths(&[p.clone()], zero)[0].state, State::Unmeasured);
        // 下一次有预算了就必须真扫，而不是从缓存里拿出上次的「—」
        assert_eq!(c.of_paths(&[p], full())[0].bytes, 100);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn cache_serves_then_expires() {
        let root = tmp_tree("cache", &[("a.bin", 100)]);
        let p = root.display().to_string();
        let c = Cache::new(Duration::from_millis(30));
        assert_eq!(c.of_paths(&[p.clone()], full())[0].bytes, 100);
        std::fs::write(root.join("b.bin"), vec![1u8; 50]).unwrap();
        // 存活期内仍返回旧值：缓存的意义就是不去重复扫盘
        assert_eq!(c.of_paths(&[p.clone()], full())[0].bytes, 100);
        std::thread::sleep(Duration::from_millis(40));
        assert_eq!(c.of_paths(&[p], full())[0].bytes, 150);
        std::fs::remove_dir_all(&root).unwrap();
    }
}
