# diskwatch

Windows 磁盘写入监控：记录**什么文件、什么进程、在什么时间写了多大**，并提供实时 Web 界面、告警、HTML 报表与 CSV 导出。本机诊断工具，Windows 专用。

## 设计目的

本工具为本机运行的 agent 工具（如 Claude Code）服务：agent 干活时会在 C 盘留下大量写入
（checkpoint / 会话快照、工具输出、日志、构建产物等）。一旦**用量激增**，需要快速回答一个问题——
**这几百 MB / 几 GB 是谁写的、刚才写到了哪里？**

- 没有本工具：只能全盘扫目录、按修改时间或体积猜，读取大量路径与文件，白白烧 token；
- 有了本工具：agent 直接查一个很小的 SQLite 库（只存达到阈值的分钟级聚合），
  命中写入元凶「路径 + 进程 + 时间」，带着**精确路径**去做定点清理（删缓存、清日志、移产物），一次到位。

四条能力对应这个流程：

- **平时记录**：`collect` 常驻，通过 ETW 记下每次达到阈值的写入（进程 + 路径 + 分钟时间戳）；
- **激增溯源**：Web 界面 / `top --by … --json` 一查即答，agent 拿到的就是干净 JSON，无需再走 HTTP；
- **空窗补账**：agent 停机期间的数据由 NTFS USN 日志**离线补录**，重启后依然能溯源；
- **区分真占用**：排行在写入量之外并排给出**当前磁盘占用**——写入量说明谁在写，现值说明谁真的还占着盘，照着前者去清理会把早已删除的临时文件也当成大户。

## 功能特性

- **实时采集**：通过 ETW（Event Tracing for Windows）内核 FileIO/Filename 提供程序捕获文件写入事件，无需轮询（使用 `ferrisetw`）。
- **离线补录**：从 NTFS USN 日志回填监控空窗期（停机、未采集时段）的变更，与实时视图合并呈现。
- **按分钟聚合**：单文件单分钟累计写入达到阈值（默认 1 MB）才入库，控制数据量。
- **当前磁盘占用（现值）**：排行与 CSV 在写入量之外并排给出「现在还占多少」。写入量只说明曾经写过，临时文件、安装器包、升级残留删掉之后仍会留在排行里；现值按需回磁盘扫描，直接区分「一直在长的日志」和「写完就删的过路货」。
- **大写入告警**：单文件单分钟写入 ≥ 100 MB（可配置，0 关闭）触发告警：写 `logs/alerts.log`、Windows 事件日志（事件 ID 100）、Web 界面可见。
- **数据保留**：默认保留 30 天，运行中每天清理一次 + 退出时清理（`writes` / `usn_changes` / `alerts` 同口径）。
- **内置 Web 界面**：只监听 `127.0.0.1:8421`，只读 GET，实时写入统计、写入节奏柱图、离线补录、告警查询，一页完成。
- **HTML 报表**：任何时间点对最近 N 小时生成静态统计报告。
- **CSV 导出**：Web 按钮导当前视图；CLI 一次导出 `writes` / `usn` / `alerts` 三份，Excel 可直接打开。
- **Windows 服务部署**：支持 `sc` 服务安装脚本，含失败自动重启与延迟自动启动。

## 工作原理

两条采集通道：

1. **实时通道（ETW）**：`collect` 以管理员权限启动用户态 ETW trace，监听内核文件的写入事件，映射 file key → 路径、PID → 进程名，按 `(分钟桶, 路径, 进程)` 聚合。
2. **离线回填（USN）**：`backfill` 读 NTFS USN 日志的变更记录，从上次书签（`meta.last_usn`）往前/往后扫描，补齐停机期间未采集的写入，并按同一阈值入库。

聚合后的记录带有 `ts`（分钟级时间戳）、`path`、`process`、累计 `bytes` 与 `ops`。

## 构建

需要 Rust 工具链（edition 2024）。

```sh
cargo build --release
```

产物：`target/release/diskwatch.exe`。部署时复制到 `bin/`（`bin/diskwatch.exe`），运行数据与日志默认落在程序目录旁的 `data/` `logs/` `reports/` `exports/`。

## 使用

```
diskwatch <SUBCOMMAND>

SUBCOMMANDS:
  collect   常驻采集文件写入事件（需要管理员权限）
  report    生成 HTML 统计报告
  backfill  手工执行一次 USN 离线回填（观测口，需要管理员权限）
  service   作为 Windows 服务运行（仅由服务控制管理器启动）
  web       只启动 Web 界面（免管理员权限，开发/手工查看通道）
  top       查询写入量排行
  export    导出 CSV（writes / usn / alerts 各一份）
```

### 常用示例

```sh
# 常驻采集 C 盘写入，同时提供 Web 界面
diskwatch.exe collect --drive C --web

# 只跑 60 秒退出（冒烟测试）
diskwatch.exe collect --duration 60

# 手工回填一次 USN 日志（对齐上次书签）
diskwatch.exe backfill --drive C

# 最近 24 小时写入量排行（按文件 / 按目录），末列是当前磁盘占用
diskwatch.exe top --last 24h --by file
diskwatch.exe top --last 24h --by dir --contains C:\Windows

# 目录很大时扫描有预算，只要写入量、不扫盘就用它
diskwatch.exe top --last 24h --by dir --no-usage

# 生成最近 24 小时 HTML 报表
diskwatch.exe report --hours 24

# agent / 脚本直接消费的 JSON（文件 / 目录 / 进程排行，仅 stdout）
# 现值以 usage_bytes / usage_files / usage_state 三个字段并排给出，未测时为 null
diskwatch.exe top --last 24h --by file --json
diskwatch.exe top --last 24h --by dir --json
diskwatch.exe top --last 24h --by process --json

# 导出最近 7 天 CSV 到 ./exports/（writes 视图末两列是当前占用）
diskwatch.exe export --last 7d
```

| 参数 | 默认值 | 含义 |
|---|---|---|
| `--drive` | `C` | 被监控盘符 |
| `--min-bytes` | `1048576`（1 MB） | 单文件单分钟累计达到该字节数才入库 |
| `--flush-secs` | `30` | 聚合落盘间隔（秒） |
| `--keep-days` | `30` | 数据保留天数 |
| `--alert-bytes` | `104857600`（100 MB） | 单文件单分钟告警阈值，`0` 关闭 |
| `--db` | `<程序目录>/data/diskwatch.db` | SQLite 数据库路径 |
| `--no-usage` | 关闭（即默认测量现值） | `top` / `export` 跳过当前磁盘占用扫描 |

现值状态四种：`exact` 完整、`partial` 触及扫描预算（值是下界，显示为 `≈`）、`missing` 写入后已被回收（显示「已删除」）、`denied` 无权限读取（显示「需管理员」，提权重跑即可取到真值）。

详见 `diskwatch.exe --help`。

## 部署为 Windows 服务

### 一键升级（推荐）

双击 `bin\deploy-update.cmd`。脚本会自动请求管理员权限，依次执行测试、release 构建、优雅停止服务、备份旧 exe、替换、启动和 Web API 健康检查；任一步失败会自动恢复旧版。数据库与日志不会删除。

只检查脚本和构建、不触碰当前服务：

```cmd
bin\deploy-update.cmd --dry-run
```

### 初次安装或卸载

```sh
# 安装（管理员）：创建服务 DiskWatch（延迟自动启动、失败 10s/30s/60s 重启）
bin\install-service.cmd

# 卸载（管理员）：停止并删除服务，保留数据
bin\uninstall-service.cmd
```

服务以 `service` 子命令启动，硬编码三期默认参数（C 盘、1 MB 阈值、30 天保留、100 MB 告警、8421 Web 端口）。服务状态/事件写入 Windows 事件日志（事件 ID 101 服务、102 Web 端口占用）。

> 旧版基于计划任务的部署脚本保留在 `bin/install-tasks.cmd`（“DiskWatch Collect”看门狗 + 每晚 21:00 “DiskWatch Report”），安装服务时会被替换。

## Web 界面

访问 `http://127.0.0.1:8421/`（回环地址，仅本机可访问）。

- 实时写入统计卡（写入量 / 次数 / 突发柱图）
- 写入节奏柱图：按分钟桶显示，桶数量随宽度自适应，柱高平方根缩放
- 三个视图：实时写入 / 离线补录（USN）/ 告警
- 时间范围、路径过滤、按文件/目录聚合、排序切换
- 当前磁盘占用列：实时写入的按文件/按目录聚合下与写入量并排显示，目录卡片副标题同样带「现占」
- CSV 导出当前视图按钮

主要接口（均只读 GET，参数多为 `last=1h&q=子串&group=file|dir|none&sort=…&order=asc|desc&limit=…&usage=1`）：

`usage=1` 才会去磁盘测现值（默认不测，避免每次自动刷新都扫盘）；单次请求最多测前 80 行，整批扫描时限 500 ms，超出的行返回 `usage_state: null` 且不缓存（下次请求重试）。结果缓存 60 秒；预算内扫不完的目录（`partial`）缓存 15 分钟——它们每次重扫都只会撞上同一道时限、得到同一个下界。

扫描成本只看**打开了多少个文件夹**（实测约 86 µs/个）：文件大小直接取自目录枚举数据，不对每个文件另发请求。按 7 天写入量取前 80 个目录，合计约 16.8 万个文件夹，其中 `C:\Windows\WinSxS` 独占 16 万——它在任何预算下都只能给下界。

| 接口 | 说明 |
|---|---|
| `/api/ping` | 存活探测 |
| `/api/status` | 心跳、行数、配置快照 |
| `/api/overview` | 页面汇总卡片 |
| `/api/writes` | 实时写入明细/聚合 |
| `/api/usn` | 离线补录变更 |
| `/api/alerts` | 告警列表 |
| `/api/processes` | 进程维度统计 |
| `/api/export/{writes\|usn\|alerts}` | CSV 导出 |

## 数据存储

SQLite（WAL 模式，`busy_timeout=5s`），位于 `<程序目录>/data/diskwatch.db`：

| 表 | 内容 |
|---|---|
| `writes` | 实时写入聚合，`(ts, path, process)` + 累计 `bytes` / `ops` |
| `usn_changes` | USN 离线补录，`(ts, path)` 唯一 |
| `alerts` | 告警记录，`(ts, path)` 唯一，`notified` 标记已通知 |
| `meta` | 键值元数据（含 USN 书签 `last_usn` / `journal_id`） |

运行日志：`logs/collect.log`（INFO/WARN）、`logs/alerts.log`。单份超过 4 MB 即改名为 `<名>.1`（覆盖上一份）后重开，每种日志在盘上最多 8 MB。数据是诊断数据，`target/`、`bin/data|logs|reports`、`.exe` 及备份均不入版本库。

**内存**：采集器常驻占用主要是 FileKey → 路径缓存（ETW 的写入事件只带 key，名字只在打开文件时出现一次）。只缓存被监控卷的路径、去掉设备前缀，两代各 10 万条轮换，常驻上限约 27 MB；持续在写的文件每次写入都会被提回当前代，不会被挤掉。

## 项目结构

```
src/
  collect.rs    ETW 实时采集主循环 + 聚合 + 清理
  usn.rs        NTFS USN 日志读取与离线回填
  db.rs         SQLite 封装（schema、查询、清理）
  web.rs        内置 HTTP 服务（tiny_http）与 API
  report.rs     HTML 统计报表
  export.rs     CSV 导出（CLI + Web 共用）
  alert.rs      告警扫描与通知
  top.rs        写入量排行 CLI
  footprint.rs  当前磁盘占用（现值）扫描：文件 stat / 目录递归 + 预算 + Web 缓存
  csv.rs        CSV 行序列化
  service.rs    Windows 服务实现
  eventlog.rs   Windows 事件日志写入
  log.rs        运行日志（stdout + 文件，4 MB 轮转）
  util.rs       路径/时间/字节 格式化与管理员检测
  assets.rs     内嵌前端资源
assets/          前端（index.html / app.js / glass.css / web.css，随程序内嵌）
bin/            部署脚本（install-service / uninstall-service / install-tasks / collect-start）
```

## 已知取舍

- **仅 Windows / NTFS**：依赖 ETW、USN 日志与 Windows 服务 API。
- **本机诊断定位**：Web 只监听回环地址、只读、无鉴权——本机任意进程都能读到数据，这是既定取舍，不对外暴露端口。
- **告警为“抽查”口径**：USN 离线视图无进程维度（补录记录不含进程名）；告警只针对实时通道的分钟聚合。
- **现值是按需扫描，不是常驻记录**：排行查的那一刻才去读磁盘，所以它准的是"现在"，无法回放历史时点的占用。目录递归带预算（条目/深度/整批时限），大目录给的是下界 `≈`；`ProgramData`、`Windows Defender\Scans` 这类目录普通权限读不到，服务以 SYSTEM 身份跑的 Web 能读到，普通终端跑的 CLI 只会得到「需管理员」。
- **现值是文件逻辑大小之和**：不含簇对齐/slack、硬链接去重与压缩差异，因此和"属性→大小"或磁盘实占会有出入，够用来定位元凶，不够做计费口径。
- **现值只覆盖实时写入的聚合视图**：分钟明细（`group=none`）与离线补录（USN）视图不带这一列，前者路径重复出现、逐行再扫只是把重复摊到扫描上。
- **无前端防篡改设计**：`index.html` 里的标题与 DOM 由 `assets.rs` 内嵌提供，改动顺序是先改 `assets/` 再经 `assets.rs` 编译进可执行文件。

## 兼容性要求

- Windows 10 / Windows Server 2016+（使用 ETW、NTFS USN、Windows 服务、事件日志）
- `collect`、`backfill`、`service` 需管理员权限；`web`、`top`、`report`、`export` 免管理员