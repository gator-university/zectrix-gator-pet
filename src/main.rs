//! ZECTRIX Note 4 像素宠物（DeepSeek Harness 版）。
//!
//! 复用 zectrix-agent-pet 的状态模型、布局与音效逻辑，把原来的 Hub 运行时替换为
//! 本机光栅化 + zectrix-sdk 局域网直连，并把 Codex 会话跟随换成 DeepSeek Harness 跟随。

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, UNIX_EPOCH};
use dsh_pet::{
    device, layout,
    model::{Event as TaskEvent, Model, Settings, Status, Task, TitleSource},
    raster::{self, Fonts},
    sound,
    uia,
    dsh::Watcher,
};
use zectrix_sdk::{Device, ImageFormat};

fn now() -> i64 {
    chrono::Local::now().timestamp()
}

fn data_dir() -> PathBuf {
    let base = std::env::var("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."));
    base.join("zectrix-dsh-pet")
}

fn state_path() -> PathBuf {
    data_dir().join("state.json")
}

fn out_dir() -> PathBuf {
    data_dir().join("out")
}

fn dsh_root() -> PathBuf {
    if let Ok(p) = std::env::var("PET4_DSH_ROOT") {
        return PathBuf::from(p);
    }
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".dsh").join("sessions")
}

/// 本机是否还有 DeepSeek Harness 在跑。只看进程名，不读任何对话内容。
fn dsh_running() -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let out = std::process::Command::new("tasklist")
            .args(["/NH", "/FI", "IMAGENAME eq DeepSeek Harness.exe"])
            .creation_flags(CREATE_NO_WINDOW)
            .output();
        match out {
            Ok(o) => String::from_utf8_lossy(&o.stdout)
                .to_ascii_lowercase()
                .contains("deepseek harness.exe"),
            Err(_) => false,
        }
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// 探测结果缓存时长。`tasklist` 每次都要起一个进程（几十毫秒），而主循环里
/// 有很多「顺手确认 DSH 还在不在」的判断；5 秒的延迟对退出检测完全够用。
const DSH_PROBE_TTL: Duration = Duration::from_secs(5);

struct ProbeCache {
    at: Instant,
    running: bool,
}

static DSH_PROBE: OnceLock<Mutex<Option<ProbeCache>>> = OnceLock::new();

/// 带 TTL 的 `dsh_running()`：只在缓存过期时真的去查进程表。
fn dsh_running_cached() -> bool {
    let cell = DSH_PROBE.get_or_init(|| Mutex::new(None));
    let mut guard = match cell.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    if let Some(probe) = guard.as_ref() {
        if probe.at.elapsed() < DSH_PROBE_TTL {
            return probe.running;
        }
    }
    let running = dsh_running();
    match guard.as_mut() {
        Some(probe) => {
            probe.at = Instant::now();
            probe.running = running;
        }
        None => *guard = Some(ProbeCache { at: Instant::now(), running }),
    }
    running
}

fn heartbeat_path() -> PathBuf {
    data_dir().join("daemon.hb")
}

fn lock_path() -> PathBuf {
    data_dir().join("daemon.lock")
}

/// 心跳文件内容：第一行 PID，第二行时间戳。非 Windows 的抢锁兜底会读它；
/// Windows 上用内核互斥体，只有测试还在校验这个旧格式。
#[cfg(any(not(windows), test))]
fn parse_heartbeat(text: &str) -> Option<(u32, i64)> {
    let mut it = text.lines();
    let pid = it.next()?.trim().parse().ok()?;
    let ts = it.next()?.trim().parse().ok()?;
    Some((pid, ts))
}

fn write_heartbeat() {
    let body = format!("{}\n{}", std::process::id(), now());
    // 心跳写失败不影响推送：它只用于「有没有守护在跑」的判断。
    let _ = std::fs::write(heartbeat_path(), body);
}

/// 非 Windows 平台的兜底实现：真正判断 PID 归属要查系统进程表，
/// Windows 上用内核互斥体抢锁，不需要它，所以这里保持保守的 false。
#[cfg(not(windows))]
fn pid_is_ours(_pid: u32) -> bool {
    false
}

/// 把锁写盘：状态查询靠它找 PID。互斥体本身不依赖这个文件。
fn write_lock_file() {
    let _ = std::fs::write(
        lock_path(),
        format!("{}\n{}", std::process::id(), now()),
    );
}

/// 抢占单实例锁：同一时刻只允许一份守护连设备。
///
/// Windows 上必须用内核互斥体，不能「读 lock → 写 lock」：新旧两个进程在同一秒
/// 抢锁时会都读到空 lock、都写、都往下跑。
/// 两份守护共用一台设备时只能互相抢连接——设备端看到的是「连上又断开」，
/// 日志里是刷不完的「设备忙碌」。互斥体由内核保证唯一，进程退出自动释放，
/// 也就不存在陈旧锁把重启挡在门外的问题。
fn claim_instance() -> bool {
    #[cfg(windows)]
    {
        use windows::core::w;
        use windows::Win32::Foundation::{CloseHandle, GetLastError, BOOL, ERROR_ALREADY_EXISTS};
        use windows::Win32::System::Threading::CreateMutexW;

        // 句柄要活到进程结束：存静态变量，不 CloseHandle。
        static HELD: std::sync::OnceLock<isize> = std::sync::OnceLock::new();
        if HELD.get().is_some() {
            return true;
        }
        let Ok(handle) = (unsafe {
            CreateMutexW(None, BOOL(1), w!("Local\\zectrix-dsh-pet-daemon"))
        }) else {
            return false;
        };
        if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
            unsafe {
                let _ = CloseHandle(handle);
            }
            return false;
        }
        let _ = HELD.set(handle.0 as isize);
        write_lock_file();
        true
    }
    #[cfg(not(windows))]
    {
        if let Ok(text) = std::fs::read_to_string(lock_path()) {
            if let Some((pid, _)) = parse_heartbeat(&text) {
                if pid != std::process::id() && pid_is_ours(pid) {
                    return false;
                }
            }
        }
        write_lock_file();
        true
    }
}

/// 只清理旧版本（控制面板/托盘/开机自启）留下的启动产物，不写任何新启动项。
/// 插件形态下 DSH 自己 spawn 宠物，登录自启与快捷方式都不再需要；升级后第一次
/// 启动清一次即可。文件不存在就跳过，删不掉只告警——清理失败不该拖住启动。
/// 返回实际删掉的个数。
fn retire_legacy_autostart() -> usize {
    let appdata = std::env::var_os("APPDATA").map(PathBuf::from);
    let profile = std::env::var_os("USERPROFILE").map(PathBuf::from);
    retire_legacy_autostart_in(appdata.as_deref(), profile.as_deref())
}

/// 清理逻辑本体：基目录由调用方给（`None` = 环境变量缺失），方便测试直接喂
/// 临时目录，不用改进程级环境变量。
fn retire_legacy_autostart_in(appdata: Option<&Path>, profile: Option<&Path>) -> usize {
    let mut targets: Vec<PathBuf> = Vec::new();
    if let Some(base) = appdata {
        let programs = base
            .join("Microsoft")
            .join("Windows")
            .join("Start Menu")
            .join("Programs");
        targets.push(programs.join("Startup").join("zectrix-dsh-pet.vbs"));
        targets.push(programs.join("Startup").join("croc-pet.vbs"));
        targets.push(programs.join("DSH 宠物.lnk"));
    }
    if let Some(profile) = profile {
        let desktop = profile.join("Desktop");
        targets.push(desktop.join("DSH 宠物.lnk"));
        targets.push(desktop.join("鳄鱼大宠物.lnk"));
    }
    let mut removed = 0usize;
    for path in targets {
        // 旧版本没装过就没有这些文件：静默跳过，别每次启动都刷日志。
        if !path.exists() {
            continue;
        }
        match std::fs::remove_file(&path) {
            Ok(()) => removed += 1,
            // 被杀毒/权限挡住时只警告：宠物该起来还得起来。
            Err(e) => eprintln!("清理旧版启动项失败（忽略）：{}：{e}", path.display()),
        }
    }
    removed
}

/// 指定的进程是否还活着（宠物用来看拉起它的 DSH 是否被强杀）。
#[cfg(windows)]
fn pid_alive(pid: u32) -> bool {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    const STILL_ACTIVE: u32 = 259;
    unsafe {
        match OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) {
            Ok(handle) => {
                let mut code = 0u32;
                let ok = GetExitCodeProcess(handle, &mut code).is_ok();
                let _ = CloseHandle(handle);
                ok && code == STILL_ACTIVE
            }
            Err(_) => false,
        }
    }
}

/// 本机持久化：只存运行时状态缓存，不含任何会话正文。
///
/// 用户设置（跟随/提示音/音量/刷新/标题来源/设备地址/屏幕格式）
/// 的唯一真源是 `pet-config.json`；这里的 `model.settings` / `target` / `format`
/// 只是**快照**（面板与预览读它，不必去解析配置），写入时不再参与「谁更权威」
/// 的合并——每拍 `apply_user_config` 会从配置重新生成。
#[derive(Clone, Serialize, Deserialize)]
struct Store {
    #[serde(default)]
    model: Model,
    #[serde(default)]
    target: Option<String>,
    #[serde(default = "default_format")]
    format: String,
    /// 屏幕格式最后一次被显式修改的时刻（毫秒）。父子进程各持一份快照，
    /// 合并时按它取新——否则守护回写会把面板刚选的 mono 又盖回 gray16。
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    format_set_at: u64,
    /// 已经提醒过的指纹（见 `sound::notice_sig`）。持久化是为了让「补提醒」
    /// 在设备超时重连、守护重启后依然不重复响——同一次等待只吵人一次。
    /// 存一组（而不是一个槽位）：多个会话同时等待时，后一个指纹不会把前一个
    /// 挤掉，重连后不会因为「指纹被覆盖」再响一遍。旧版本存的是字符串，
    /// `de_announced` 负责兼容读取。
    #[serde(
        default,
        deserialize_with = "de_announced",
        skip_serializing_if = "Vec::is_empty"
    )]
    announced: Vec<String>,
    /// `announced` 最后一次写入的时刻。父子两个进程各持一份 state.json 快照，
    /// 合并时按时间取新——否则父进程回写会把子进程刚响过的指纹盖回旧值，
    /// 下一轮重连又响一次（实测一次等待响 4 遍）。
    #[serde(default, skip_serializing_if = "is_zero")]
    announced_at: i64,
    /// 上次交还设备的时刻（秒）。释放后安静一会儿再允许重新占用，
    /// 避免「释放→新记录→立刻重占→再释放」的抖动（实测 20 分钟 13 轮）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    released_at: Option<i64>,
    /// 守护模式下，最后一个任务结束到交还设备之间保留多久（秒）。规则是
    /// 「任务完成后 5 分钟内没有新任务就解除占用」。`None` = 还没人设过，
    /// 跟随 `default_idle_grace()`——这样「显式设成 300」和「默认 300」能区分开，
    /// 否则写入合并时会把它当成默认值又让磁盘旧值盖回去。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    idle_grace: Option<u32>,
    /// 本次加载是否读到了损坏的 state.json。只存在内存里（不落盘）：
    /// 真时 `save_store` 拒绝覆盖磁盘上的 state.json，只写 `state.recovered.json`，
    /// 直到下一次成功解析干净文件才清除——否则一份坏文件会让默认值把
    /// 42KB 的任务/提醒/设置整份冲掉。
    #[serde(skip)]
    degraded: bool,
}

/// 子智能体（agent team / subagent）是否还在跑。
///
/// 现场问题：`session-bc15bf0c…` 的父会话回合已经 `turn/end completed`，但它的
/// 两个子智能体（`origin: subagent`、header 里有 `parentSession`）还有未结束的
/// 回合——DSH 侧边栏显示「1 个子智能体运行中」，设备却显示「任务完成」。
/// 这里扫同项目目录下的子会话日志，把「有未结束回合」的父会话标出来。

/// 全量扫描会话目录的间隔：目录遍历 + 每个子会话 stat。原来是每 2 秒一次，
/// 但子会话只会在 agent team 启动/结束时增删，10 秒足够；两次全扫之间只对
/// 已知文件做 `metadata()` 快查（长度/mtime 变了才解压）。
const TEAM_FULL_SCAN_INTERVAL: Duration = Duration::from_secs(10);
/// 子会话日志超过这个年龄就不再跟踪（解压很贵，历史记录也没有意义）。
const TEAM_STALE_SECS: i64 = 12 * 3600;
/// busy 溯源的最大层数（agent team 可能多层嵌套）。
const TEAM_MAX_DEPTH: usize = 8;

#[derive(Default)]
struct TeamScan {
    /// 子日志路径 → (文件长度, mtime 秒, parentSession, 是否还有未结束回合)
    files: std::collections::HashMap<std::path::PathBuf, (u64, i64, Option<String>, bool)>,
    last_full_scan: Option<std::time::Instant>,
    busy: std::collections::HashMap<String, i64>,
}

impl TeamScan {
    /// 该不该走一遍全目录扫描。默认是「第一次一定扫，之后每 10 秒一次」。
    fn should_full_scan(&self, at: std::time::Instant) -> bool {
        match self.last_full_scan {
            Some(last) => at.saturating_duration_since(last) >= TEAM_FULL_SCAN_INTERVAL,
            None => true,
        }
    }

    /// 丢掉过期（>12h）或已经不存在的子会话日志，避免 `files` 无限增长。
    /// 用 `metadata` 而不是 `is_file`：一次 stat 就能同时确认存在并刷新
    /// 长度/mtime，文件被截断时下一轮 `touch` 会重新解码。
    fn prune(
        files: &mut std::collections::HashMap<
            std::path::PathBuf,
            (u64, i64, Option<String>, bool),
        >,
        at: i64,
    ) {
        files.retain(|path, entry| {
            if at.saturating_sub(entry.1) > TEAM_STALE_SECS {
                return false;
            }
            std::fs::metadata(path).is_ok()
        });
    }

    /// 记录一个子会话日志的当前状态；长度/mtime 变了才解压。读失败保持旧值，
    /// 下一轮再试（文件可能正被写进程 rename，读到半份不值得缓存）。
    fn touch(&mut self, path: &Path, len: u64, mtime: i64) {
        let entry = self
            .files
            .entry(path.to_path_buf())
            .or_insert((0, 0, None, false));
        if (entry.0, entry.1) == (len, mtime) {
            return;
        }
        let Ok(bytes) = std::fs::read(path) else {
            return;
        };
        let (parent, open) = decode_team_log(&bytes);
        *entry = (len, mtime, parent, open);
    }

    /// 全目录扫描：找出所有子智能体会话日志，再 prune 掉消失/过期的。
    fn full_scan(&mut self, root: &Path, at: i64) {
        if let Ok(projects) = std::fs::read_dir(root) {
            for project in projects.flatten() {
                if !project.path().is_dir() {
                    continue;
                }
                let Ok(entries) = std::fs::read_dir(project.path()) else {
                    continue;
                };
                for child in entries.flatten() {
                    let name = child.file_name().to_string_lossy().to_string();
                    // `session-…` 是主会话，其余（裸 uuid）才是子智能体会话。
                    if name.starts_with("session-") {
                        continue;
                    }
                    let dir = child.path();
                    if !dir.is_dir() {
                        continue;
                    }
                    let mut log = dir.join("session.v4.jsonl.zstd");
                    if !log.is_file() {
                        log = dir.join("session.v3.jsonl.zstd");
                    }
                    if !log.is_file() {
                        continue;
                    }
                    let Ok(meta) = std::fs::metadata(&log) else {
                        continue;
                    };
                    let mtime = mtime_secs(&meta);
                    // 很旧的子会话不值得解码。
                    if at.saturating_sub(mtime) > TEAM_STALE_SECS {
                        continue;
                    }
                    self.touch(&log, meta.len(), mtime);
                }
            }
        }
        Self::prune(&mut self.files, at);
    }

    /// 快查：只 stat 已知文件，变了才重新解码；不遍历目录。
    fn quick_scan(&mut self, at: i64) {
        let known: Vec<PathBuf> = self.files.keys().cloned().collect();
        for path in known {
            match std::fs::metadata(&path) {
                Ok(meta) => {
                    let mtime = mtime_secs(&meta);
                    if at.saturating_sub(mtime) > TEAM_STALE_SECS {
                        self.files.remove(&path);
                        continue;
                    }
                    self.touch(&path, meta.len(), mtime);
                }
                // 目录被删/改名：不再跟踪。
                Err(_) => {
                    self.files.remove(&path);
                }
            }
        }
    }

    /// 由 `files` 重建 busy 集合，返回是否变化（调用方据此落盘给面板看）。
    fn refresh_busy(&mut self, model: &mut dsh_pet::model::Model, at: i64) -> bool {
        // 父会话 → 父会话的父（用于把多层子会话一路溯到最上层的任务）
        let mut parents: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        let mut raw_busy: Vec<String> = Vec::new();
        for (log, entry) in &self.files {
            let Some(parent) = &entry.2 else {
                continue;
            };
            let session = log
                .parent()
                .and_then(|p| p.file_name())
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            parents.insert(session, parent.clone());
            if entry.3 {
                raw_busy.push(parent.clone());
            }
        }
        // 已知任务（主会话 id）集合：把子会话的 parentSession 一路往上溯，
        // 直到落在一个已知主会话上；多层 agent team 也能显示到最上层。
        let known: std::collections::HashSet<String> = model
            .tasks
            .iter()
            .filter_map(|t| t.event.thread_id.clone())
            .chain(model.tasks.iter().map(|t| t.event.task_id.clone()))
            .collect();
        let mut busy = std::collections::HashMap::new();
        for parent in raw_busy {
            let mut cur = parent;
            for _ in 0..TEAM_MAX_DEPTH {
                if known.contains(&cur) {
                    busy.insert(cur, at);
                    break;
                }
                match parents.get(&cur) {
                    Some(next) if next != &cur => cur = next.clone(),
                    _ => break,
                }
            }
        }
        let changed = self.busy != busy;
        self.busy = busy;
        model.team_busy = self.busy.clone();
        changed
    }

    /// 每拍调用：10 秒一次全目录扫描，其余拍只做已知文件的 metadata 快查；
    /// 结果写进 `model.team_busy`。返回 busy 集合是否变化。
    fn sync(&mut self, model: &mut dsh_pet::model::Model, root: &std::path::Path) -> bool {
        let now_instant = std::time::Instant::now();
        let at = now();
        if self.should_full_scan(now_instant) {
            self.last_full_scan = Some(now_instant);
            self.full_scan(root, at);
        } else {
            self.quick_scan(at);
        }
        self.refresh_busy(model, at)
    }
}

/// 解出「子会话的 parentSession + 是否还有未结束回合」。只读 session/turn 记录。
fn decode_team_log(bytes: &[u8]) -> (Option<String>, bool) {
    let text = dsh_pet::dsh::decode_all_frames(bytes);
    let text = String::from_utf8_lossy(&text);
    let mut parent = None;
    let mut open_turns = 0i64;
    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        match v["type"].as_str() {
            Some("session") => parent = v["parentSession"].as_str().map(str::to_string),
            Some("turn/start") => open_turns += 1,
            Some("turn/end") => open_turns -= 1,
            _ => {}
        }
    }
    (parent, open_turns > 0)
}

/// 文件的 mtime（秒）。拿不到时间戳时返回 0：会被当成很旧而跳过。
fn mtime_secs(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|m| m.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 由 DSH 插件读写、宠物进程热读的唯一用户配置源：
/// %LOCALAPPDATA%\zectrix-dsh-pet\pet-config.json。
///
/// 插件设置页、`dsh-pet set`、设备手势改的都是这一个文件；
/// 守护每拍比较 mtime，改完 ≤2 秒生效，不用重启。`state.json` 里那份
/// `model.settings` 只是快照，读设置时不再参考它（旧版本的值会在首次启动时
/// 一次性迁移过来）。
fn pet_config_path() -> PathBuf {
    data_dir().join("pet-config.json")
}

#[derive(Debug, Default, Clone)]
struct PetConfig {
    follow: Option<bool>,
    sound: Option<bool>,
    volume: Option<u8>,
    target: Option<String>,
    format: Option<String>,
    /// realtime / balance / calm / minimal
    mode: Option<String>,
    refresh_secs: Option<u32>,
    stale_minutes: Option<u32>,
    /// 标题来源：session（默认，只读 session/title）/ prompt（显式 opt-in）。
    title_source: Option<TitleSource>,
}

/// 一次写入 `pet-config.json` 的字段集合：只有这里声明的键能被写进文件，
/// 这份结构体本身就是**写盘白名单**（值类型连同范围校验一起约束住了）。
#[derive(Debug, Default, Clone)]
struct PetConfigPatch {
    follow: Option<bool>,
    sound: Option<bool>,
    volume: Option<u8>,
    target: Option<String>,
    format: Option<String>,
    mode: Option<String>,
    refresh_secs: Option<u32>,
    stale_minutes: Option<u32>,
    title_source: Option<TitleSource>,
    /// 显式删掉 `mode`。CLI 单独改 refresh 时必须清掉它，
    /// 否则面板写下的模式会在下一拍把这项又盖回去。
    clear_mode: bool,
}

impl PetConfigPatch {
    /// 只产出这次真正要改的键；没设的键保持文件里的原值。
    fn to_map(&self) -> serde_json::Map<String, serde_json::Value> {
        use serde_json::Value;
        let mut map = serde_json::Map::new();
        if let Some(v) = self.follow {
            map.insert("follow".into(), Value::Bool(v));
        }
        if let Some(v) = self.sound {
            map.insert("sound".into(), Value::Bool(v));
        }
        if let Some(v) = self.volume {
            map.insert("volume".into(), Value::from(v));
        }
        if let Some(v) = self.target.as_deref() {
            map.insert("target".into(), Value::String(v.to_string()));
        }
        if let Some(v) = self.format.as_deref() {
            map.insert("format".into(), Value::String(v.to_string()));
        }
        if let Some(v) = self.mode.as_deref() {
            map.insert("mode".into(), Value::String(v.to_string()));
        }
        if let Some(v) = self.refresh_secs {
            map.insert("refresh_secs".into(), Value::from(v));
        }
        if let Some(v) = self.stale_minutes {
            map.insert("stale_minutes".into(), Value::from(v));
        }
        if let Some(v) = self.title_source {
            map.insert("title_source".into(), Value::String(v.as_str().into()));
        }
        map
    }

    fn is_empty(&self) -> bool {
        self.to_map().is_empty() && !self.clear_mode
    }
}

/// 插件设置页可选的四种模式（含中文别名）。
const PET_CONFIG_MODES: [&str; 8] = [
    "realtime", "实时", "balance", "平衡", "calm", "安静", "minimal", "极简",
];

/// 插件每 2 秒热读一次配置：坏字段会一直在，不能每拍刷一行日志。
/// 只有文件内容变了才重新提示。键 = (路径, 内容哈希)。
static PET_CONFIG_WARN: std::sync::Mutex<Option<(PathBuf, u64)>> = std::sync::Mutex::new(None);

fn pet_config_warn_changed(path: &Path, bytes: &[u8]) -> bool {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut hasher);
    let hash = hasher.finish();
    let mut last = match PET_CONFIG_WARN.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    let same = matches!(last.as_ref(), Some((p, h)) if p == path && *h == hash);
    if !same {
        *last = Some((path.to_path_buf(), hash));
    }
    !same
}

/// 字段级容错的取值器：类型/范围不对就记一条警告并丢弃该字段，
/// 其余字段照常生效——不再因为一个坏 volume 把整份配置丢掉。
fn pet_bool_field(
    obj: &serde_json::Map<String, serde_json::Value>,
    name: &str,
    warnings: &mut Vec<String>,
) -> Option<bool> {
    let value = obj.get(name)?;
    match value.as_bool() {
        Some(v) => Some(v),
        None => {
            warnings.push(format!("[配置] 忽略非法字段 {name}={value}"));
            None
        }
    }
}

fn pet_u32_field(
    obj: &serde_json::Map<String, serde_json::Value>,
    name: &str,
    min: u32,
    max: u32,
    warnings: &mut Vec<String>,
) -> Option<u32> {
    let value = obj.get(name)?;
    match value.as_u64() {
        Some(n) if n >= u64::from(min) && n <= u64::from(max) => Some(n as u32),
        _ => {
            warnings.push(format!("[配置] 忽略非法字段 {name}={value}"));
            None
        }
    }
}

fn pet_str_field(
    obj: &serde_json::Map<String, serde_json::Value>,
    name: &str,
    warnings: &mut Vec<String>,
    valid: impl Fn(&str) -> bool,
) -> Option<String> {
    let value = obj.get(name)?;
    let Some(s) = value.as_str() else {
        warnings.push(format!("[配置] 忽略非法字段 {name}={value}"));
        return None;
    };
    let s = s.trim();
    if !valid(s) {
        warnings.push(format!("[配置] 忽略非法字段 {name}={value}"));
        return None;
    }
    Some(s.to_string())
}

/// 整体 JSON 语法错/顶层不是对象：另存 `.corrupt-<ts>.json` 并忽略整份配置。
fn quarantine_pet_config(path: &Path, why: &str) {
    let corrupt = corrupt_path_for(path);
    let name = corrupt
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "pet-config.corrupt.json".into());
    match std::fs::rename(path, &corrupt) {
        Ok(()) => eprintln!("pet-config.json 解析失败，已另存 {name}: {why}"),
        Err(err) => eprintln!(
            "pet-config.json 解析失败，另存 {name} 失败（忽略本次配置）: {why}; {err}"
        ),
    }
}

/// 逐字段校验：类型/范围不对就记一条警告并丢弃该字段，
/// 其余字段照常生效——不再因为一个坏 volume 把整份配置丢掉。
fn pet_config_from_obj(
    obj: &serde_json::Map<String, serde_json::Value>,
    warnings: &mut Vec<String>,
) -> PetConfig {
    let mut cfg = PetConfig {
        follow: pet_bool_field(obj, "follow", warnings),
        sound: pet_bool_field(obj, "sound", warnings),
        volume: pet_u32_field(obj, "volume", 0, 100, warnings).map(|v| v as u8),
        target: None,
        format: pet_str_field(obj, "format", warnings, |s| {
            !s.is_empty() && device::format(s).is_ok()
        })
        .and_then(|s| device::format(&s).ok().map(|f| device::format_name(f).to_string())),
        mode: pet_str_field(obj, "mode", warnings, |s| PET_CONFIG_MODES.contains(&s)),
        refresh_secs: pet_u32_field(obj, "refresh_secs", 0, 3600, warnings),
        stale_minutes: pet_u32_field(obj, "stale_minutes", 1, 1440, warnings),
        title_source: pet_str_field(obj, "title_source", warnings, |s| {
            TitleSource::parse(s).is_some()
        })
        .and_then(|s| TitleSource::parse(&s)),
    };
    // 空字符串 = 没设过，静默忽略；非字符串才算非法。
    if let Some(value) = obj.get("target") {
        match value.as_str() {
            Some(s) if !s.trim().is_empty() => cfg.target = Some(s.trim().to_string()),
            Some(_) => {}
            None => warnings.push(format!("[配置] 忽略非法字段 target={value}")),
        }
    }
    cfg
}

/// 读取插件配置：先整体解析成 `Value`，语法错就整份隔离；
/// 否则逐字段校验，坏字段丢弃并提示，其余照常生效。
fn load_pet_config_at(path: &Path) -> Option<PetConfig> {
    let bytes = std::fs::read(path).ok()?;
    let value: serde_json::Value = match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(e) => {
            quarantine_pet_config(path, &e.to_string());
            return None;
        }
    };
    let Some(obj) = value.as_object() else {
        quarantine_pet_config(path, "顶层不是 JSON 对象");
        return None;
    };
    let mut warnings = Vec::new();
    let cfg = pet_config_from_obj(obj, &mut warnings);
    if !warnings.is_empty() && pet_config_warn_changed(path, &bytes) {
        for warning in warnings {
            eprintln!("{warning}");
        }
    }
    Some(cfg)
}

/// pet-config.json 的 mtime 缓存：插件可能每 2 秒写一次、守护每拍读一次，
/// 文件没变就直接复用上次的解析结果（也不再重复打印坏字段警告）。
/// 键 = (路径, Option<(mtime 秒, mtime 纳秒, 文件长度)>, 解析结果)。
type PetConfigCacheEntry = (PathBuf, Option<(u64, u32, u64)>, Option<PetConfig>);
static PET_CONFIG_CACHE: std::sync::Mutex<Option<PetConfigCacheEntry>> =
    std::sync::Mutex::new(None);

fn pet_config_cache_lock() -> std::sync::MutexGuard<'static, Option<PetConfigCacheEntry>> {
    match PET_CONFIG_CACHE.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// 文件指纹：mtime（纳秒级）+ 长度。两者都没变就认为内容没变。
fn pet_config_stamp(path: &Path) -> Option<(u64, u32, u64)> {
    let meta = std::fs::metadata(path).ok()?;
    let modified = meta.modified().ok()?;
    let since = modified.duration_since(UNIX_EPOCH).ok()?;
    Some((since.as_secs(), since.subsec_nanos(), meta.len()))
}

fn invalidate_pet_config_cache() {
    *pet_config_cache_lock() = None;
}

/// 走 mtime 缓存读配置：文件没变就不再解析一遍。
fn load_pet_config_cached(path: &Path) -> Option<PetConfig> {
    let stamp = pet_config_stamp(path);
    {
        let cache = pet_config_cache_lock();
        if let Some((cached_path, cached_stamp, cached_cfg)) = cache.as_ref() {
            if cached_path == path && *cached_stamp == stamp {
                return cached_cfg.clone();
            }
        }
    }
    let cfg = load_pet_config_at(path);
    *pet_config_cache_lock() = Some((path.to_path_buf(), stamp, cfg.clone()));
    cfg
}

/// 读现有配置的原始 JSON 对象（坏文件按空对象处理）：写盘时用它保留
/// 本次没改的键——手写的注释、未来版本新增的字段都不会被这次保存吃掉。
fn read_pet_config_map(path: &Path) -> serde_json::Map<String, serde_json::Value> {
    std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default()
}

/// 原子写 pet-config.json（tmp → fsync → rename）。
///
/// 只允许写 `PetConfigPatch` 里声明的键（写盘白名单），并且写前用读端同一套
/// 校验逻辑验一遍：非法值直接报错、不落盘，绝不让守护读到一份半对的文件。
fn write_pet_config_at(path: &Path, patch: &PetConfigPatch) -> Result<()> {
    let patch_map = patch.to_map();
    let mut warnings = Vec::new();
    let _ = pet_config_from_obj(&patch_map, &mut warnings);
    if !warnings.is_empty() {
        anyhow::bail!("拒绝写入 pet-config.json：{}", warnings.join("；"));
    }
    let mut map = read_pet_config_map(path);
    for (key, value) in patch_map {
        map.insert(key, value);
    }
    if patch.clear_mode {
        map.remove("mode");
    }
    let bytes = serde_json::to_vec_pretty(&serde_json::Value::Object(map))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    write_json_atomic(path, &bytes)?;
    invalidate_pet_config_cache();
    Ok(())
}

/// 面板上的模式 → 细节合并秒数。模式只决定刷新节流，没有别的维度：
/// realtime=0、balance=10、calm=30、minimal=120。
fn mode_preset(mode: &str) -> u32 {
    match mode {
        "realtime" | "实时" => 0,
        "balance" | "平衡" => 10,
        "minimal" | "极简" => 120,
        _ => 30,
    }
}

/// 从刷新节流反推模式显示名（日志与诊断用，和插件设置页一致）。
fn mode_label(refresh_secs: u32) -> &'static str {
    match refresh_secs {
        0 => "实时",
        120.. => "极简",
        30..=45 => "安静",
        _ => "平衡",
    }
}

/// 由配置生成完整的设置：缺字段一律回落到 Rust 默认值（`Settings::default`），
/// 不再拿 state.json 里那半份旧快照来补。
fn settings_from_pet_config(c: &PetConfig) -> Settings {
    let mut s = Settings::default();
    if let Some(v) = c.follow {
        s.follow_enabled = v;
    }
    if let Some(v) = c.sound {
        s.sound_enabled = v;
    }
    if let Some(v) = c.volume {
        s.volume = v;
    }
    if let Some(v) = c.stale_minutes {
        s.stale_minutes = v;
    }
    if let Some(v) = c.title_source {
        s.title_source = v;
    }
    if let Some(mode) = c.mode.as_deref() {
        s.refresh_secs = mode_preset(mode);
    } else if let Some(v) = c.refresh_secs {
        s.refresh_secs = v;
    }
    s
}

/// `Settings` 在 model.rs 里没有 `PartialEq`（不在本次改动范围），
/// 这里逐项比较，语义等价。
fn settings_equal(a: &Settings, b: &Settings) -> bool {
    a.follow_enabled == b.follow_enabled
        && a.stale_minutes == b.stale_minutes
        && a.sound_enabled == b.sound_enabled
        && a.volume == b.volume
        && a.refresh_secs == b.refresh_secs
        && a.title_source == b.title_source
}

/// 把一份解析好的配置套进 store：设置**整体替换**（缺字段走默认），
/// 设备地址与屏幕格式按需更新。返回是否生效了变化。
fn apply_pet_config_value(store: &mut Store, c: &PetConfig) -> bool {
    let next = settings_from_pet_config(c);
    let mut changed = false;
    if !settings_equal(&store.model.settings, &next) {
        store.model.settings = next;
        changed = true;
    }
    if let Some(t) = c.target.as_deref() {
        let t = t.trim();
        if !t.is_empty() && store.target.as_deref() != Some(t) {
            store.target = Some(t.to_string());
            changed = true;
        }
    }
    if let Some(f) = c.format.as_deref() {
        if f != store.format {
            store.format = f.to_string();
            store.format_set_at = now().max(1) as u64;
            changed = true;
        }
    }
    if changed {
        let s = &store.model.settings;
        println!(
            "[配置] pet-config.json 已生效：follow={} 模式={} 音量={} 目标={} 格式={} 标题来源={}",
            s.follow_enabled,
            mode_label(s.refresh_secs),
            s.volume,
            store.target.as_deref().unwrap_or("自动发现"),
            store.format,
            s.title_source.as_str(),
        );
    }
    changed
}

/// 一次性迁移：`pet-config.json` 不存在、但同目录已有 `state.json` 时，
/// 把旧版本写在 state.json 里的设置/地址/格式搬进配置。之后一律只认配置。
///
/// 全新安装不建这个文件：首次加载会顺手写出一个「全默认」的 state.json，
/// 那份东西没有值得保留的东西，凭空造出配置反而会让人以为迁移过。
fn migrate_state_settings_to_pet_config(path: &Path, store: &Store) -> bool {
    if path.exists() || !path.with_file_name("state.json").exists() {
        return false;
    }
    if store.target.is_none()
        && store.format == default_format()
        && settings_equal(&store.model.settings, &Settings::default())
    {
        return false;
    }
    let s = &store.model.settings;
    let patch = PetConfigPatch {
        follow: Some(s.follow_enabled),
        sound: Some(s.sound_enabled),
        volume: Some(s.volume),
        target: store.target.clone(),
        format: Some(store.format.clone()),
        refresh_secs: Some(s.refresh_secs),
        stale_minutes: Some(s.stale_minutes),
        title_source: Some(s.title_source),
        ..Default::default()
    };
    match write_pet_config_at(path, &patch) {
        Ok(()) => {
            println!(
                "[配置] 已把 state.json 里的旧设置迁移到 {}（一次性；之后只认这份配置）",
                path.display()
            );
            true
        }
        Err(e) => {
            eprintln!("[配置] 旧设置迁移到 pet-config.json 失败（本次沿用 state.json 快照）: {e}");
            false
        }
    }
}

/// 用户配置的唯一入口：把 pet-config.json 套进 store（设置 + 设备地址 +
/// 屏幕格式）。每拍 / 每 2 秒调一次；文件没变时只做比较、不重复解析
/// （mtime 缓存）。返回 true = 有字段生效（调用方据此更新模型、格式并强推一帧）。
fn apply_user_config(store: &mut Store) -> bool {
    apply_user_config_at(&pet_config_path(), store)
}

fn apply_user_config_at(path: &Path, store: &mut Store) -> bool {
    migrate_state_settings_to_pet_config(path, store);
    let Some(cfg) = load_pet_config_cached(path) else {
        return false;
    };
    apply_pet_config_value(store, &cfg)
}


/// 任务完成后多久没有新任务就交还设备：5 分钟。
fn default_idle_grace() -> u32 {
    300
}

fn default_format() -> String {
    // 默认 mono（黑白 1 位）：设备实测整屏刷新约 1.1 秒，而 gray16 要 5 秒——
    // 16 灰阶会走完整的「多段去残影」波形，看起来就是连闪好几次。mono 的
    // 文字有网点感（Floyd–Steinberg 抖动），但刷新快、闪得少，日常更舒服。
    // 想要更细腻可以 `set --format gray16`（或面板「高级 / 诊断」里切）。
    "mono".into()
}

fn is_zero(v: &i64) -> bool {
    *v == 0
}

fn is_zero_u64(v: &u64) -> bool {
    *v == 0
}

/// 兼容旧版 `state.json` 里的 `"announced": "…"`（单个字符串）与新版数组。
/// 直接换成 `Vec<String>` 会让旧文件整份解析失败——那会把任务缓存一起丢掉，
/// 相当于把所有历史重放一遍，代价太大。
fn de_announced<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(serde::Deserialize)]
    #[serde(untagged)]
    enum Raw {
        One(String),
        Many(Vec<String>),
    }
    Ok(match Option::<Raw>::deserialize(deserializer)? {
        None => Vec::new(),
        Some(Raw::One(s)) => vec![s],
        Some(Raw::Many(v)) => v,
    })
}

/// 提醒指纹最多留几条：覆盖多会话同时等待，又不会把 state.json 撑大。
const ANNOUNCED_KEEP: usize = 12;

/// 交还设备后，设备要大约 8 秒才释放独占租约：这段时间内重连只会拿到
/// 「设备忙碌」，还会把日志刷满。
const RELEASE_LEASE: i64 = 8;

/// 会话文件多久没动算「已停笔」。释放设备前必须满足它，否则会在对方
/// 还在写日志的时候把连接撤掉，下一次扫描又把它拉起来。
const WRITING_WINDOW: i64 = 30;

/// 刚连上时的静音期：音频通路还没就绪就开播，喇叭容易「咔」一声。
const SOUND_SETTLE: i64 = 3;

impl Default for Store {
    fn default() -> Self {
        Self {
            model: Model::default(),
            target: None,
            format: default_format(),
            format_set_at: 0,
            announced: Vec::new(),
            announced_at: 0,
            released_at: None,
            idle_grace: None,
            degraded: false,
        }
    }
}

impl Store {
    /// 实际生效的宽限秒数：没设过就用默认 5 分钟。
    fn idle_grace(&self) -> i64 {
        self.idle_grace.unwrap_or_else(default_idle_grace) as i64
    }
}

/// 读 `state.json` 的 stamp：mtime（秒 + 纳秒）+ 长度。三者任一变化都算「换了
/// 内容」；文件不存在/读不了 metadata 时返回 `(None, 0)`。
fn state_stamp(path: &Path) -> (Option<(u64, u32)>, u64) {
    match std::fs::metadata(path) {
        Ok(meta) => (
            meta.modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| (d.as_secs(), d.subsec_nanos())),
            meta.len(),
        ),
        Err(_) => (None, 0),
    }
}

/// 上一次读过/写过的 `state.json` 快照。主循环每秒都要看「磁盘上别人刚写的
/// 运行字段」（announced / released_at / idle_grace / format_set_at），但每秒
/// 重读 + 解析整份 JSON 纯属浪费：mtime+长度没变就直接克隆这份内存快照。
struct DiskStateCache {
    path: PathBuf,
    mtime: Option<(u64, u32)>,
    len: u64,
    store: Store,
}

static DISK_STATE_CACHE: Mutex<Option<DiskStateCache>> = Mutex::new(None);

fn disk_state_cache_lock() -> std::sync::MutexGuard<'static, Option<DiskStateCache>> {
    match DISK_STATE_CACHE.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn load_store() -> Store {
    cached_load_store()
}

/// 命中缓存时零解析；返回的是快照克隆，调用方随便改。
fn cached_load_store() -> Store {
    cached_load_store_at(&state_path())
}

fn cached_load_store_at(path: &Path) -> Store {
    let before = state_stamp(path);
    {
        let cache = disk_state_cache_lock();
        if let Some(entry) = cache.as_ref() {
            if entry.path == path && entry.mtime == before.0 && entry.len == before.1 {
                return entry.store.clone();
            }
        }
    }
    let store = load_store_at(path);
    // 读的这段时间文件可能被别的进程改写：只有前后 stamp 一致才敢缓存，
    // 否则会缓存下「旧内容 + 新 stamp」，之后永远读不到再次更新。
    let after = state_stamp(path);
    if after == before {
        let mut cache = disk_state_cache_lock();
        *cache = Some(DiskStateCache {
            path: path.to_path_buf(),
            mtime: after.0,
            len: after.1,
            store: store.clone(),
        });
    }
    store
}

/// 保存成功后把刚写下去的内容同步进读缓存：下一拍 `cached_load_store`
/// 不会因为「mtime 变了但内存快照还是旧的」而白解析一次。写盘期间持有
/// `state.lock`，别的进程写不进来，所以这里存 `out` 是安全的。
fn refresh_disk_state_cache(path: &Path, store: &Store) {
    let stamp = state_stamp(path);
    let mut cache = disk_state_cache_lock();
    *cache = Some(DiskStateCache {
        path: path.to_path_buf(),
        mtime: stamp.0,
        len: stamp.1,
        store: store.clone(),
    });
}

/// 损坏文件改名用的路径：`state.corrupt-<unix>.json`。拷贝/改名都发生在同目录，
/// 不删除原文件，也不让默认值覆盖它。同一秒内重复损坏时追加序号，
/// 不把上一份损坏现场盖掉。
fn corrupt_path_for(path: &Path) -> PathBuf {
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "state".into());
    let ts = now();
    let mut candidate = path.with_file_name(format!("{stem}.corrupt-{ts}.json"));
    let mut n = 1;
    while candidate.exists() {
        n += 1;
        candidate = path.with_file_name(format!("{stem}.corrupt-{ts}-{n}.json"));
    }
    candidate
}

/// `state.json` / `pet-config.json` 的同目录兄弟文件（`state.bak.json` 等）。
fn sibling_json(path: &Path, role: &str) -> PathBuf {
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "state".into());
    path.with_file_name(format!("{stem}.{role}.json"))
}

/// 同目录 tmp → `sync_all` → rename 覆盖。Windows 下目标被杀软/索引/别的进程
/// 短暂占用时 rename 会失败，此时回退 copy + remove，保证内容仍然落盘。
fn write_json_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    // tmp 名必须唯一：多个进程/线程同时保存时共用一个 `state.json.tmp` 会让
    // 先完成的 rename 把文件挪走，另一个 rename 拿到 ENOENT（或写进半份内容）。
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp = path.with_extension(format!("tmp.{}.{stamp}", std::process::id()));
    {
        let mut f = std::fs::File::create(&tmp)
            .with_context(|| format!("创建临时文件 {} 失败", tmp.display()))?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(rename_err) => match std::fs::copy(&tmp, path) {
            Ok(_) => {
                let _ = std::fs::remove_file(&tmp);
                Ok(())
            }
            Err(copy_err) => {
                let _ = std::fs::remove_file(&tmp);
                Err(anyhow::anyhow!(
                    "写入 {} 失败（rename: {rename_err}；copy: {copy_err}）",
                    path.display()
                ))
            }
        },
    }
}

/// 从磁盘加载。解析失败时把原文件改名为 `state.corrupt-<ts>.json`
///（不删除、不复制覆盖），返回默认 Store 并置 `degraded`——
/// `save_store_at` 据此拒绝覆盖，只写 `state.recovered.json`。
fn load_store_at(path: &Path) -> Store {
    let mut store = match std::fs::read(path) {
        Ok(bytes) => match serde_json::from_slice::<Store>(&bytes) {
            Ok(store) => store,
            Err(e) => {
                let corrupt = corrupt_path_for(path);
                let name = corrupt
                    .file_name()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_else(|| "state.corrupt.json".into());
                match std::fs::rename(path, &corrupt) {
                    Ok(()) => {
                        eprintln!("state.json 解析失败，已另存 {name}: {e}");
                    }
                    Err(rename_err) => {
                        // 改名失败（文件被占用等）也必须降级：原文件留在原地，
                        // 后面任何 save 都不能拿默认值把它盖掉。
                        eprintln!(
                            "state.json 解析失败，另存 {name} 失败（原文件保持不动，进入降级保护）: {e}; {rename_err}"
                        );
                    }
                }
                let mut default = Store::default();
                default.degraded = true;
                default
            }
        },
        Err(_) => Store::default(),
    };
    // 缓存版本过旧时重建跟随任务；此前从未调用过迁移，旧的序号式轮次 ID
    // 会一直留在缓存里挡住新事件。
    let cache_migrated = store.model.migrate_cache();
    // 旧版 announced 存的是「任务 key|状态|60 字文案」，可能含提示词正文。
    // 只保留新格式（sha256 前 16 位），其余一律清掉；用新的 announced_at
    // 保证 save_store_at 的按时间合并不会把旧条目从磁盘捞回来。
    let before = store.announced.len();
    store.announced.retain(|sig| sound::is_notice_hash(sig));
    let announced_migrated = store.announced.len() != before;
    if announced_migrated {
        store.announced_at = now().max(1);
    }
    if cache_migrated || announced_migrated {
        // 降级时这只写 state.recovered.json，不会覆盖损坏的 state.json。
        let _ = save_store_at(path, &store);
    }
    store
}

/// 保存到指定路径，供测试注入临时目录。
///
/// - 正常：上一版先滚成 `state.bak.json`（单份，失败不阻塞），再 tmp+fsync+rename。
/// - 降级（内存里 `degraded`，或磁盘上这份解析不了）：绝不覆盖已有 state.json，
///   只写 `state.recovered.json` 留给用户手工恢复。
fn save_store_at(path: &Path, store: &Store) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // 跨进程串行化：守护、`set`、插件（经 CLI）可能同时写同一份 state.json。
    // 读-合并-写整段都待在锁里，否则两边各自读到旧值再各自回写，字段就丢了。
    // 拿不到锁（2 秒超时）时降级为不加锁写入：宁可偶尔丢字段，也不能卡住守护。
    let _lock = acquire_file_lock(
        &path.with_extension("lock"),
        std::time::Duration::from_secs(2),
    );
    // 守护与交互命令共用一份 state.json。守护内存里的 store 是启动时的快照，
    // 若整份回写，会把别处（init / set / run）刚保存的设备地址覆盖成 None。
    // 写入前合并一次：设备地址与格式以「磁盘上的非空值」为准，除非本次显式改了。
    let disk_bytes = std::fs::read(path).ok();
    let disk = disk_bytes
        .as_deref()
        .and_then(|b| serde_json::from_slice::<Store>(b).ok());
    // 磁盘上这份读得出来但解析不了 = 已经损坏；即使调用方没带 degraded 标记，
    // 也不能覆盖它（例如另一个进程先写出了半份 JSON）。
    let degraded = store.degraded || (disk_bytes.is_some() && disk.is_none());

    let mut out = store.clone();
    if let Some(disk) = disk {
        if out.target.is_none() {
            out.target = disk.target;
        }
        if disk.format_set_at > out.format_set_at {
            out.format = disk.format;
            out.format_set_at = disk.format_set_at;
        }
        // 提醒指纹是「谁新谁说了算」：父子进程各持一份快照，只有按时间合并
        // 才不会让慢半拍的那份把刚响过的指纹盖回去（盖回去 = 下轮重连再响）。
        if disk.announced_at > out.announced_at {
            out.announced = disk.announced;
            out.announced_at = disk.announced_at;
        }
        if out.released_at.is_none() {
            out.released_at = disk.released_at;
        }
        if out.idle_grace.is_none() {
            out.idle_grace = disk.idle_grace;
        }
        // 注意：`model.settings` **不再**参与合并——它们是 pet-config.json
        // 的只读快照，真源在那边；这里合并只会把别处刚改的设置盖回旧值。
    }
    // 内存标记不落盘：recovered / 正常文件里都必须是 false。
    out.degraded = false;
    let bytes = serde_json::to_vec_pretty(&out)?;

    if degraded {
        let recovered = sibling_json(path, "recovered");
        write_json_atomic(&recovered, &bytes)?;
        // 守护每拍都可能保存：只提示一次，别把日志刷满。
        static WARNED: std::sync::atomic::AtomicBool =
            std::sync::atomic::AtomicBool::new(false);
        if !WARNED.swap(true, Ordering::Relaxed) {
            eprintln!(
                "[state] 降级保护中：state.json 损坏未恢复，本次及之后只写 {}（不覆盖原文件）",
                recovered.display()
            );
        }
        return Ok(());
    }

    // 已经写回真实文件了：下次再损坏时要重新提示一次。
    static WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    WARNED.store(false, Ordering::Relaxed);

    // 每次成功保存前把现有 state.json 留成上一版（单份滚动）。备份失败只影响
    // 后悔药，不能挡住主保存。
    if path.exists() {
        let bak = sibling_json(path, "bak");
        match std::fs::copy(path, &bak) {
            Ok(_) => {
                if let Ok(f) = std::fs::File::open(&bak) {
                    let _ = f.sync_all();
                }
            }
            Err(e) => eprintln!("[state] 备份上一版 {} 失败（忽略）: {e}", bak.display()),
        }
    }

    write_json_atomic(path, &bytes)?;
    // 刚写下去的内容就是磁盘现状：同步进读缓存，下一拍不必重新解析。
    refresh_disk_state_cache(path, &out);
    Ok(())
}

/// 非强制保存的最小间隔：守护每拍都会保存模型（换任务、team 状态、花絮），
/// 但一秒内把整份 state.json 重写多次没有意义，只是多出 fsync + bak 拷贝。
const SAVE_MIN_INTERVAL: Duration = Duration::from_secs(1);

/// 纯函数：距上次落盘是否已经过了节流窗口（测试直接对着它断言）。
fn save_window_open(last_flush: Instant, at: Instant) -> bool {
    at.saturating_duration_since(last_flush) >= SAVE_MIN_INTERVAL
}

struct SaveThrottle {
    last_flush: Instant,
    dirty: bool,
    /// 窗口内被压下的最新一份快照。后到的那份覆盖前一份：store 是累积的，
    /// 丢中间几份没事，丢了最后一份才会丢字段。
    pending: Option<Store>,
}

impl SaveThrottle {
    fn new() -> Self {
        // 初始 last_flush 回拨一个窗口：进程内第一次保存立刻落盘。
        Self {
            last_flush: Instant::now()
                .checked_sub(SAVE_MIN_INTERVAL)
                .unwrap_or_else(Instant::now),
            dirty: false,
            pending: None,
        }
    }
}

static SAVE_THROTTLE: OnceLock<Mutex<SaveThrottle>> = OnceLock::new();

fn save_throttle_lock() -> std::sync::MutexGuard<'static, SaveThrottle> {
    let cell = SAVE_THROTTLE.get_or_init(|| Mutex::new(SaveThrottle::new()));
    match cell.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// 保存（节流）：1 秒内至多真正写一次盘。窗口内的请求压进 `pending`，由
/// 下一拍的 `flush_pending_store` 或退出时的强制保存落盘。写入本身仍是
/// tmp + `sync_all` + rename，不会为了省 IO 牺牲掉掉电安全。
fn save_store(store: &Store) -> Result<()> {
    let at = Instant::now();
    let mut throttle = save_throttle_lock();
    if !save_window_open(throttle.last_flush, at) {
        throttle.pending = Some(store.clone());
        throttle.dirty = true;
        return Ok(());
    }
    throttle.pending = None;
    throttle.dirty = false;
    throttle.last_flush = at;
    let result = save_store_at(&state_path(), store);
    drop(throttle);
    result
}

/// 强制保存：立刻落盘（用户动作、显式持久化、断线、退出）。攒着的 pending
/// 比这份旧，直接丢掉。
fn save_store_now(store: &Store) -> Result<()> {
    let mut throttle = save_throttle_lock();
    throttle.pending = None;
    throttle.dirty = false;
    throttle.last_flush = Instant::now();
    let result = save_store_at(&state_path(), store);
    drop(throttle);
    result
}

/// 窗口一到就把攒着的最后一份保存写下去：保证「1 秒内最多写一次」的同时
/// 「最多晚 1 秒落盘」。主循环每拍调一次，退出路径也会调。
fn flush_pending_store() -> Result<()> {
    let at = Instant::now();
    let mut throttle = save_throttle_lock();
    if !throttle.dirty || !save_window_open(throttle.last_flush, at) {
        return Ok(());
    }
    let Some(store) = throttle.pending.take() else {
        throttle.dirty = false;
        return Ok(());
    };
    throttle.dirty = false;
    throttle.last_flush = at;
    let result = save_store_at(&state_path(), &store);
    drop(throttle);
    result
}

/// 跨进程写锁的句柄：Drop 时删掉 `state.lock`（只删自己建的那份）。
struct FileLock {
    path: PathBuf,
}

impl Drop for FileLock {
    fn drop(&mut self) {
        let mine = std::process::id().to_string();
        if let Ok(text) = std::fs::read_to_string(&self.path) {
            if text.trim() == mine {
                let _ = std::fs::remove_file(&self.path);
            }
        }
    }
}

/// 独占创建 `state.lock`（`create_new` = 原子抢占），拿到就写进本进程 pid。
///
/// - 锁已存在：可能是别的进程正在写，也可能上一个进程崩了没删。`mtime`
///   超过 10 秒视为陈旧锁，删掉重试。
/// - 超过 `timeout`（默认 2 秒）还拿不到：打印一次警告、返回 None，
///   调用方照常写盘（降级：并发时可能丢字段，但不至于卡死守护）。
fn acquire_file_lock(path: &Path, timeout: std::time::Duration) -> Option<FileLock> {
    let started = std::time::Instant::now();
    let stale_after = std::time::Duration::from_secs(10);
    loop {
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
        {
            Ok(mut file) => {
                let _ = writeln!(file, "{}", std::process::id());
                return Some(FileLock {
                    path: path.to_path_buf(),
                });
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                let stale = std::fs::metadata(path)
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.elapsed().ok())
                    .map(|age| age > stale_after)
                    .unwrap_or(false);
                if stale {
                    let _ = std::fs::remove_file(path);
                    continue;
                }
                if started.elapsed() >= timeout {
                    eprintln!(
                        "[state] 等待 {} 超时（{} 秒）：降级为不加锁写入，并发时可能丢字段",
                        path.display(),
                        timeout.as_secs()
                    );
                    return None;
                }
                std::thread::sleep(std::time::Duration::from_millis(15));
            }
            // 目录不可写等硬错误：锁不是必需品，直接降级。
            Err(_) => return None,
        }
    }
}

struct Args {
    cmd: String,
    flags: Vec<(String, String)>,
}

impl Args {
    fn parse(argv: Vec<String>) -> Self {
        let mut cmd = String::from("help");
        let mut flags = vec![];
        let mut it = argv.into_iter().peekable();
        while let Some(a) = it.next() {
            if let Some(name) = a.strip_prefix("--") {
                let (name, value) = match name.split_once('=') {
                    Some((n, v)) => (n.to_string(), v.to_string()),
                    None => {
                        let next = it.peek().cloned();
                        let value = match next {
                            Some(n) if !n.starts_with("--") => {
                                it.next();
                                n
                            }
                            _ => "true".into(),
                        };
                        (name.to_string(), value)
                    }
                };
                flags.push((name, value));
            } else if cmd == "help" {
                cmd = a;
            } else {
                flags.push((format!("arg{}", flags.len()), a));
            }
        }
        Self { cmd, flags }
    }

    fn get(&self, name: &str) -> Option<&str> {
        self.flags
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    fn has(&self, name: &str) -> bool {
        self.flags.iter().any(|(k, _)| k == name)
    }
}

fn parse_status(value: &str) -> Result<Option<Status>> {    Ok(match value.trim().to_ascii_lowercase().as_str() {
        "idle" | "none" | "off" => None,
        "working" | "work" => Some(Status::Working),
        "waiting" | "wait" => Some(Status::Waiting),
        "done" | "finish" => Some(Status::Done),
        "error" => Some(Status::Error),
        "stopped" | "stop" => Some(Status::Stopped),
        "unknown" => Some(Status::Unknown),
        other => anyhow::bail!(
            "未知状态 {other}，可用 working / waiting / done / error / stopped / unknown / idle"
        ),
    })
}

fn scan_into(model: &mut Model, watcher: &mut Watcher) -> (Vec<TaskEvent>, bool) {
    // 设置每拍都可能被 `set` / 插件配置改掉：扫描前同步，prompt 模式才读正文。
    watcher.use_prompt_titles(model.settings.title_source == TitleSource::Prompt);
    let mut applied = vec![];
    match watcher.scan() {
        Ok(events) => {
            for (ev, at) in events {
                let keep = ev.clone();
                if model.apply(ev, "dsh", at).unwrap_or(false) {
                    applied.push(keep);
                }
            }
        }
        Err(e) => eprintln!("读取 DeepSeek Harness 会话失败: {e}"),
    }
    let mut changed = !applied.is_empty();
    // 用户在侧边栏归档/删掉的会话要退场。它的日志还在磁盘上，被归档时若停在
    // 未答复的提问上就会一直算「等你处理」——等待优先于已完成，画面会长期停在
    // 一条已经不存在的任务上。每次扫描全量对一遍，state.json 里持久化的旧任务
    // 也能在重启后被清掉。只查模型里出现过的 id，别拿整张归档表去套。
    if !watcher.deleted().is_empty() {
        let deleted = watcher.deleted();
        let mut victims: Vec<String> = model
            .tasks
            .iter()
            .filter(|t| t.source == "dsh")
            .filter_map(|t| t.event.thread_id.clone())
            .filter(|id| deleted.contains(id))
            .collect();
        victims.sort();
        victims.dedup();
        for stem in victims {
            if model.forget_session("dsh", &stem) {
                changed = true;
                println!("[跟随] 会话已归档，退场：{stem}");
            }
        }
    }
    // 状态没变但会话还在写：把「最后活动时间」推上去。
    // `event()` 只在展示内容变化时才产出事件，同一轮里状态长期不变时
    // updated_at 会停在状态切换那一刻——5 分钟释放规则据此误判成「已经没人了」，
    // 于是任务还在跑、设备却被交还，紧接着新记录又把它叫回来，来回抖。
    let tick = now();
    for (task_id, at) in watcher.activity() {
        if let Some(t) = model
            .tasks
            .iter_mut()
            .find(|t| t.source == "dsh" && t.event.task_id == task_id)
        {
            let at = at.min(tick);
            if at > t.updated_at {
                t.updated_at = at;
                changed = true;
            }
        }
    }
    // 待命页的鳄鱼用这个判断「刚忙完」还是「闲很久了」（30 分钟后睡觉）。
    if !model.tasks.is_empty() {
        model.last_active_at = Some(tick);
    }
    // 主卡跟着「已经显示的那条」走：两个会话并行时不来回抢屏。
    model.auto_focus(tick);
    // 左下角的上下文占用跟着「当前展示的会话」走：切任务、对话变长都能反映出来。
    // 只在整数百分比变化时触发推送——每 1% 刷一次墨水屏已足够，再多就是闪。
    let now = now();
    let ctx = model
        .current(now)
        .and_then(|t| t.event.thread_id)
        .and_then(|id| watcher.context_of(&id));
    let (used, size) = match ctx {
        Some((u, s)) => (Some(u), Some(s)),
        None => (None, None),
    };
    let pct = |u: Option<u32>, s: Option<u32>| {
        u.zip(s).map(|(u, s)| (u as f32 / s as f32 * 100.0) as i32)
    };
    if pct(model.ctx_used, model.ctx_size) != pct(used, size) {
        changed = true;
    }
    model.ctx_used = used;
    model.ctx_size = size;
    (applied, changed)
}

fn render(model: &Model) -> Result<Vec<u8>> {
    let fonts = Fonts::load()?;
    let scene = layout::draw(model, now());
    raster::to_png(&scene, &fonts)
}

/// 设备占用闸门：只有「确实该占着」的时候才握着这条独占连接。
///
/// - 有任务在跑（Working）→ 占着，它随时会换画面。
/// - 有确认/提问在等你（Waiting）→ 也占着：设备就是那条「回电脑处理」的通知，
///   到点撒手会让提示和连接一起消失。实测一条写入确认等 18 分钟，旧规则
///   5 分钟就释放，随后新记录又把它拉回来，20 分钟里抖了 13 轮。
/// - 一轮对话结束（完成 / 已停止）→ 从最后一次活动起算，静默 `grace` 秒就交还。
///
/// 判定用「最后一个任务多久没动静」而不是内存计时器：重启、重连后依然成立，
/// 也天然覆盖「任务在断开期间结束」的情况。超过 `stale_minutes` 没更新的
/// Working 已经算「状态待确认」，同样不再占着设备。
fn should_hold(model: &Model, at: i64, grace: i64) -> bool {
    let active = model.running(at);
    if active.iter().any(|t| t.status() == Status::Working) {
        return true;
    }
    if active.iter().any(|t| t.status() == Status::Waiting) {
        return true;
    }
    match model.tasks.iter().map(|t| t.updated_at).max() {
        Some(last) => at.saturating_sub(last) < grace.max(0),
        // 一个任务都没有（比如手动 run --standby）：没有可等的，不必占用。
        None => false,
    }
}

/// 是否该交还设备。与 `should_hold` 同源，避免「占用」和「释放」两处判定漂移。
fn may_release(model: &Model, at: i64, grace: i64) -> bool {
    !should_hold(model, at, grace)
}

/// 状态行里的会话短号：`session-<前 8 位>`，不含标题或提问正文。
/// 没有 thread_id 时从 task_id 去掉轮次后缀再取前 8 位；都没有就用 task。
fn short_session(thread: Option<&str>) -> Option<String> {
    let raw = thread?.trim();
    if raw.is_empty() {
        return None;
    }
    let core = raw.strip_prefix("session-").unwrap_or(raw);
    // 会话 id 常见形态 `session-<uuid>`：取 uuid 第一段（最多 8 位）当短号。
    let short: String = core.chars().take_while(|c| *c != '-').take(8).collect();
    if short.is_empty() {
        return None;
    }
    Some(format!("session-{short}"))
}

fn short_task_ref(ev: &TaskEvent) -> String {
    short_session(ev.thread_id.as_deref())
        .or_else(|| {
            let base = ev
                .task_id
                .rsplit_once("-t")
                .map(|(b, _)| b)
                .unwrap_or(&ev.task_id);
            short_session(Some(base))
        })
        .unwrap_or_else(|| "task".to_string())
}

/// 把已有任务的标题抹成不含正文的短号，返回实际替换条数。
/// `forget --titles` 与测试共用这一份规则。
fn scrub_task_titles(model: &mut Model) -> usize {
    let mut replaced = 0usize;
    for (i, t) in model.tasks.iter_mut().enumerate() {
        let label = short_session(t.event.thread_id.as_deref())
            .unwrap_or_else(|| format!("task-{}", i + 1));
        if t.event.title != label {
            t.event.title = label;
            replaced += 1;
        }
    }
    replaced
}

/// 扫描一次会话：只有真实状态变化才落盘、打日志、响提示音。
/// `allow_chime` 用于抑制首次导入历史时的一串提示音；`verbose` 决定状态行
/// 是否附带标题（默认只记 `session-短号` + 状态词）。
async fn scan_and_report(
    model: &mut Model,
    watcher: &mut Watcher,
    store: &mut Store,
    dev: &Device,
    allow_chime: bool,
    verbose: bool,
) -> bool {
    let before: Vec<(String, Option<Status>)> = model
        .tasks
        .iter()
        .filter(|t| t.source == "dsh")
        .map(|t| (t.event.task_id.clone(), Some(t.status())))
        .collect();
    let (events, changed) = scan_into(model, watcher);
    if changed {
        store.model = model.clone();
        if let Err(e) = save_store(store) {
            eprintln!("配置保存失败: {e}");
        }
    }
    for (i, ev) in events.iter().enumerate() {
        let previous = before
            .iter()
            .find(|(id, _)| *id == ev.task_id)
            .and_then(|(_, s)| *s);
        let progress = ev
            .progress
            .map(|p| format!("（清单 {p}%）"))
            .unwrap_or_default();
        if verbose {
            println!(
                "[状态] {} · {}{} · 标题：{}",
                short_task_ref(ev),
                ev.status.label(),
                progress,
                ev.title
            );
        } else {
            // 默认只留会话短号 + 状态词：pet.log 可能被转发/上传，标题可能来自
            // 提示词首行（prompt 模式）或项目名，不应出现在日志里。
            println!("[状态] {} · {}", short_task_ref(ev), ev.status.label());
        }
        // 同一批记录里可能先出现「完成」又立刻回到「工作」（上一轮完成记录
        // 延迟补写的场景），提示音只认每个任务的最终状态，中间态不响铃。
        let is_final = !events[i + 1..].iter().any(|e| e.task_id == ev.task_id);
        if is_final && sound::should_chime(allow_chime, previous, ev.status) {
            let target = model
                .tasks
                .iter()
                .find(|t| t.source == "dsh" && t.event.task_id == ev.task_id)
                .cloned();
            if let Some(task) = target {
                announce(dev, model, store, &task).await;
            }
        }
    }
    changed
}

/// 主卡上「值得立刻刷新」的那几个字段的快照，用于节流判定。
#[derive(Clone, PartialEq)]
struct CardSig {
    status: Option<Status>,
    title: String,
    detail: String,
    progress: Option<u8>,
    ctx_pct: Option<u32>,
}

/// 取当前主卡快照。上下文百分比只在整数位变化时才算变化（见 scan_into）。
fn card_sig(model: &Model, at: i64) -> CardSig {
    let ctx_pct = model
        .ctx_used
        .zip(model.ctx_size)
        .map(|(u, s)| if s == 0 { 0 } else { (u as f32 / s as f32 * 100.0) as u32 });
    match model.current(at) {
        Some(t) => CardSig {
            status: Some(t.status()),
            title: t.event.title.clone(),
            detail: t.event.detail.clone(),
            progress: t.event.progress,
            ctx_pct,
        },
        None => CardSig {
            status: None,
            title: String::new(),
            detail: String::new(),
            progress: None,
            ctx_pct,
        },
    }
}

/// 这一拍该不该刷屏。
///
/// - 状态切换（工作→等待→完成/停止/出错）、任务换人、等待页文案变化 → **立刻刷**，
///   这些是用户真正要看的信号；
/// - 其余（工具活动描述、进度、上下文百分比）→ 受 `refresh_secs` 节流，
///   默认 0 表示不节流（保持原来实时跟手的行为），设成 30/60 就安静很多。
fn refresh_due(
    last: Option<&CardSig>,
    cur: &CardSig,
    last_push_at: i64,
    at: i64,
    throttle: i64,
) -> bool {
    let Some(prev) = last else {
        return true;
    };
    if prev.status != cur.status || prev.title != cur.title {
        return true;
    }
    // 等待页的题干/权限提示变了必须立刻上屏——那就是要人处理的信号。
    if cur.status == Some(Status::Waiting) && prev.detail != cur.detail {
        return true;
    }
    if throttle <= 0 {
        return true;
    }
    at.saturating_sub(last_push_at) >= throttle
}

/// 花絮采样间隔：两次 UIA 调用之间至少隔这么久。
const QUIP_INTERVAL: Duration = Duration::from_secs(2);

/// 后台花絮采样器：UIA 是跨进程 COM 调用（`current_quip` 内部还会重试 8 次、
/// 最坏阻塞 1.5 秒），**绝不能放在推送路径上**。单独起一个线程，每 2 秒最多采一次，
/// 成功结果塞进 `Arc<Mutex<Option<String>>>`；推送时只读缓存，拿锁是微秒级。
///
/// 采样线程是进程级单例：`watch` 会反复进出 `cmd_run`，不能每次会话都再起一条。
struct QuipSampler {
    cache: Arc<Mutex<Option<String>>>,
}

impl QuipSampler {
    /// 进程内唯一实例；首次调用时启动线程并**立即**采一次。
    fn global() -> &'static QuipSampler {
        static INSTANCE: OnceLock<QuipSampler> = OnceLock::new();
        INSTANCE.get_or_init(QuipSampler::spawn)
    }

    fn spawn() -> Self {
        let cache = Arc::new(Mutex::new(None));
        let shared = Arc::clone(&cache);
        let spawned = std::thread::Builder::new()
            .name("quip-sampler".into())
            .spawn(move || loop {
                // 启动后第一轮就采，别等 2 秒；读不到（窗口没开/UIA 被占）就保留
                // 上一句——传 None 会把屏幕上已有的花絮擦掉，那不是用户想看的。
                if let Some(quip) = uia::current_quip() {
                    if let Ok(mut slot) = shared.lock() {
                        *slot = Some(quip);
                    }
                }
                std::thread::sleep(QUIP_INTERVAL);
            });
        if spawned.is_err() {
            // 起不来就退化成「永远读不到花絮」：推送路径完全不受影响。
            eprintln!("[花絮] 采样线程启动失败，界面花絮将不可用");
        }
        Self { cache }
    }

    /// 推送路径唯一入口：只读缓存，不碰 UIA。
    fn cached(&self) -> Option<String> {
        self.cache.lock().ok().and_then(|slot| slot.clone())
    }
}

/// 场景签名：按元素逐个哈希字段（类型 / 坐标 / 文本 / 颜色…），不分配字符串。
///
/// 原来是 `scene.to_string()` 再比字符串：每拍都要把整棵场景树序列化一遍
/// （几十 KB、几十次分配），而这里其实只需要一个「变没变」的布尔。
fn scene_signature(elements: &[serde_json::Value]) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    elements.len().hash(&mut hasher);
    for element in elements {
        hash_value(element, &mut hasher);
    }
    hasher.finish()
}

/// 整个场景（`{"elements": [...]}`）的签名：取元素数组交给 `scene_signature`。
fn scene_value_signature(scene: &serde_json::Value) -> u64 {
    let empty: &[serde_json::Value] = &[];
    let elements = scene
        .get("elements")
        .and_then(|v| v.as_array())
        .map(|v| v.as_slice())
        .unwrap_or(empty);
    scene_signature(elements)
}

/// 递归哈希一个 JSON 值。每层先写一个类型判别字节：否则 `{"1":2}` 与
/// `{"1":"2"}` 这类结构不同但拼接相同的值会撞在一起。
fn hash_value(value: &serde_json::Value, hasher: &mut impl std::hash::Hasher) {
    use serde_json::Value;
    use std::hash::Hash;
    match value {
        Value::Null => 0u8.hash(hasher),
        Value::Bool(b) => {
            1u8.hash(hasher);
            b.hash(hasher);
        }
        Value::Number(n) => {
            2u8.hash(hasher);
            // 整数与浮点分开喂：布局里不会混用，但区分开更保险。
            if let Some(i) = n.as_i64() {
                0u8.hash(hasher);
                i.hash(hasher);
            } else if let Some(u) = n.as_u64() {
                1u8.hash(hasher);
                u.hash(hasher);
            } else if let Some(f) = n.as_f64() {
                2u8.hash(hasher);
                f.to_bits().hash(hasher);
            }
        }
        Value::String(s) => {
            3u8.hash(hasher);
            s.hash(hasher);
        }
        Value::Array(items) => {
            4u8.hash(hasher);
            items.len().hash(hasher);
            for item in items {
                hash_value(item, hasher);
            }
        }
        Value::Object(map) => {
            5u8.hash(hasher);
            map.len().hash(hasher);
            // serde_json 默认用 BTreeMap，键有序，遍历顺序稳定。
            for (key, item) in map {
                key.hash(hasher);
                hash_value(item, hasher);
            }
        }
    }
}

/// 当前模型渲染出的场景元素数：`--trace-tick` 打印用，只在真要打印时才调。
fn scene_element_count(model: &Model) -> usize {
    layout::draw(model, now())
        .get("elements")
        .and_then(|v| v.as_array())
        .map_or(0, Vec::len)
}

/// `--trace-tick` 的采样器：只在开启时记录每拍耗时（毫秒），每 10 拍打印一行，
/// 退出时给出平均 / P95 / 峰值。默认关闭时 `record` 直接返回，等于零开销。
struct TickTrace {
    enabled: bool,
    ticks: u64,
    /// 只留最近 600 拍（1 秒一拍 = 10 分钟）：报告要的是分布，不是全量历史。
    samples: std::collections::VecDeque<u64>,
}

impl TickTrace {
    fn from_args(args: &Args) -> Self {
        Self {
            enabled: args.has("trace-tick"),
            ticks: 0,
            samples: std::collections::VecDeque::new(),
        }
    }

    /// 记录一拍。`elements` 是惰性求值的：每 10 拍才真的渲染一次数元素，
    /// 关闭追踪时连这个闭包都不会被调用。
    fn record(&mut self, started: Instant, elements: impl FnOnce() -> usize) {
        if !self.enabled {
            return;
        }
        let ms = started.elapsed().as_millis() as u64;
        self.samples.push_back(ms);
        if self.samples.len() > 600 {
            self.samples.pop_front();
        }
        self.ticks = self.ticks.wrapping_add(1);
        if self.ticks % 10 == 0 {
            println!("[tick] {ms}ms elements={}", elements());
        }
    }

    fn summary(&self) -> Option<String> {
        if !self.enabled || self.samples.is_empty() {
            return None;
        }
        let mut sorted: Vec<u64> = self.samples.iter().copied().collect();
        sorted.sort_unstable();
        let sum: u64 = sorted.iter().sum();
        let avg = sum as f64 / sorted.len() as f64;
        let p95 = sorted[(sorted.len() * 95 / 100).min(sorted.len() - 1)];
        Some(format!(
            "[tick] 共 {} 拍，平均 {avg:.1} ms，P95 {p95} ms，最大 {} ms",
            sorted.len(),
            sorted[sorted.len() - 1]
        ))
    }
}

/// 只在画面签名变化时推送。签名涵盖状态、标题、进度与当前选中项，
/// 没有真实变化就一个字节都不发——电子墨水屏刷新很贵，也不该无谓闪烁。
/// 把当前模型渲染上屏；画面没变就不重刷。
///
/// 花絮（底部那句）**不再单独触发刷新**：只有主画面确实要更新时才顺便取一次
/// 后台采样好的界面状态行，把花絮并进同一帧。否则界面状态行每变一次
/// （每个工具调用都变）就会白闪一下墨水屏——这正是「每 2 秒闪一次」的另一半原因。
async fn push_if_changed(
    dev: &Device,
    model: &mut Model,
    store: &mut Store,
    quips: &QuipSampler,
    fonts: &Fonts,
    fmt: ImageFormat,
    last_sig: &mut Option<u64>,
) -> bool {
    let mut scene = layout::draw(model, now());
    // 只比哈希，不序列化整棵树：空闲时这一句省下每秒几十 KB 的分配。
    let mut sig = scene_value_signature(&scene);
    if last_sig.as_ref() == Some(&sig) {
        // 主画面没变：连花絮缓存都不读，安静地返回。
        return false;
    }
    // 主画面确实要更新了，顺带把后台采好的花絮并进这一帧。
    // 只读缓存：这里做任何 UIA 调用都会把推送拖慢到 1.5 秒级。
    if let Some(quip) = quips.cached() {
        if model.set_live_quip(&quip) {
            println!("[花絮] {quip}");
            store.model = model.clone();
            if let Err(e) = save_store(store) {
                eprintln!("配置保存失败: {e}");
            }
            scene = layout::draw(model, now());
            // 花絮上了屏，签名要跟着这一帧走，否则下次同样的画面会被当成没变。
            sig = scene_value_signature(&scene);
        }
    }
    let png = match raster::to_png(&scene, fonts) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("渲染失败: {e}");
            return false;
        }
    };
    let show_started = std::time::Instant::now();
    match dev.show_with_format(&png, fmt).await {
        Ok(()) => match model.current(now()) {
            // 只记状态词，不记标题：stdout 会进 pet.log，可能与 state.json 一起被转发。
            Some(t) => println!(
                "[{}] 已推送：{}（{:.2}s）",
                chrono::Local::now().format("%H:%M:%S%.3f"),
                t.label(),
                show_started.elapsed().as_secs_f32()
            ),
            None => println!(
                "[{}] 已推送：等待任务（{:.2}s）",
                chrono::Local::now().format("%H:%M:%S%.3f"),
                show_started.elapsed().as_secs_f32()
            ),
        },
        // 结果未知时按约定不重发，也不当作失败。
        Err(zectrix_sdk::Error::ResultUnknown) => {
            eprintln!("设备未确认显示结果（ResultUnknown），不重试；请在设备上目检");
        }
        Err(e) => eprintln!("推送失败（不重试）: {e}"),
    }
    // 失败也记录签名：宁可停在旧画面，也不对同一帧反复重刷。
    *last_sig = Some(sig);
    true
}

/// 把现成的场景树推到设备（过渡页用：它不属于 `Model` 的正常排版）。
///
/// 返回是否真的推了：签名相同就不推，避免对一个静态页面反复刷墨水屏。
async fn push_scene(
    dev: &Device,
    scene: serde_json::Value,
    fonts: &Fonts,
    fmt: ImageFormat,
    last_sig: &mut Option<u64>,
) -> bool {
    let sig = scene_value_signature(&scene);
    if last_sig.as_ref() == Some(&sig) {
        return false;
    }
    let png = match raster::to_png(&scene, fonts) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("渲染失败: {e}");
            return false;
        }
    };
    match dev.show_with_format(&png, fmt).await {
        Ok(()) => {}
        Err(zectrix_sdk::Error::ResultUnknown) => {
            eprintln!("设备未确认显示结果（ResultUnknown），不重试；请在设备上目检");
        }
        Err(e) => eprintln!("推送失败（不重试）: {e}"),
    }
    *last_sig = Some(sig);
    true
}

fn describe(model: &Model) -> String {
    let at = now();
    let current = model.current(at);
    let running = model.running(at);
    let mut out = String::new();
    match &current {
        Some(t) => {
            out.push_str(&format!("当前：{} · {}\n", t.label(), t.event.title));
            out.push_str(&format!("来源：{}", t.source));
            if let Some(p) = t.event.progress {
                match t.event.progress_steps {
                    Some((done, total)) => out.push_str(&format!(" · 清单 {done}/{total}（{p}%）")),
                    None => out.push_str(&format!(" · 上报进度 {p}%")),
                }
            }
            let age = at.saturating_sub(t.updated_at);
            out.push_str(&format!(" · {} 秒前更新\n", age.max(0)));
            if !t.event.detail.is_empty() {
                out.push_str(&format!("详情：{}\n", t.event.detail));
            }
        }
        None => out.push_str("当前：无任务（等待任务）\n"),
    }
    out.push_str(&format!(
        "进行中/等待：{} 个｜已知任务：{} 个｜跟随：{}｜提示音：{}（音量 {}）\n",
        running.len(),
        model.tasks.len(),
        if model.settings.follow_enabled {
            "开"
        } else {
            "关"
        },
        if model.settings.sound_enabled {
            "开"
        } else {
            "关"
        },
        model.settings.volume
    ));
    out
}

fn write_png(path: &Path, png: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, png).with_context(|| format!("无法写入 {}", path.display()))?;
    Ok(())
}

fn label_of(model: &Model) -> String {
    model
        .current(now())
        .map(|t| match t.status() {
            Status::Working => "working",
            Status::Waiting => "waiting",
            Status::Done => "done",
            Status::Error => "error",
            Status::Stopped => "stopped",
            Status::Unknown => "unknown",
        })
        .unwrap_or("idle")
        .to_string()
}

fn with_model(args: &Args, store: &mut Store) -> Model {
    let mut model = store.model.clone();
    match args.get("demo").map(parse_status) {
        Some(Ok(demo)) => {
            // 演示状态只存在于内存，绝不写入任务历史。
            model.demo = demo;
            model
        }
        Some(Err(e)) => {
            eprintln!("{e}");
            model
        }
        None => {
            let mut watcher = Watcher::new(dsh_root());
            if let Some(stem) = args.get("session") {
                // 只看这一个会话：否则当前任务会被其它会话更新的记录顶掉。
                let prefix = format!("{stem}-");
                model
                    .tasks
                    .retain(|t| t.source != "dsh" || t.event.task_id.starts_with(&prefix));
                watcher = watcher.only(stem);
            }
            scan_into(&mut model, &mut watcher);
            // 预览/手动推图也要认「子智能体还在跑」，否则设备显示和 DSH 界面不一致。
            let mut team = TeamScan::default();
            team.sync(&mut model, &dsh_root());
            // 单会话预览不落盘：避免把其它会话的任务从状态缓存里抹掉。
            if args.get("session").is_none() {
                store.model = model.clone();
                let _ = save_store_now(store);
            }
            model
        }
    }
}

/// `on` / `off` / `true` / `false` / `1` / `0` / `开` / `关`。
fn parse_onoff(value: &str) -> Result<bool> {
    Ok(match value.trim().to_ascii_lowercase().as_str() {
        "on" | "true" | "1" | "yes" | "开" => true,
        "off" | "false" | "0" | "no" | "关" => false,
        other => anyhow::bail!("只能是 on / off，收到 {other}"),
    })
}

fn onoff_label(value: bool) -> &'static str {
    if value {
        "开"
    } else {
        "关"
    }
}

/// 查看 / 修改跟随与提示音设置（`dsh-pet set ...`）。
///
/// 写 `pet-config.json`（唯一用户配置源，插件面板也读写它）；`state.json` 里的
/// `model.settings` 只是守护留下的只读快照。守护每一拍重新读配置，所以改完 2 秒
/// 内生效，不需要重启守护。`--follow off` 会让守护释放设备（回到待命、不再占用），
/// 这是「暂时让它别上屏」的开关；插件被禁用或 DSH 退出时宠物随之收尾。
fn cmd_set(args: &Args) -> Result<()> {
    let mut store = load_store();
    // 先套一遍配置：「不带参数」打印的就是真正生效的值，而不是 state.json 旧快照。
    let _ = apply_user_config(&mut store);
    let mut patch = PetConfigPatch::default();
    if let Some(v) = args.get("volume") {
        let n: u8 = v
            .parse()
            .with_context(|| format!("--volume 需要 0-100 的整数，收到 {v}"))?;
        patch.volume = Some(n.min(100));
    }
    if let Some(v) = args.get("sound") {
        patch.sound = Some(parse_onoff(v)?);
    }
    if let Some(v) = args.get("follow") {
        patch.follow = Some(parse_onoff(v)?);
    }
    if let Some(v) = args.get("stale") {
        let n: u32 = v
            .parse()
            .with_context(|| format!("--stale 需要分钟数，收到 {v}"))?;
        patch.stale_minutes = Some(n.max(1));
    }
    if let Some(v) = args.get("refresh") {
        let n: u32 = v
            .parse()
            .with_context(|| format!("--refresh 需要秒数（0 = 不节流），收到 {v}"))?;
        patch.refresh_secs = Some(n);
        patch.clear_mode = true;
    }
    if let Some(v) = args.get("title-source") {
        patch.title_source = Some(TitleSource::parse(v).with_context(|| {
            format!("--title-source 只能是 session（默认）或 prompt，收到 {v}")
        })?);
    }
    if let Some(v) = args.get("format") {
        // 只接受 mono / gray4 / gray16；写回统一的名字，守护下一拍热切换。
        patch.format = Some(device::format_name(device::format(v)?).to_string());
    }
    // 一键「安静模式」：30 秒节流。嫌墨水屏一闪一闪时用这个；
    // `--calm off` 恢复实时跟随。
    if let Some(v) = args.get("calm") {
        let calm = parse_onoff(v)?;
        patch.refresh_secs = Some(if calm { 30 } else { 0 });
        patch.clear_mode = true;
    }
    if patch.is_empty() {
        let s = &store.model.settings;
        println!("跟随：{}", onoff_label(s.follow_enabled));
        println!(
            "提示音：{}（音量 {} / 100）",
            onoff_label(s.sound_enabled),
            s.volume
        );
        if s.refresh_secs == 0 {
            println!("刷新节流：关（内容变化就刷，最多 2 秒一次）");
        } else {
            println!("刷新节流：至少 {} 秒一次（状态切换仍立即刷）", s.refresh_secs);
        }
        println!("陈旧判定：{} 分钟无更新降级为「状态待确认」", s.stale_minutes);
        println!(
            "标题来源：{}（session = 只读 DSH 会话标题；prompt = 允许把提示词首行写进 state.json / 设备）",
            s.title_source.as_str()
        );
        println!(
            "屏幕格式：{}（mono 刷新最快/最不闪，gray4 折中，gray16 最细腻）",
            store.format
        );
        println!();
        println!("配置位置：{}", pet_config_path().display());
        println!();
        println!("改法示例：");
        println!("  dsh-pet set --calm on            安静模式：30 秒节流（推荐先试这个）");
        println!("  dsh-pet set --refresh 60         内容更新至少隔 60 秒才刷");
        println!("  dsh-pet set --format mono        黑白 1 位：整屏刷新快 4 倍、闪得少（默认）");
        println!("  dsh-pet set --sound off          完全静音");
        println!("  dsh-pet set --volume 20          只把提示音调小（默认 90）");
        println!("  dsh-pet set --follow off         暂停跟随，释放设备");
        println!("  dsh-pet set --title-source prompt 允许标题取提示词首行（会写入 state.json / 设备）");
        println!("  dsh-pet set --title-source session 恢复默认：标题只来自 DSH 会话标题");
        return Ok(());
    }
    write_pet_config_at(&pet_config_path(), &patch)?;
    // 立刻套回来：打印的是真生效值，state.json 里的快照也跟着对齐。
    let _ = apply_user_config(&mut store);
    save_store_now(&store)?;
    let s = &store.model.settings;
    println!(
        "已保存：跟随 {}｜提示音 {}（音量 {}）｜刷新节流 {}｜陈旧 {} 分钟｜屏幕 {}｜标题来源 {}",
        onoff_label(s.follow_enabled),
        onoff_label(s.sound_enabled),
        s.volume,
        if s.refresh_secs == 0 {
            "关".to_string()
        } else {
            format!("{} 秒", s.refresh_secs)
        },
        s.stale_minutes,
        store.format,
        s.title_source.as_str()
    );
    println!("写入：{}", pet_config_path().display());
    println!("守护下一拍生效，不需要重启。");
    Ok(())
}

async fn cmd_preview(args: &Args) -> Result<()> {
    let mut store = load_store();
    let _ = apply_user_config(&mut store);
    let model = with_model(args, &mut store);
    let scene = layout::draw(&model, now());
    if args.has("scene") {
        println!("{}", serde_json::to_string_pretty(&scene)?);
        return Ok(());
    }
    let png = raster::to_png(&scene, &Fonts::load()?)?;
    let path = args
        .get("out")
        .map(PathBuf::from)
        .unwrap_or_else(|| out_dir().join(format!("pet-{}.png", label_of(&model))));
    write_png(&path, &png)?;
    print!("{}", describe(&model));
    println!("画布：400×300 连续灰阶（未量化、未抖动）");
    println!("预览：{}（{} 字节）", path.display(), png.len());
    Ok(())
}

async fn cmd_sessions(_args: &Args) -> Result<()> {
    let mut watcher = Watcher::new(dsh_root());
    println!("会话根目录：{}", watcher.root().display());
    let events = watcher.scan()?;
    if events.is_empty() {
        println!("没有可跟随的会话（最近 12 小时无活动或目录为空）");
    }
    for (e, at) in events {
        let progress = match (e.progress, e.progress_steps) {
            (Some(p), Some((done, total))) => format!("清单 {done}/{total} · {p}%"),
            (Some(p), None) => format!("上报 {p}%"),
            (None, _) => "-".to_string(),
        };
        println!(
            "{:<26} {:<10} {:<18} {}  [{}]",
            e.task_id,
            match e.status {
                Status::Working => "工作",
                Status::Waiting => "等你处理",
                Status::Done => "完成",
                Status::Error => "出错",
                Status::Stopped => "已停止",
                Status::Unknown => "待确认",
            },
            progress,
            e.title,
            at
        );
    }
    Ok(())
}

async fn cmd_check(args: &Args) -> Result<()> {
    let mut store = load_store();
    let _ = apply_user_config(&mut store);
    let target = args.get("target").map(str::to_string).or(store.target);
    let dev = device::open(target.as_deref()).await?;
    println!(
        "已连接: id={} name={} addr={}",
        dev.summary.id, dev.summary.name, dev.summary.address
    );
    match dev.status().await {
        Ok(s) => println!(
            "电量 {:?}% | 充电 {:?} | RSSI {:?} dBm",
            s.battery_percent, s.charging, s.wifi_rssi_dbm
        ),
        Err(e) => println!("读取状态失败: {e}"),
    }
    let p = dev.palette();
    println!("色板 gray4={:?}", p.gray4);
    println!("色板 gray16={:?}", p.gray16);
    dev.close().await?;
    println!("已断开（未推送任何画面）");
    Ok(())
}

/// 交互确认：读不到输入（管道/非交互）就按「否」处理，不挂住脚本。
fn confirm(prompt: &str) -> bool {
    print!("{prompt} [y/N] ");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).is_err() {
        return false;
    }
    matches!(
        line.trim().to_ascii_lowercase().as_str(),
        "y" | "yes" | "是"
    )
}

/// 首次使用引导：先讲清现状，再连设备、生成预览、按用户确认推送。
/// 每一步都允许失败而不中断——连不上设备仍能看预览，修正后重跑即可。
async fn cmd_init(args: &Args) -> Result<()> {
    let mut store = load_store();
    let _ = apply_user_config(&mut store);
    println!("DeepSeek Harness 宠物 · 初始化\n");

    // 1. 现状：把「程序当前认到的配置」先摆出来，用户才能判断哪里不对。
    let sessions = dsh_root();
    println!("数据目录      {}", data_dir().display());
    println!("用户配置      {}", pet_config_path().display());
    println!(
        "状态缓存      {}（{} 个任务）",
        state_path().display(),
        store.model.tasks.len()
    );
    println!(
        "DeepSeek Harness    会话 {} ｜进程 {}",
        if sessions.is_dir() {
            "已找到"
        } else {
            "未找到（先启动一次 DeepSeek Harness）"
        },
        if dsh_running_cached() {
            "正在运行"
        } else {
            "未运行"
        }
    );
    println!(
        "已保存设备    {}",
        store.target.as_deref().unwrap_or("（未设置）")
    );
    println!(
        "当前设置      跟随 {}｜提示音 {}（音量 {}）｜格式 {}",
        if store.model.settings.follow_enabled {
            "开"
        } else {
            "关"
        },
        if store.model.settings.sound_enabled {
            "开"
        } else {
            "关"
        },
        store.model.settings.volume,
        store.format
    );

    // 2. 设备
    println!("\n[1/2] 连接设备");
    let mut target = args
        .get("target")
        .map(str::to_string)
        .or(store.target.clone());
    // 用户显式写下的地址就有价值：先存起来（写 pet-config.json），连不上也不至于
    // 下次还要再输一遍。
    if let Some(t) = args.get("target").map(str::to_string) {
        if store.target.as_deref() != Some(t.as_str()) {
            write_pet_config_at(
                &pet_config_path(),
                &PetConfigPatch {
                    target: Some(t),
                    ..Default::default()
                },
            )?;
            let _ = apply_user_config(&mut store);
            save_store_now(&store)?;
            println!(
                "  已保存设备地址：{}",
                store.target.as_deref().unwrap_or_default()
            );
        }
    }
    if target.is_none() {
        println!("  未指定地址：自动发现（最多 5 秒）…");
    }
    let mut dev: Option<Device> = None;
    match device::open(target.as_deref()).await {
        Ok(d) => {
            println!(
                "  已连接  id={}  name={}  addr={}",
                d.summary.id, d.summary.name, d.summary.address
            );
            match d.status().await {
                Ok(s) => println!(
                    "  电量 {:?}%｜充电 {:?}｜信号 {:?} dBm",
                    s.battery_percent, s.charging, s.wifi_rssi_dbm
                ),
                Err(e) => println!("  读取设备状态失败：{e}"),
            }
            // 记住地址（写 pet-config.json）：之后 watch/run/push 不带 --target 也能用。
            if store.target.as_deref() != Some(d.summary.address.as_str()) {
                write_pet_config_at(
                    &pet_config_path(),
                    &PetConfigPatch {
                        target: Some(d.summary.address.clone()),
                        ..Default::default()
                    },
                )?;
                let _ = apply_user_config(&mut store);
                save_store_now(&store)?;
                println!("  已保存设备地址：{}", d.summary.address);
            }
            target = store.target.clone();
            dev = Some(d);
        }
        Err(e) => {
            // device::open 用 anyhow 包装，多设备要回原始错误类型才能列出候选。
            if let Some(zectrix_sdk::Error::MultipleDevices(list)) =
                e.downcast_ref::<zectrix_sdk::Error>()
            {
                println!("  发现多台设备，请用 --target 指定其一后重跑：");
                for d in list {
                    println!("    {:<16} {:<14} {}", d.address, d.id, d.name);
                }
            } else {
                println!("  连接失败：{e}");
                println!("  逐项检查：① 设备与电脑同一局域网 ② 地址正确");
                println!("            ③ 设备「设置 → 网络同步 → 待机保持 Wi-Fi」已开");
                println!("            ④ 没有别的程序占用（旧守护先停，强杀后等 15 秒）");
            }
        }
    }

    // 3. 预览：先让用户看到要推到设备上的到底是什么。
    println!("\n[2/2] 生成预览");
    let mut watcher = Watcher::new(dsh_root());
    scan_into(&mut store.model, &mut watcher);
    let scene = layout::draw(&store.model, now());
    let png = raster::to_png(&scene, &Fonts::load()?)?;
    let path = out_dir().join("pet-init.png");
    write_png(&path, &png)?;
    print!("{}", describe(&store.model));
    println!(
        "  预览图：{}（400×300，{} 字节）",
        path.display(),
        png.len()
    );

    if let Some(d) = dev {
        if confirm("  先打开上面的预览确认版式，然后把它推到设备？") {
            let fmt = device::format(args.get("format").unwrap_or(&store.format))?;
            match d.show_with_format(&png, fmt).await {
                Ok(()) => println!("  设备已完成显示（{}）", device::format_name(fmt)),
                Err(zectrix_sdk::Error::ResultUnknown) => println!(
                    "  设备未确认结果（ResultUnknown）：按 SDK 约定不重复推送，请在设备上目检"
                ),
                Err(e) => println!("  推送失败：{e}"),
            }
        } else {
            println!("  已跳过推送");
        }
        let _ = d.close().await;
        println!("  已断开连接（不占用设备）");
    }

    // 不再写任何开机自启/快捷方式：插件形态下 DSH 启动时自己拉起宠物，
    // 旧版本的残留由 `watch` 加载阶段清一次（见 retire_legacy_autostart）。

    println!(
        "\n初始化结束。日常使用：`dsh-pet watch{}`",
        target.map(|t| format!(" --target {t}")).unwrap_or_default()
    );
    Ok(())
}

async fn cmd_push(args: &Args) -> Result<()> {
    let mut store = load_store();
    let _ = apply_user_config(&mut store);
    let model = with_model(args, &mut store);
    let fmt = device::format(args.get("format").unwrap_or(&store.format))?;
    let png = render(&model)?;
    let target = args
        .get("target")
        .map(str::to_string)
        .or(store.target.clone());
    print!("{}", describe(&model));

    let dev = device::open(target.as_deref()).await?;
    println!(
        "已连接: id={} model={} addr={}",
        dev.summary.id, dev.summary.name, dev.summary.address
    );
    let result = dev.show_with_format(&png, fmt).await;
    let closed = dev.close().await;
    match result {
        Ok(()) => println!(
            "设备已完成显示（{}，{} 字节 PNG）",
            device::format_name(fmt),
            png.len()
        ),
        Err(zectrix_sdk::Error::ResultUnknown) => {
            println!("设备未确认本次显示结果（ResultUnknown）：按 SDK 约定不重试、不重复刷图，请在设备上目检。");
        }
        Err(e) => return Err(anyhow::anyhow!("推送失败: {e}")),
    }
    closed?;
    Ok(())
}

/// 本次连接建立的时刻。刚连上时音频通路可能还没就绪，立刻播放会「咔」一声
/// （实测重连后紧跟着的第一声尤其明显），先让链路安静几秒再响。
static CONNECTED_AT: AtomicI64 = AtomicI64::new(0);

async fn chime(dev: &zectrix_sdk::Device, status: Status, volume: u8) -> bool {
    let since = CONNECTED_AT.load(Ordering::Relaxed);
    if since > 0 {
        let wait = SOUND_SETTLE - (now() - since);
        if wait > 0 {
            tokio::time::sleep(Duration::from_secs(wait as u64)).await;
        }
    }
    let audio = device::clip(&sound::pcm(status));
    match dev.play(&audio, volume).await {
        Ok(_) => {
            println!("提示音：已播放（{}）", status.label());
            true
        }
        // 播放失败不重试，切回前台也不补响。
        Err(e) => {
            eprintln!("提示音播放失败（不重试）: {e}");
            false
        }
    }
}

/// 为某个任务提醒一次，并把指纹记到磁盘。
/// 指纹相同就跳过：等待页每 2 秒被重复上报时不会反复响，设备超时重连后
/// 也不会把同一条等待再响一遍；换一张确认卡、换一个提问则指纹不同，会再响。
async fn announce(dev: &Device, model: &Model, store: &mut Store, task: &Task) {
    if !model.settings.sound_enabled {
        return;
    }
    if let Some(sig) = sound::notice_sig(task) {
        if !store.announced.iter().any(|s| s == &sig) && chime(dev, task.status(), model.settings.volume).await {
            store.announced.push(sig);
            let len = store.announced.len();
            if len > ANNOUNCED_KEEP {
                store.announced.drain(..len - ANNOUNCED_KEEP);
            }
            store.announced_at = now();
            if let Err(e) = save_store_now(store) {
                eprintln!("配置保存失败: {e}");
            }
        }
    }
}

/// 录音事件的兜底时限：`RecordingStarted` 之后这么久还没 `RecordingFinished`，
/// 就认为通知丢了、自己复位（否则 `if recording { continue }` 会让扫描/推送永久停摆）。
const RECORDING_TIMEOUT: Duration = Duration::from_secs(10);

/// 原地重连的退避序列：第 1/2/3 次分别等 1s、2s、4s。
const RECONNECT_BACKOFF_SECS: [u64; 3] = [1, 2, 4];

/// 录音是否已经超时（纯函数，便于单测）。
fn recording_expired(now: Instant, started_at: Option<Instant>) -> bool {
    started_at.is_some_and(|start| now.saturating_duration_since(start) >= RECORDING_TIMEOUT)
}

/// 这一下点击要不要立刻推帧。单击立即推；双击的第二下不推——静音结果等
/// 650ms 判定窗口结束后由 tick 统一推一帧，否则一次双击会连闪 3 屏。
fn gesture_pushes_now(click_count: u8) -> bool {
    click_count <= 1
}

/// 外层长窗口重连的等待秒数：15s → 30s → 60s 封顶（原来是 15s×次数，越等越久）。
fn outer_reconnect_wait(fails: u32) -> u64 {
    match fails {
        0 | 1 => 15,
        2 => 30,
        _ => 60,
    }
}

/// 设备断开后在同一个 `cmd_run` 里原地重连：最多 3 次，退避 1/2/4 秒。
/// 成功返回新连接（调用方负责关掉旧句柄）；3 次都失败返回 None，交回外层长窗口。
async fn reconnect_in_place(target: Option<&str>) -> Option<Device> {
    for (i, wait) in RECONNECT_BACKOFF_SECS.iter().enumerate() {
        let attempt = i + 1;
        println!("重连第 {attempt} 次：设备断开，{wait} 秒后重试");
        tokio::time::sleep(Duration::from_secs(*wait)).await;
        match device::open(target).await {
            Ok(dev) => {
                println!("已重新连接（第 {attempt} 次尝试）");
                return Some(dev);
            }
            Err(e) => eprintln!("重连第 {attempt} 次失败：{e}"),
        }
    }
    None
}

async fn cmd_run(args: &Args) -> Result<()> {
    // 进会话前先把上一段（wait_for_task）欠下的节流写入落盘，否则下面读到的
    // 磁盘快照可能缺最近一次 announced/format 变化。
    let _ = flush_pending_store();
    let mut store = load_store();
    // 用户配置的唯一来源是 pet-config.json（插件面板与 CLI 都写它）。
    // state.json 里的 `model.settings` 只是快照，只在首次迁移时被读一次。
    let _ = apply_user_config(&mut store);
    if let Some(v) = args.get("volume").and_then(|v| v.parse::<u8>().ok()) {
        store.model.settings.volume = v.min(100);
    }
    if args.has("no-sound") {
        store.model.settings.sound_enabled = false;
    }
    let mut fmt = device::format(args.get("format").unwrap_or(&store.format))?;
    // 显式地址优先并记住（写 pet-config.json），避免每次都得带 --target。
    if let Some(t) = args.get("target").map(str::to_string) {
        if store.target.as_deref() != Some(t.as_str()) {
            write_pet_config_at(
                &pet_config_path(),
                &PetConfigPatch {
                    target: Some(t),
                    ..Default::default()
                },
            )?;
            let _ = apply_user_config(&mut store);
            save_store_now(&store)?;
        }
    }
    let target = args
        .get("target")
        .map(str::to_string)
        .or(store.target.clone());
    let mut model = store.model.clone();
    // 守护模式（standby）下跟随被手动关掉：别去占设备，直接回待命。
    // 判在 `device::open` 之前——否则会在用户说「先别上屏」之后还去抢一次连接。
    if args.has("standby") && !model.settings.follow_enabled {
        println!("跟随已关闭：不占用设备。");
        return Ok(());
    }
    let mut watcher = Watcher::new(dsh_root());
    let fonts = Fonts::load()?;
    // 花絮采样线程：进程级单例，这里只是确保它已经起来了（UIA 调用留在后台线程）。
    let quips = QuipSampler::global();

    let mut dev = device::open(target.as_deref()).await?;
    println!(
        "已连接: id={} model={} addr={}｜灰阶 {}｜Ctrl+C 退出",
        dev.summary.id,
        dev.summary.name,
        dev.summary.address,
        device::format_name(fmt)
    );
    let mut status_at = now();
    if let Ok(s) = dev.status().await {
        println!(
            "电量 {:?}% | RSSI {:?} dBm",
            s.battery_percent, s.wifi_rssi_dbm
        );
        model.battery = s.battery_percent;
        model.rssi = s.wifi_rssi_dbm.map(i16::from);
        model.battery_at = Some(now());
    }
    CONNECTED_AT.store(now(), Ordering::Relaxed);

    let watch = args.has("exit-with-dsh");
    // 待命模式（守护用）：任务结束就交还设备。手动 `run` 默认一直占着，便于调试。
    let standby = args.has("standby");
    // 默认状态行不带标题；`--verbose` 才附加，便于排查（可能含提示词首行）。
    let verbose = args.has("verbose");
    let grace = args
        .get("idle-grace")
        .and_then(|v| v.parse::<u32>().ok())
        .map(|v| v as i64)
        .unwrap_or_else(|| store.idle_grace());
    let mut last_sig: Option<u64> = None;
    // 设备正在录音时固件占着采集，推送和响铃都会拿到 Busy——这一拍直接跳过，
    // 等 RecordingFinished 再恢复，别让日志里刷一串「推送失败」。
    let mut recording = false;
    // `RecordingStarted` 的时刻：10 秒没等到 `RecordingFinished` 就自己复位，
    // 免得固件漏发通知后扫描/推送被永久跳过。
    let mut recording_started_at: Option<Instant> = None;
    // 设备手势：确认键「双击 = 静音/恢复」「三击 = 退出回普通界面」。
    // 固件只上报单键短按/长按，没有组合键，双击/三击在这里用时间窗识别：
    // 1 次=回首页（原有行为），2 次=静音，3 次=退出。双击会等 650ms 看有没有
    // 第三次，避免「想退出却先被静音」。
    let mut clicks: (i64, u8) = (0, 0); // (最后一次确认短按的毫秒时间戳, 连击数)
    // 1 秒一拍：状态变化先攒进冷却闸门，但到点要能在 1 秒内推出去，
    // 不能因为扫描间隔没到而拖成 4 秒。
    let mut scan_tick = tokio::time::interval(Duration::from_secs(1));
    scan_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    // 连上设备后先「等画面稳定」再推第一帧：刚连上时 DSH 的回合事件往往还在
    // 接连落盘，立刻推一帧、2 秒后又因为状态变了再推一帧，看起来就是连续闪。
    // 这里最多观察 2 秒，连续两次扫描状态一致就收手，只推最终那一帧。
    // 子智能体检测：父会话回合结束后，队友（subagent）可能还在跑。
    let mut team = TeamScan::default();
    let mut settle_prev: Option<CardSig> = None;
    // 设备刚从默认界面切过来时，固件自己要刷一次过渡画面；先让它落定，
    // 别在过渡动画中间再推一帧（会叠加成连闪）。
    tokio::time::sleep(Duration::from_millis(1200)).await;
    for _ in 0..4 {
        scan_and_report(&mut model, &mut watcher, &mut store, &dev, false, verbose).await;
        team.sync(&mut model, &dsh_root());
        let sig = card_sig(&model, now());
        let stable = settle_prev.as_ref() == Some(&sig);
        settle_prev = Some(sig);
        if stable {
            break;
        }
        tokio::time::sleep(Duration::from_millis(450)).await;
    }
    let mut last_push_at = now();
    if push_if_changed(&dev, &mut model, &mut store, quips, &fonts, fmt, &mut last_sig).await {
        last_push_at = now();
    }
    // 两次刷新之间的最小间隔：状态抖动（子任务开始/结束、多会话轮流上报）
    // 很容易在几秒内触发好几次刷新，墨水屏每次都整屏闪。冷却期内的变化先攒着，
    // 到点只推最后的状态——看到的是「一次闪」而不是「连闪」。
    const PUSH_COOLDOWN_SECS: i64 = 3;
    let mut push_not_before = last_push_at + PUSH_COOLDOWN_SECS;
    let mut pending_push = false;
    // 屏幕上现在是什么：节流判定用它区分「状态切换（必须立刻刷）」和
    // 「工具活动/进度这类细节（可以等 refresh_secs）」。
    let mut last_card = Some(card_sig(&model, now()));
    // 首扫静默是为了不把历史回放当成新提醒，但如果此刻屏幕上正停在「等你处理」，
    // 那是一次真正的漏报——设备超时重连、守护重启都会走到这里，等待页会无声出现。
    // 这里补一次提醒；同一提示只响一次（指纹落盘），所以反复重连不会反复响。
    // 完成态只在刚发生（1 分钟内）时补，避免重启后把几小时前的结果当新闻播一遍。
    if let Some(task) = model.current(now()) {
        let status = task.status();
        let fresh = now().saturating_sub(task.updated_at) <= 60;
        if status == Status::Waiting || (status == Status::Done && fresh) {
            announce(&dev, &model, &mut store, &task).await;
        }
    }

    // `--trace-tick`：只在开启时记录每拍耗时，每 10 拍打印一行；默认零开销。
    let mut trace = TickTrace::from_args(args);
    let outcome = loop {
        tokio::select! {
            _ = scan_tick.tick() => {
                let tick_started = Instant::now();
                // 上一拍在节流窗口里攒下的状态，到点就落盘（1 秒内最多写一次）。
                let _ = flush_pending_store();
                // DSH 插件/面板退出或被结束：子进程自己收尾，别留下孤儿占着设备。
                if args
                    .get("watch-pid")
                    .or_else(|| args.get("parent"))
                    .and_then(|v| v.parse::<u32>().ok())
                    .is_some_and(|p| !pid_alive(p))
                {
                    println!("DSH 已退出：释放设备，宠物退出。");
                    break Ok(());
                }
                if watch && !dsh_running_cached() {
                    println!("DeepSeek Harness 已退出：正常断开设备，回到待命。");
                    break Ok(());
                }
                // 录音期间设备在采集，任何推送都会 Busy：这一拍什么都不做。
                // 但固件的 `RecordingFinished` 可能丢（被打断、拔线、固件异常），
                // 不能因此永久停摆——超过 10 秒就自己复位，恢复扫描与推送。
                if recording && recording_expired(Instant::now(), recording_started_at) {
                    recording = false;
                    recording_started_at = None;
                    println!("[手势] 录音事件超时，已复位（10 秒内没有收到 RecordingFinished）");
                }
                if recording {
                    trace.record(tick_started, || scene_element_count(&model));
                    continue;
                }
                // 磁盘上的提醒指纹可能被别的进程（插件面板 / 守护父进程）刚更新：
                // 按时间取新，避免本进程把同一条等待再响一遍。缓存按 mtime+长度
                // 命中，文件没变时零解析。
                let disk = cached_load_store();
                if disk.announced_at > store.announced_at {
                    store.announced = disk.announced;
                    store.announced_at = disk.announced_at;
                }
                // 用户配置每拍重读一次（pet-config.json；mtime 没变时直接复用上次
                // 解析结果）：插件面板或 `dsh-pet set` 改完 2 秒内生效，不用重启守护。
                if apply_user_config(&mut store) {
                    model.settings = store.model.settings.clone();
                    if args.get("format").is_none() {
                        if let Ok(new_fmt) = device::format(&store.format) {
                            if device::format_name(new_fmt) != device::format_name(fmt) {
                                println!("[屏幕] 刷新格式切换为 {}（下一帧生效）", store.format);
                                fmt = new_fmt;
                            }
                        }
                    }
                    last_sig = None;
                    pending_push = true;
                    push_not_before = now();
                }
                // 待命页要显示设备电量与 Wi-Fi：每 5 分钟刷新一次设备状态，
                // 别让空闲页面永远停在连接那一刻的读数上。
                if now().saturating_sub(status_at) >= 300 {
                    status_at = now();
                    if let Ok(s) = dev.status().await {
                        model.battery = s.battery_percent;
                        model.rssi = s.wifi_rssi_dbm.map(i16::from);
                        model.battery_at = Some(now());
                    }
                }
                // 双击确认：两次短按后 650ms 内没有第三次点击，才落定「静音切换」。
                // 放在扫描 tick 里判断，省掉一条额外的定时分支；最坏晚 2 秒生效。
                if clicks.1 == 2
                    && chrono::Local::now().timestamp_millis() - clicks.0 >= 650
                {
                    clicks = (0, 0);
                    model.settings.sound_enabled = !model.settings.sound_enabled;
                    store.model = model.clone();
                    // 静音开关的真源是 pet-config.json：只改内存会被下一拍的
                    // 配置覆盖盖回去，所以这里写配置（state.json 只同步快照）。
                    let patch = PetConfigPatch {
                        sound: Some(model.settings.sound_enabled),
                        ..Default::default()
                    };
                    if let Err(e) = write_pet_config_at(&pet_config_path(), &patch) {
                        eprintln!("设置保存失败: {e}");
                    }
                    if let Err(e) = save_store_now(&store) {
                        eprintln!("设置保存失败: {e}");
                    }
                    model.quip = if model.settings.sound_enabled {
                        "提示音已开"
                    } else {
                        "已静音"
                    }
                    .into();
                    println!("[手势] 双击确认 → {}", model.quip);
                    // 屏幕右下角给个即时反馈（下一帧会被真实花絮替换）。
                    // 这是本次双击唯一一帧结果帧：第二下点击在按键分支里被推迟了。
                    let scene = layout::draw(&model, now());
                    if push_scene(&dev, scene, &fonts, fmt, &mut last_sig).await {
                        println!("[手势] 静音结果已上屏（双击判定结束，只推这一帧）");
                    }
                    last_card = Some(card_sig(&model, now()));
                    // 手势反馈已经闪了一次，主画面别紧跟着再闪：一并走冷却闸门。
                    last_push_at = now();
                    push_not_before = last_push_at + PUSH_COOLDOWN_SECS;
                    pending_push = false;
                }
                // 交还设备的判定放在扫描之前：判定用上一拍的状态，
                // grace 以秒计，差一拍不影响，却让「跟随已关闭」时也能正常释放。
                // 跟随被手动关掉时立刻释放：这是「暂时别上屏」的开关，不该等宽限期。
                let follow = model.settings.follow_enabled;
                if standby
                    && (!follow || (may_release(&model, now(), grace) && !session_writing(&model)))
                {
                    if follow {
                        println!("任务已结束（{grace} 秒内没有活跃任务）：释放设备，回到待命。");
                    } else {
                        println!("跟随已关闭：释放设备，回到待命（`dsh-pet set --follow on` 可恢复）。");
                    }
                    // 记下交还时刻：外层重新待命后，冷却期内不再立刻抢回来。
                    store.released_at = Some(now());
                    let _ = save_store_now(&store);
                    break Ok(());
                }
                if !model.settings.follow_enabled {
                    trace.record(tick_started, || scene_element_count(&model));
                    continue;
                }
                // 扫描到变化：先只标记「想刷新」，真正的推送统一走下面的冷却闸门。
                let changed =
                    scan_and_report(&mut model, &mut watcher, &mut store, &dev, true, verbose).await;
                // 子智能体状态也要每拍更新：父会话「已完成」但队友还在跑时，
                // 主卡应显示「队友在跑」而不是完成（DSH 侧边栏也是这么显示的）。
                if team.sync(&mut model, &dsh_root()) {
                    // 子智能体开始/结束：落盘，面板端也能马上看到。
                    store.model = model.clone();
                    let _ = save_store(&store);
                }
                if changed {
                    let at = now();
                    let sig = card_sig(&model, at);
                    if refresh_due(
                        last_card.as_ref(),
                        &sig,
                        last_push_at,
                        at,
                        model.settings.refresh_secs as i64,
                    ) {
                        pending_push = true;
                    }
                }
                // 冷却期内攒着不推；到点一次性推当前最新状态，避免连续整屏闪。
                // 双击判定窗口还开着时也不推：中间那一帧会让一次双击闪 3 屏，
                // 等 650ms 判定完，上面已经按最终状态推过静音结果了。
                let double_click_pending = clicks.1 == 2
                    && chrono::Local::now().timestamp_millis() - clicks.0 < 650;
                if pending_push && !double_click_pending && now() >= push_not_before {
                    if push_if_changed(
                        &dev, &mut model, &mut store, quips, &fonts, fmt, &mut last_sig,
                    )
                    .await
                    {
                        last_push_at = now();
                        push_not_before = last_push_at + PUSH_COOLDOWN_SECS;
                        last_card = Some(card_sig(&model, now()));
                        pending_push = false;
                    }
                }
                trace.record(tick_started, || scene_element_count(&model));
            }
            event = dev.next_event() => {
                match event {
                    Ok(Some(zectrix_sdk::Event::Key { key, action })) => {
                        println!("[按键] {key:?} {action:?}");
                        // 确认键连击：1 次=回首页，2 次=静音切换，3 次=退出回普通界面。
                        // 双击不立刻执行：等 650ms 确认没有第三次点击，才在下一拍落定
                        // （见扫描 tick 里的 pending_double_click），避免想退出时先被静音。
                        if matches!(
                            (key, action),
                            (zectrix_sdk::Key::Confirm, zectrix_sdk::KeyAction::Short)
                        ) {
                            let at = chrono::Local::now().timestamp_millis();
                            let count = if clicks.1 > 0 && at - clicks.0 <= 700 {
                                clicks.1 + 1
                            } else {
                                1
                            };
                            clicks = (at, count);
                            if count >= 3 {
                                // 三击：退出到普通界面。停跟随 → 守护回到待命、释放设备；
                                // 要再回来，在插件设置页把「跟随」打开即可。
                                model.settings.follow_enabled = false;
                                store.model = model.clone();
                                store.released_at = Some(now());
                                // 跟随是用户配置：写进 pet-config.json，
                                // 别让守护下一拍读配置时又把 follow 改回开。
                                let patch = PetConfigPatch {
                                    follow: Some(false),
                                    ..Default::default()
                                };
                                if let Err(e) =
                                    write_pet_config_at(&pet_config_path(), &patch)
                                {
                                    eprintln!("设置保存失败: {e}");
                                }
                                if let Err(e) = save_store_now(&store) {
                                    eprintln!("设置保存失败: {e}");
                                }
                                println!(
                                    "[手势] 三击确认 → 退出到普通界面（跟随已暂停，控制面板里可恢复）"
                                );
                                break Ok(());
                            }
                        }
                        // 允许的手势：上下短按翻任务/翻页，确认单击回首页、
                        // 双击静音、三击退出；长按按规格不做动作。
                        let handled = match (key, action) {
                            // 上下键只在任务之间翻页/选择；选项交互已取消
                            //（设备无法替你点电脑上的选项，等待时只提示回电脑）。
                            (zectrix_sdk::Key::Up, zectrix_sdk::KeyAction::Short) => {
                                model.cycle(-1, now());
                                true
                            }
                            (zectrix_sdk::Key::Down, zectrix_sdk::KeyAction::Short) => {
                                model.cycle(1, now());
                                true
                            }
                            // 确认 = 回到首页并恢复自动跟随；不批准任何操作。
                            (zectrix_sdk::Key::Confirm, zectrix_sdk::KeyAction::Short) => {
                                model.selected = None;
                                true
                            }
                            _ => false,
                        };
                        // 双击的第二下不单独推帧：静音结果等 650ms 判定窗口结束后
                        // 由扫描 tick 统一推一帧。否则一次双击会连闪 3 屏
                        // （单击回首页、第二下、静音结果）。
                        let defer_confirm = matches!(
                            (key, action),
                            (zectrix_sdk::Key::Confirm, zectrix_sdk::KeyAction::Short)
                        ) && !gesture_pushes_now(clicks.1);
                        if defer_confirm {
                            println!(
                                "[手势] 双击窗口内的第 {} 次点击：不单独推帧（等判定结束只推静音结果）",
                                clicks.1
                            );
                        }
                        if handled
                            && !defer_confirm
                            && push_if_changed(
                                &dev,
                                &mut model,
                                &mut store,
                                quips,
                                &fonts,
                                fmt,
                                &mut last_sig,
                            )
                            .await
                        {
                            last_push_at = now();
                            last_card = Some(card_sig(&model, now()));
                        }
                    }
                    Ok(Some(zectrix_sdk::Event::RecordingStarted)) => {
                        // 新版规格：长按确认键不做动作。固件仍会上报录音事件，
                        // 这里只标记「设备正忙」，不转写、不粘贴、不推送。
                        recording = true;
                        recording_started_at = Some(Instant::now());
                        println!("[手势] 长按确认键：按规格无操作（不录音、不投递）");
                    }
                    Ok(Some(zectrix_sdk::Event::RecordingFinished { audio, reason })) => {
                        // 长按无操作：录音数据直接丢弃，设备回到主画面。
                        recording = false;
                        recording_started_at = None;
                        if let Some(audio) = audio {
                            println!(
                                "[手势] 忽略长按录音（{} 秒，{reason:?}）",
                                audio.samples.len() as f32 / 16_000.0
                            );
                        }
                        last_sig = None;
                    }
                    Ok(Some(zectrix_sdk::Event::Disconnected { reason })) => {
                        // 断线先原地重连：1s/2s/4s 共 3 次，成功就留在这一轮会话里；
                        // 3 次都失败才返回外层，由 `cmd_watch` 用 15s/30s/60s 的长窗口兜底。
                        println!("[连接] 设备连接已结束（{reason:?}）");
                        // 断线是持久化边界：节流窗口里攒着的状态立刻落盘，
                        // 免得接下来的重连/退出把最后一笔改动丢了。
                        let _ = flush_pending_store();
                        if watch && !dsh_running_cached() {
                            println!("DeepSeek Harness 已退出：不再重连。");
                            break Ok(());
                        }
                        let Some(new_dev) = reconnect_in_place(target.as_deref()).await else {
                            break Err(anyhow::anyhow!(
                                "设备连接已结束（{reason:?}），原地重连 3 次失败"
                            ));
                        };
                        let _ = dev.close().await;
                        dev = new_dev;
                        // 新连接的事件流从零开始：录音标记、连击、画面签名都复位，
                        // 否则旧签名可能把首帧拦住（画面没变就不推）。
                        recording = false;
                        recording_started_at = None;
                        clicks = (0, 0);
                        last_sig = None;
                        pending_push = false;
                        CONNECTED_AT.store(now(), Ordering::Relaxed);
                        status_at = now();
                        // 重连成功先推首帧（≤2 秒目标），再取电量——别让状态查询拖慢首帧。
                        if push_if_changed(
                            &dev, &mut model, &mut store, quips, &fonts, fmt, &mut last_sig,
                        )
                        .await
                        {
                            last_push_at = now();
                            last_card = Some(card_sig(&model, now()));
                            push_not_before = last_push_at + PUSH_COOLDOWN_SECS;
                        }
                        if let Ok(s) = dev.status().await {
                            println!(
                                "电量 {:?}% | RSSI {:?} dBm",
                                s.battery_percent, s.wifi_rssi_dbm
                            );
                            model.battery = s.battery_percent;
                            model.rssi = s.wifi_rssi_dbm.map(i16::from);
                            model.battery_at = Some(now());
                        }
                        continue;
                    }
                    Ok(None) => break Err(anyhow::anyhow!("连接已终止（事件流关闭），交给外层重连")),
                    Err(e) => break Err(anyhow::anyhow!("事件通道结束: {e}")),
                }
            }
            _ = tokio::signal::ctrl_c() => break Ok(()),
        }
    };

    if let Some(summary) = trace.summary() {
        println!("{summary}");
    }
    let closed = dev.close().await;
    store.model = model;
    let _ = save_store_now(&store);
    println!("已断开并保存状态。");
    closed?;
    outcome
}

/// 待命：只读会话文件，不连设备，等到出现活跃任务（正在工作/等你处理）为止。
/// 返回 `true` = 该去占用设备了；`false` = DeepSeek Harness 已退出，回外层重新等。
/// 这里刻意不响铃、不推屏：设备还没连上，也还不该被占用。
async fn wait_for_task(
    store: &mut Store,
    watcher: &mut Watcher,
    parent: Option<u32>,
    trace: &mut TickTrace,
) -> Result<bool> {
    let mut tick = tokio::time::interval(Duration::from_secs(2));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // 待命阶段也要看「子智能体是否还在跑」：父会话可能早已 turn/end，
    // 但只要队友还在跑，就该占回设备显示「队友在跑」。
    let mut team = TeamScan::default();
    loop {
        tick.tick().await;
        let tick_started = Instant::now();
        let _ = flush_pending_store();
        if parent.is_some_and(|p| !pid_alive(p)) {
            println!("控制面板已退出：结束待命。");
            return Ok(false);
        }
        // 待命阶段也要读用户配置：`dsh-pet set --follow off` 之后不能再用启动时的
        // 旧快照判断「要不要占设备」，否则刚释放又立刻被拉起来。设备地址与屏幕
        // 格式也在这一个入口里同步（配置变了才回写 state.json）。
        if apply_user_config(store) {
            let _ = save_store(store);
        }
        // 外层手上这份 store 是启动时的快照：上一轮会话在 `cmd_run` 里写下的
        // `released_at` 只有从磁盘取回才知道，否则冷却期内会立刻把设备抢回来。
        // 命中磁盘缓存时零解析；文件没被改过就只克隆一次内存快照。
        let disk = cached_load_store();
        if let Some(released) = disk.released_at {
            if store.released_at.is_none_or(|mine| released > mine) {
                store.released_at = Some(released);
            }
        }
        if store.idle_grace.is_none() {
            store.idle_grace = disk.idle_grace;
        }
        if !dsh_running_cached() {
            println!("DeepSeek Harness 已退出：结束待命。");
            return Ok(false);
        }
        if !store.model.settings.follow_enabled {
            trace.record(tick_started, || scene_element_count(&store.model));
            continue;
        }
        let (events, changed) = scan_into(&mut store.model, watcher);
        if changed {
            if let Err(e) = save_store(store) {
                eprintln!("配置保存失败: {e}");
            }
        }
        // 队友还在跑：即使父会话没有新记录，也要算「有活跃工作」。
        if team.sync(&mut store.model, &dsh_root()) {
            let _ = save_store(store);
        }
        if !store.model.team_busy.is_empty() {
            println!("检测到子智能体仍在运行：准备占用设备。");
        }
        for ev in events {
            println!(
                "[状态] {} · {} → {}{}",
                ev.title,
                ev.task_id,
                ev.status.label(),
                ev.progress
                    .map(|p| format!("（清单 {p}%）"))
                    .unwrap_or_default()
            );
        }
        // 与释放判定同源：任务在跑、或上一轮刚结束还在宽限期内才占用设备。
        // 只看 running 会把「等你处理」也算进来，释放后立刻又被重占，
        // 变成「连上→等 5 分钟→释放→再连上」的循环。
        if should_hold(&store.model, now(), store.idle_grace()) {
            // 刚交还过设备：只有「确实又有新记录」才重新占用。上一轮的宽限期
            // 可能还剩着，光看 should_hold 会在冷却一过就再连一次、再放一次。
            // 另外设备释放租约要 8 秒左右，抢太早只会拿到「设备忙碌」。
            let ready = store
                .released_at
                .is_none_or(|r| now().saturating_sub(r) >= RELEASE_LEASE);
            let fresh = store.released_at.is_none_or(|r| {
                // 有新记录，或者此刻正有人在等你处理（等待态不会再写日志，
                // 但设备就是那条通知的出口，必须重新占上）。
                store.model.tasks.iter().any(|t| t.updated_at > r)
                    || store.model.needs_action(now())
            });
            let working = store
                .model
                .running(now())
                .iter()
                .any(|t| t.status() == Status::Working);
            if ready && (working || fresh) {
                // 马上要进 cmd_run：把欠下的节流写入落盘，避免它读到旧快照。
                let _ = flush_pending_store();
                return Ok(true);
            }
        }
        trace.record(tick_started, || scene_element_count(&store.model));
    }
}

/// 守护：DeepSeek Harness 一启动就进入待命（只读会话、不占设备），一有任务才开始占用
/// 设备推送；任务全部结束并静默 `idle_grace` 秒就交还设备，回到待命。
/// 设备休眠/空闲会主动断开（Timeout），这类断线按 15 秒步长退避重连，
/// 连续失败 5 次才停（避免反复抢占设备）。ResultUnknown 不重发、不抢连接——
/// 这条 SDK 约定只约束「同一次显示请求」，不影响断线后的重新建连。
/// 把 SDK 的短错误翻成「现在该做什么」。守护日志是排错第一现场，
/// 只写「没有发现可用设备」会让人以为是 IP 或网线的问题——实际最常见的是
/// 设备空闲后断网休眠（固件 3.7.5 起尤其明显），设备端不开「联网保持」就唤不醒。
/// 只对这两类给建议：别的错误（协议、驱动）另有原因，别误导。
fn device_hint(e: &anyhow::Error) -> Option<&'static str> {
    match e.downcast_ref::<zectrix_sdk::Error>() {
        Some(zectrix_sdk::Error::NoDevice) => Some(
            "设备不在线。IP 没变却连不上，通常是设备空闲后断网休眠：在设备「设置 → 网络同步」打开「联网保持」，或在设备上打开「连接管理端」。",
        ),
        Some(zectrix_sdk::Error::Busy) => {
            Some("设备正被占用（另一份程序或设备端正在操作），约 8 秒释放，稍后自动重试。")
        }
        _ => None,
    }
}

/// 最近 `WRITING_WINDOW` 秒内还有**正在跟随的**会话日志在增长：说明那边的活没停。
/// 屏幕上的状态可能没变（同一轮里持续在跑），但这时候把设备交还，下一次扫描
/// 又会把它抢回来——先等它停笔。只认模型里跟踪的会话，已归档/子代理等不上屏的
/// 文件不能拿来续命，否则设备会被它们永久占住。
fn session_writing(model: &Model) -> bool {
    let tracked: std::collections::HashSet<&str> = model
        .tasks
        .iter()
        .filter(|t| t.source == "dsh")
        .filter_map(|t| t.event.thread_id.as_deref())
        .collect();
    if tracked.is_empty() {
        return false;
    }
    let Ok(projects) = std::fs::read_dir(dsh_root()) else {
        return false;
    };
    let deadline = now() - WRITING_WINDOW;
    for project in projects.flatten() {
        let proj_dir = project.path();
        if !proj_dir.is_dir() {
            continue;
        }
        let Ok(sessions) = std::fs::read_dir(&proj_dir) else {
            continue;
        };
        for session in sessions.flatten() {
            let session_dir = session.path();
            let dir_name = session_dir
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("");
            if !tracked.contains(dir_name) {
                continue;
            }
            let Ok(logs) = std::fs::read_dir(&session_dir) else {
                continue;
            };
            for log in logs.flatten() {
                let name = log.file_name();
                let name = name.to_string_lossy();
                // 会话日志：`session.v4.jsonl.zstd`（跨版本迁移会并存旧 .vN，取到哪份都算）。
                if !name.starts_with("session") || !name.ends_with(".jsonl.zstd") {
                    continue;
                }
                let Ok(modified) = log.metadata().and_then(|m| m.modified()) else {
                    continue;
                };
                let secs = modified
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0);
                if secs >= deadline {
                    return true;
                }
            }
        }
    }
    false
}

async fn cmd_watch(args: &Args) -> Result<()> {
    // 单实例：watch.cmd 会在 exe 刚退出时立刻重启，若另一份守护已经在跑，
    // 这里退出——不打印会让「为什么没动静」无从查起，所以留一行带 PID 的痕迹。
    if !claim_instance() {
        println!(
            "[{}] 已有守护在跑，本进程（pid {}）退出",
            chrono::Local::now().format("%H:%M:%S"),
            std::process::id()
        );
        return Ok(());
    }
    println!(
        "[{}] 守护进程启动（pid {}）",
        chrono::Local::now().format("%H:%M:%S"),
        std::process::id()
    );
    // 先落一次心跳，状态查询立刻能看到「守护在跑」，不用等第一个 tick。
    write_heartbeat();
    // 花絮采样线程：UIA 调用留在后台（每 2 秒最多一次），推送路径只读缓存。
    // 放在待命之前启动，进程一起来就先采一句。
    QuipSampler::global();

    // 显式给了地址就记住（写 pet-config.json）：下次 `watch` / `run` / `push`
    // 不带参数也能连上。用户配置只认这一份文件。
    let mut store = load_store();
    let _ = apply_user_config(&mut store);
    if let Some(t) = args.get("target").map(str::to_string) {
        if store.target.as_deref() != Some(t.as_str()) {
            write_pet_config_at(
                &pet_config_path(),
                &PetConfigPatch {
                    target: Some(t),
                    ..Default::default()
                },
            )?;
            let _ = apply_user_config(&mut store);
            save_store_now(&store)?;
        }
    }
    // 显式给的宽限要记下来（`Some` 才代表用户设过），下次不带参数也照旧。
    if let Some(g) = args.get("idle-grace").and_then(|v| v.parse::<u32>().ok()) {
        if store.idle_grace != Some(g) {
            store.idle_grace = Some(g);
            save_store_now(&store)?;
        }
    }
    let target = args
        .get("target")
        .map(str::to_string)
        .or_else(|| store.target.clone());
    let grace = store.idle_grace();
    println!(
        "守护模式已启动：目标 {}；有任务才占用设备，{grace} 秒无活跃任务即交还。",
        target.as_deref().unwrap_or("自动发现")
    );
    // 心跳：5 秒一次，供状态判断「宠物是否在跑」。写失败（磁盘满、权限）
    // 只影响这个判断，不影响推送。
    tokio::spawn(async {
        let mut tick = tokio::time::interval(Duration::from_secs(5));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            write_heartbeat();
        }
    });
    // 旧版本（面板/托盘/登录自启）的残留只在加载阶段清一次，幂等；0 个时保持安静。
    let retired = retire_legacy_autostart();
    if retired > 0 {
        println!("已清理旧版启动项 {retired} 个");
    }
    let mut watcher = Watcher::new(dsh_root());
    // DSH 插件 spawn 时传 `--watch-pid <DSH Host pid>`；旧面板形态传 `--parent`。
    // 父进程消失就自退（DSH 崩溃/被强杀时 dispose 不会执行，只能靠这条兜底）。
    let parent = args
        .get("watch-pid")
        .or_else(|| args.get("parent"))
        .and_then(|v| v.parse::<u32>().ok());
    // `--trace-tick` 在 watch 里也生效：待命 tick 与之后每次会话（session 继承
    // 同一批 flags）都会采样；关闭时 record 是空操作。
    let mut trace = TickTrace::from_args(args);
    loop {
        if let Some(p) = parent {
            if !pid_alive(p) {
                println!("DSH 已退出：宠物收尾，释放设备。");
                return Ok(());
            }
        }
        if !dsh_running_cached() {
            tokio::time::sleep(Duration::from_secs(5)).await;
            continue;
        }
        println!("检测到 DeepSeek Harness，进入待命（只读会话，不占用设备）。");
        if !wait_for_task(&mut store, &mut watcher, parent, &mut trace).await? {
            tokio::time::sleep(Duration::from_secs(5)).await;
            continue;
        }
        println!("检测到任务，开始占用设备。");
        let mut session = Args {
            cmd: "run".into(),
            flags: args.flags.clone(),
        };
        session
            .flags
            .push(("exit-with-dsh".into(), "true".into()));
        session.flags.push(("standby".into(), "true".into()));
        let mut fails = 0u32;
        let mut exhausted = false;
        loop {
            match cmd_run(&session).await {
                // 任务结束交还设备、或 DeepSeek Harness 退出：都回外层重新待命。
                Ok(()) => break,
                Err(e) => {
                    eprintln!("本次会话异常结束：{e}");
                    // 每次会话只提示一次，别让 15/30/45 秒的重连把日志刷满。
                    if fails == 0 {
                        if let Some(hint) = device_hint(&e) {
                            eprintln!("　{hint}");
                        }
                    }
                    // 拿不到设备时别硬抢：已经没有活跃任务就把设备让出来。
                    // 走缓存：这里每 15/30/60 秒才跑一次，但文件没变时不必重解析。
                    let disk = cached_load_store();
                    if !should_hold(&disk.model, now(), grace) {
                        println!("已无活跃任务，不再重连。");
                        break;
                    }
                    fails += 1;
                    if fails > 5 {
                        eprintln!("连续失败 {fails} 次，暂停重连，等下一个任务再试。");
                        exhausted = true;
                        break;
                    }
                    if !dsh_running_cached() {
                        break;
                    }
                    // 外层兜底：15s → 30s → 60s 封顶。原地重连已经试过 3 次，
                    // 这里再拖长只会让设备回来得更慢，所以不再线性增长。
                    let wait = outer_reconnect_wait(fails);
                    println!("{wait} 秒后重连（第 {fails} 次）");
                    tokio::time::sleep(Duration::from_secs(wait)).await;
                }
            }
        }
        if exhausted {
            // 设备可能正在休眠或被人占用：先冷一会儿再回待命，否则
            // 「待命→发现有任务→重连→失败」会立刻转成高频循环。
            tokio::time::sleep(Duration::from_secs(60)).await;
        }
    }
}

async fn cmd_sound(args: &Args) -> Result<()> {
    let mut store = load_store();
    // 音量默认值来自 pet-config.json（其次 Rust 默认 90），不从 state.json 快照取。
    let _ = apply_user_config(&mut store);
    let status = args
        .get("status")
        .map(parse_status)
        .transpose()?
        .flatten()
        .context("需要 --status waiting|done")?;
    let volume = args
        .get("volume")
        .and_then(|v| v.parse::<u8>().ok())
        .unwrap_or(store.model.settings.volume)
        .min(100);
    let target = args.get("target").map(str::to_string).or(store.target);
    let dev = device::open(target.as_deref()).await?;
    println!(
        "已连接: id={} addr={}｜播放 {} 提示音（音量 {volume}%）",
        dev.summary.id,
        dev.summary.address,
        status.label()
    );
    // 实测连接后立刻发音频会被设备以结果码 4 拒绝；留出音频通路初始化时间。
    let settle = args
        .get("settle-ms")
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(1500);
    tokio::time::sleep(Duration::from_millis(settle)).await;
    let result = dev.play(&device::clip(&sound::pcm(status)), volume).await;
    let closed = dev.close().await;
    match result {
        Ok(end) => println!("播放结束: {end:?}"),
        Err(e) => return Err(anyhow::anyhow!("播放失败: {e}")),
    }
    closed?;
    Ok(())
}

fn usage() {
    println!(
        r#"鳄鱼大宠物 · ZECTRIX Note 4 墨水屏（DeepSeek Harness 插件）

  DSH 启动时会自动拉起宠物（`watch`），退出时自动收尾；一般不需要手动运行。
  设置：DeepSeek Harness → 设置 → Plugins → 鳄鱼大宠物（跟随/模式/声音/设备 IP/格式）。
  所有设置只写一个文件：pet-config.json（插件面板与 `dsh-pet set` 都改它）；
  state.json 只存运行时状态（任务、提醒指纹、电量、设备占用与交还时间）。

诊断命令：
  dsh-pet preview [--demo <状态>] [--out 路径] [--session <ID>]
                                           渲染 400×300 预览，不连设备
  dsh-pet scene   [--demo <状态>]          打印布局 JSON（调试用）
  dsh-pet sessions                         列出可跟随的本机会话、状态与清单进度
  dsh-pet set     [--sound on|off] [--volume 0-100] [--follow on|off]
                  [--refresh 秒] [--calm on|off] [--format mono|gray4|gray16]
                  [--title-source session|prompt]
                                           查看/修改用户配置（写 pet-config.json，2 秒内生效）
  dsh-pet check   [--target IP|ID]         只核对设备，不推画面
  dsh-pet sound   --status waiting|done [--target IP|ID] [--volume 0-100]
                                           只播放一次提示音
  dsh-pet push    [--demo <状态>] [--target IP|ID] [--format mute]
                                           单次推送后立即断开
  dsh-pet watch   [--watch-pid PID] [--idle-grace 秒] [--verbose] [--trace-tick]
                                           守护：有任务才占用设备，任务结束 5 分钟无新任务即交还
                                           --trace-tick：每 10 拍打印 [tick] 耗时与元素数（默认关闭）
  dsh-pet where                            显示数据与配置路径（配置 = pet-config.json）
  dsh-pet forget                           删除本机状态缓存
  dsh-pet forget --titles                  只清理隐私：已有任务标题换成 session-短号，清空提醒指纹

状态：working / waiting / done / error / stopped / unknown / idle
设备需与电脑同一局域网，并在设备上开启「设置 → 网络同步 → 待机保持 Wi-Fi」。
"#
    );
}

/// `forget --titles`：只清理隐私，不删整份状态。
///
/// 把 state.json 里每条任务的 title 换成 `session-<短号>`（没有会话 id 的用
/// `task-<序号>`），并清空 announced 指纹。旧版本曾把提示词首行当标题写进
/// state.json，升级后这些历史值不会自己消失，需要这条命令手动抹一次。
fn cmd_forget_titles() -> Result<()> {
    let path = state_path();
    if !path.exists() {
        println!("没有已保存的状态。");
        return Ok(());
    }
    let mut store = load_store_at(&path);
    let replaced = scrub_task_titles(&mut store.model);
    let fingerprints = store.announced.len();
    store.announced.clear();
    store.announced_at = now().max(1);
    save_store_at(&path, &store)?;
    if store.degraded {
        println!(
            "[清理] 状态文件曾损坏：已写 {}，原 state.json 保持不变。",
            sibling_json(&path, "recovered").display()
        );
    }
    println!(
        "[清理] 标题已替换 {replaced} 条（无会话 id 用 task-<序号>），清空提醒指纹 {fingerprints} 条：{}",
        path.display()
    );
    println!("提示：pet.log / 设备上的旧画面属于历史输出，不会被这条命令改写。");
    Ok(())
}

fn main() {
    let args = Args::parse(std::env::args().skip(1).collect());
    // `--version` / `-V`：只打版本号，别掉进 usage 分支。
    if args.cmd == "version" || args.cmd == "-V" || args.has("version") {
        println!("dsh-pet {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    let runtime = tokio::runtime::Runtime::new().expect("创建 tokio 运行时失败");
    if let Err(e) = runtime.block_on(async_main(args)) {
        eprintln!("错误: {e}");
        std::process::exit(1);
    }
}

async fn async_main(args: Args) -> Result<()> {
    let result = dispatch(args).await;
    // 节流窗口里可能还攒着最后一笔保存（例如 `set` 之前的一次自动同步）：
    // 进程退出前统一落盘，保证「命令返回 = 已经写进 state.json」。
    let _ = flush_pending_store();
    result
}

async fn dispatch(args: Args) -> Result<()> {
    let cmd = args.cmd.clone();
    let result = match cmd.as_str() {
        "preview" | "scene" => {
            if cmd == "scene" {
                let mut a = args;
                a.flags.push(("scene".into(), "true".into()));
                cmd_preview(&a).await
            } else {
                cmd_preview(&args).await
            }
        }
        "sessions" | "ls" => cmd_sessions(&args).await,
        "set" | "settings" | "config" => cmd_set(&args),
        "init" | "setup" | "guide" => cmd_init(&args).await,
        "check" => cmd_check(&args).await,
        "sound" | "tone" => cmd_sound(&args).await,
        "uia" | "tree" => {
            // 诊断用：把 DeepSeek Harness 窗口的可访问性树打出来，定位输入框与发送按钮。
            let nodes = uia::dump_nodes();
            println!("共 {} 个节点（只列可聚焦 / 可编辑 / 按钮）", nodes.len());
            for (i, n) in nodes.iter().enumerate() {
                if !(n.focusable
                    || n.patterns.contains("value")
                    || n.patterns.contains("text")
                    || matches!(n.control.as_str(), "Button" | "Edit" | "Document")
                    // 诊断：确认卡的文案可能落在别的控件类型上，也要打出来。
                    || n.name.contains("允许")
                    || n.name.contains("拒绝")
                    || n.name.contains("询问规则"))
                {
                    continue;
                }
                let name: String = n.name.chars().take(30).collect();
                println!(
                    "{i:>4} {:<9} focus={:<5} {:<10} class={:<18} name={name}",
                    n.control,
                    n.focusable,
                    n.patterns,
                    n.class.chars().take(18).collect::<String>()
                );
            }
            Ok(())
        }
        "quip" => {
            // 诊断用：看一眼此刻从 DeepSeek Harness 界面读到的状态是什么。
            match uia::current_quip() {
                Some(quip) => println!("界面当前状态：{quip}"),
                None => {
                    // 读不到时把候选节点摊出来，方便判断是「树没长出来」还是
                    // 「界面确实没有状态行」（比如空闲）。
                    let nodes = uia::dump_nodes();
                    println!("没读到状态行（节点 {} 个）", nodes.len());
                    for n in nodes.iter().filter(|n| {
                        n.name.contains("正在") || n.name.contains("深度求索中")
                    }) {
                        println!("  候选 {} class={} name={}", n.control, n.class, n.name);
                    }
                    if let Some(q) = uia::pick_from_nodes(&nodes) {
                        println!("  按诊断节点重建后应读到：{q}");
                    }
                }
            }
            Ok(())
        }
        "push" | "once" => cmd_push(&args).await,
        "run" | "start" => cmd_run(&args).await,
        "watch" | "daemon" => cmd_watch(&args).await,
        "where" => {
            println!("配置: {}（唯一用户配置源）", pet_config_path().display());
            println!("状态缓存: {}（运行时状态，不是设置）", state_path().display());
            println!("输出: {}", out_dir().display());
            println!("会话: {}", dsh_root().display());
            Ok(())
        }
        "forget" => {
            if args.has("titles") {
                return cmd_forget_titles();
            }
            let path = state_path();
            match std::fs::remove_file(&path) {
                Ok(()) => println!("已删除 {}", path.display()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    println!("没有已保存的状态。")
                }
                Err(e) => eprintln!("删除失败: {e}"),
            }
            Ok(())
        }
        _ => {
            usage();
            Ok(())
        }
    };
    if let Err(e) = result {
        return Err(e);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_pet::model::ProgressSource;

    fn event(seq: u64, status: Status) -> TaskEvent {
        TaskEvent {
            task_id: "t1".into(),
            thread_id: None,
            seq,
            status,
            title: "真实任务".into(),
            detail: String::new(),
            progress: None,
            progress_source: ProgressSource::Agent,
            progress_steps: None,
            agent: "DeepSeek Harness".into(),
        }
    }

    fn model_with(seq: u64, status: Status, at: i64) -> Model {
        let mut m = Model::default();
        m.settings.follow_enabled = true;
        m.apply(event(seq, status), "dsh", at).unwrap();
        m
    }

    #[test]
    fn releases_only_after_the_grace_period_with_no_active_task() {
        // 任务在跑：不管多久都占着。
        let m = model_with(1, Status::Working, 5_000);
        assert!(!may_release(&m, 5_000, 60));
        assert!(!may_release(&m, 5_300, 60));
        // 任务完成：从完成那一刻起算，满 60 秒才交还。
        let m = model_with(1, Status::Done, 200);
        assert!(!may_release(&m, 200, 60));
        assert!(!may_release(&m, 259, 60));
        assert!(may_release(&m, 260, 60));
    }

    #[test]
    fn a_new_task_cancels_a_pending_release() {
        let mut m = model_with(1, Status::Done, 200);
        assert!(!may_release(&m, 230, 60));
        // 宽限期内用户又发了一条：继续占着，不必断开再重连。
        m.apply(event(2, Status::Working), "dsh", 230)
            .unwrap();
        assert!(!may_release(&m, 231, 60));
        assert!(!may_release(&m, 400, 60));
        // 再次结束后重新计时。
        m.apply(event(3, Status::Done), "dsh", 410).unwrap();
        assert!(!may_release(&m, 410, 60));
        assert!(may_release(&m, 470, 60));
    }

    #[test]
    fn a_pending_confirmation_keeps_the_device_until_it_is_answered() {
        // 「等你确认 / 等你回答」不是「一轮答完了」——它就是要人回电脑，
        // 而设备正是这条通知唯一的出口。到点撒手会让确认卡和连接一起消失，
        // 紧接着新记录又把它拉回来（实测 20 分钟抖了 13 轮）。
        let m = model_with(1, Status::Waiting, 100);
        assert!(should_hold(&m, 400, 300));
        assert!(!may_release(&m, 5_000, 300));
        // 超过 15 分钟也不算「状态待确认」：等待不会自己往前走，但依然有效。
        assert_eq!(
            m.current(5_000).map(|t| t.status()),
            Some(Status::Waiting),
            "等待确认的任务不能被陈旧判定降级"
        );

        // 确认处理完、这一轮结束：从结束那一刻起算，满 grace 才交还。
        let mut done = m.clone();
        done.apply(event(2, Status::Done), "dsh", 5_000)
            .unwrap();
        assert!(!may_release(&done, 5_299, 300));
        assert!(may_release(&done, 5_300, 300));

        // 还在跑的任务不受宽限影响：一直占着（静默超过 15 分钟才算状态待确认）。
        let m = model_with(1, Status::Working, 100);
        assert!(!may_release(&m, 1_000, 60));
        assert!(may_release(&m, 1_100, 60));
    }

    #[test]
    fn zero_or_absent_grace_releases_immediately() {
        let m = model_with(1, Status::Done, 100);
        assert!(may_release(&m, 100, 0));
        // 负值按 0 处理，不会卡住不交还。
        assert!(may_release(&m, 100, -5));
        // 一个任务都没有（手动 run --standby）：直接交还。
        let m = Model::default();
        assert!(may_release(&m, 100, 60));
    }

    #[test]
    fn default_grace_is_five_minutes_after_the_last_task() {
        // 规则：任务完成后 5 分钟内没有新任务下达就解除占用。
        let store = Store::default();
        assert_eq!(store.idle_grace(), 300);
        let m = model_with(1, Status::Done, 1_000);
        assert!(!may_release(&m, 1_000, store.idle_grace()));
        assert!(!may_release(&m, 1_299, store.idle_grace()));
        assert!(may_release(&m, 1_300, store.idle_grace()));
    }

    #[test]
    fn an_explicit_grace_is_not_mistaken_for_the_default() {
        // 显式设过（Some）就必须以它为准，不能被「等于默认值」误判成没设过，
        // 否则写回 state.json 时会被磁盘旧值盖回去。
        let mut store = Store {
            idle_grace: Some(90),
            ..Store::default()
        };
        assert_eq!(store.idle_grace(), 90);
        // 设成和默认一样的 300 也要能分辨：Some(300) 不等于 None。
        store.idle_grace = Some(300);
        assert!(store.idle_grace.is_some());
    }

    #[test]
    fn heartbeat_parses_pid_and_time() {
        assert_eq!(parse_heartbeat("4242\n1700000000\n"), Some((4242, 1_700_000_000)));
        // 只有一行、或不是数字，都算无效——宁可当成「没有守护」，也不能误判成活着。
        assert_eq!(parse_heartbeat("4242\n"), None);
        assert_eq!(parse_heartbeat(""), None);
        assert_eq!(parse_heartbeat("abc\ndef\n"), None);
    }

    /// T4：旧版启动项只清已知的 5 个文件；别的桌面文件不碰；缺文件时不报错，
    /// 第二次调用返回 0（幂等）。基目录直接传临时目录，不改进程环境变量。
    #[test]
    fn retire_legacy_autostart_removes_only_known_artifacts() {
        let dir = TempDir::new("retire");
        let appdata = dir.path().join("appdata");
        let profile = dir.path().join("profile");
        let programs = appdata
            .join("Microsoft")
            .join("Windows")
            .join("Start Menu")
            .join("Programs");
        let startup = programs.join("Startup");
        let desktop = profile.join("Desktop");
        for d in [&startup, &desktop] {
            std::fs::create_dir_all(d).unwrap();
        }
        let known = [
            startup.join("zectrix-dsh-pet.vbs"),
            startup.join("croc-pet.vbs"),
            programs.join("DSH 宠物.lnk"),
            desktop.join("DSH 宠物.lnk"),
            desktop.join("鳄鱼大宠物.lnk"),
        ];
        for f in &known {
            std::fs::write(f, b"legacy").unwrap();
        }
        let unrelated = desktop.join("unrelated.txt");
        std::fs::write(&unrelated, b"keep").unwrap();

        let removed = retire_legacy_autostart_in(Some(&appdata), Some(&profile));

        assert_eq!(removed, 5, "五个已知旧文件都要清掉");
        for f in &known {
            assert!(!f.exists(), "{} 应已删除", f.display());
        }
        assert!(unrelated.exists(), "不相干的桌面文件不能动");
        assert_eq!(
            retire_legacy_autostart_in(Some(&appdata), Some(&profile)),
            0,
            "第二次调用必须幂等"
        );
        assert_eq!(
            retire_legacy_autostart_in(None, None),
            0,
            "环境变量缺失时静默跳过"
        );
    }

    #[test]
    fn legacy_skin_and_animation_fields_are_ignored() {
        // 0.8.0 删掉了造型与动画：旧 state.json 里的这些键必须按未知字段静默
        // 忽略，升级后照样能读出目标地址、音量等真正还在的字段。
        let old = r#"{"model":{"settings":{"follow_enabled":true,"animation":true,
            "pet_skin":"astro","stale_minutes":15,"sound_enabled":true,"volume":90}},
            "target":"192.168.50.123","format":"gray16"}"#;
        let store: Store = serde_json::from_str(old).unwrap();
        assert_eq!(store.target.as_deref(), Some("192.168.50.123"));
        assert_eq!(store.model.settings.volume, 90);
        assert!(store.model.settings.follow_enabled);
    }
    #[test]
    fn onoff_parser_accepts_common_forms() {
        for v in ["on", "ON", "true", "1", "yes", "开"] {
            assert!(parse_onoff(v).unwrap(), "{v} 应解析为开");
        }
        for v in ["off", "OFF", "false", "0", "no", "关"] {
            assert!(!parse_onoff(v).unwrap(), "{v} 应解析为关");
        }
        assert!(parse_onoff("maybe").is_err(), "乱七八糟的值必须报错，不能猜");
        assert_eq!(onoff_label(true), "开");
        assert_eq!(onoff_label(false), "关");
    }

    fn card(status: Status, detail: &str) -> CardSig {
        CardSig {
            status: Some(status),
            title: "任务".into(),
            detail: detail.into(),
            progress: None,
            ctx_pct: Some(10),
        }
    }

    #[test]
    fn refresh_throttle_keeps_urgent_transitions_immediate() {
        // 第一次总要刷。
        assert!(refresh_due(None, &card(Status::Working, "a"), 0, 100, 30));
        // 状态切换立刻刷。
        assert!(refresh_due(
            Some(&card(Status::Working, "a")),
            &card(Status::Done, "a"),
            100,
            101,
            30
        ));
        // 工作中「工具活动」这类细节变化：节流期内不刷，到点才刷。
        assert!(!refresh_due(
            Some(&card(Status::Working, "a")),
            &card(Status::Working, "b"),
            100,
            101,
            30
        ));
        assert!(refresh_due(
            Some(&card(Status::Working, "a")),
            &card(Status::Working, "b"),
            100,
            130,
            30
        ));
        // 等待页的题干变化必须立刻刷（那是要人处理的信号）。
        assert!(refresh_due(
            Some(&card(Status::Waiting, "问题一")),
            &card(Status::Waiting, "问题二"),
            100,
            101,
            60
        ));
        // 节流为 0 = 不节流，每拍都可刷。
        assert!(refresh_due(
            Some(&card(Status::Working, "a")),
            &card(Status::Working, "b"),
            100,
            100,
            0
        ));
    }

    /// 测试用的临时目录：每个测试一份，Drop 时清掉，绝不碰真实 %LOCALAPPDATA%。
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let stamp = std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let dir = std::env::temp_dir().join(format!(
                "dsh-pet-{tag}-{}-{stamp}",
                std::process::id()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn names_in(dir: &Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect()
    }

    #[test]
    fn test_load_store_corrupt_renames_and_returns_default() {
        let dir = TempDir::new("corrupt");
        let path = dir.path().join("state.json");
        std::fs::write(&path, b"{bad").unwrap();

        let store = load_store_at(&path);

        assert!(store.degraded, "解析失败必须标记降级");
        assert_eq!(store.format, default_format(), "应返回默认 Store");
        assert!(store.model.tasks.is_empty());
        assert!(!path.exists(), "损坏文件必须改名，不能留在原位等着被覆盖");
        let corrupt: Vec<String> = names_in(dir.path())
            .into_iter()
            .filter(|n| n.starts_with("state.corrupt-"))
            .collect();
        assert_eq!(corrupt.len(), 1, "应生成 state.corrupt-<ts>.json：{corrupt:?}");
        assert_eq!(
            std::fs::read(dir.path().join(&corrupt[0])).unwrap(),
            b"{bad",
            "损坏现场必须原样保留"
        );

        // 降级期间保存：只写 recovered，不重建/不覆盖 state.json。
        save_store_at(&path, &store).unwrap();
        assert!(!path.exists(), "降级期间不得重建 state.json");
        assert!(
            dir.path().join("state.recovered.json").is_file(),
            "降级期间应写 state.recovered.json"
        );
    }

    #[test]
    fn test_save_refuses_overwrite_when_degraded() {
        let dir = TempDir::new("degraded");
        let path = dir.path().join("state.json");
        let original = br#"{"format":"gray16","target":"10.9.9.9"}"#;
        std::fs::write(&path, original).unwrap();

        let mut store = Store::default();
        store.degraded = true;
        store.target = Some("10.1.2.3".into());
        save_store_at(&path, &store).unwrap();

        assert_eq!(
            std::fs::read(&path).unwrap(),
            original,
            "降级期间绝不能覆盖已存在的 state.json"
        );
        let recovered: Store = serde_json::from_slice(
            &std::fs::read(dir.path().join("state.recovered.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(recovered.target.as_deref(), Some("10.1.2.3"));
        assert!(!recovered.degraded, "降级标记只存在内存里，不能落盘");

        // 即使调用方没带标记，磁盘上这份解析不了也不能拿默认值盖掉。
        std::fs::write(&path, b"{bad").unwrap();
        let clean = Store::default();
        save_store_at(&path, &clean).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"{bad");
    }

    #[test]
    fn test_save_keeps_previous_as_bak() {
        let dir = TempDir::new("bak");
        let path = dir.path().join("state.json");

        // 首次保存没有旧文件，不应产生 bak。
        let mut s1 = Store::default();
        s1.model.cache_version = 3; // 跳过迁移，避免加载时多写一次
        s1.target = Some("10.0.0.1".into());
        save_store_at(&path, &s1).unwrap();
        assert!(!dir.path().join("state.bak.json").exists());
        let first = std::fs::read(&path).unwrap();

        let mut s2 = load_store_at(&path);
        assert!(!s2.degraded);
        s2.target = Some("10.0.0.2".into());
        save_store_at(&path, &s2).unwrap();
        assert_eq!(
            std::fs::read(dir.path().join("state.bak.json")).unwrap(),
            first,
            "第二次保存要把上一版留成 state.bak.json"
        );
        let second = std::fs::read(&path).unwrap();

        // 再存一次：bak 滚动成最近一版，且只有一份。
        let mut s3 = load_store_at(&path);
        s3.target = Some("10.0.0.3".into());
        save_store_at(&path, &s3).unwrap();
        assert_eq!(
            std::fs::read(dir.path().join("state.bak.json")).unwrap(),
            second
        );
        assert_eq!(
            names_in(dir.path())
                .into_iter()
                .filter(|n| n.starts_with("state.bak"))
                .count(),
            1
        );
    }

    #[test]
    fn test_pet_config_field_level_fallback() {
        let dir = TempDir::new("petcfg");
        let path = dir.path().join("pet-config.json");
        std::fs::write(
            &path,
            br#"{
                "follow": true,
                "volume": "loud",
                "format": "plaid",
                "target": "192.168.1.9",
                "stale_minutes": 30,
                "mode": "minimal"
            }"#,
        )
        .unwrap();

        let cfg = load_pet_config_at(&path).expect("整体语法没错，只该丢坏字段");
        assert_eq!(cfg.volume, None, "坏 volume 必须丢弃");
        assert_eq!(cfg.format, None, "坏 format 必须丢弃");
        assert_eq!(cfg.follow, Some(true), "好字段必须保留");
        assert_eq!(cfg.target.as_deref(), Some("192.168.1.9"));
        assert_eq!(cfg.stale_minutes, Some(30));
        assert_eq!(cfg.mode.as_deref(), Some("minimal"));

        // 其余字段仍然生效：坏字段不把整份配置拖下水。
        let mut store = Store::default();
        assert!(apply_pet_config_value(&mut store, &cfg));
        assert!(store.model.settings.follow_enabled);
        assert_eq!(store.model.settings.stale_minutes, 30);
        assert_eq!(store.model.settings.volume, 90, "坏 volume 不能改动音量");
        assert_eq!(store.target.as_deref(), Some("192.168.1.9"));
        assert_eq!(store.format, "mono", "坏 format 不能改动屏幕格式");
        assert_eq!(store.model.settings.refresh_secs, 120, "mode=minimal → 120 秒节流");

        // 整体 JSON 语法错：另存 .corrupt-<ts> 并忽略整份配置。
        let bad = dir.path().join("pet-config-bad.json");
        std::fs::write(&bad, b"{bad").unwrap();
        assert!(load_pet_config_at(&bad).is_none());
        assert!(!bad.exists(), "整份损坏的配置应改名隔离");
        let quarantined: Vec<String> = names_in(dir.path())
            .into_iter()
            .filter(|n| n.starts_with("pet-config-bad.corrupt-"))
            .collect();
        assert_eq!(quarantined.len(), 1, "{quarantined:?}");
        assert_eq!(
            std::fs::read(dir.path().join(&quarantined[0])).unwrap(),
            b"{bad"
        );
    }

    #[test]
    fn settings_come_from_pet_config_only() {
        let dir = TempDir::new("single-source");
        let pet = dir.path().join("pet-config.json");
        let state = dir.path().join("state.json");
        // state.json 里还留着旧快照（音量 12、跟随关、animation/pet_skin 这类
        // 已删除的字段、标题来源 prompt）。pet-config 只写了音量与跟随：其余字段
        // 必须回落到 Rust 默认值，不能拿快照补；已删除的字段静默忽略。
        std::fs::write(
            &state,
            br#"{"model":{"cache_version":3,"settings":{"follow_enabled":false,
                "sound_enabled":false,"volume":12,"animation":false,"stale_minutes":99,
                "refresh_secs":300,"pet_skin":"astro","title_source":"prompt"}},
                "format":"gray16","target":"10.0.0.9"}"#,
        )
        .unwrap();
        std::fs::write(&pet, br#"{"volume":30,"follow":true}"#).unwrap();

        let mut store: Store = serde_json::from_slice(&std::fs::read(&state).unwrap()).unwrap();
        assert!(apply_user_config_at(&pet, &mut store));

        let s = &store.model.settings;
        assert_eq!(s.volume, 30, "pet-config 是唯一真源");
        assert!(s.follow_enabled);
        assert!(s.sound_enabled, "配置里没写的字段回落到 Rust 默认，而不是 state.json");
        assert_eq!(s.stale_minutes, 15);
        assert_eq!(s.refresh_secs, 0);
        assert_eq!(s.title_source, TitleSource::Session);
        // 配置里没有 target/format：沿用运行时状态，不能被清空。
        assert_eq!(store.format, "gray16");
        assert_eq!(store.target.as_deref(), Some("10.0.0.9"));
    }

    #[test]
    fn cli_set_survives_plugin_save() {
        let dir = TempDir::new("cli-vs-plugin");
        let pet = dir.path().join("pet-config.json");

        // CLI：只改音量与标题来源。
        write_pet_config_at(
            &pet,
            &PetConfigPatch {
                volume: Some(33),
                title_source: Some(TitleSource::Prompt),
                ..Default::default()
            },
        )
        .unwrap();

        // 插件保存：读整份 → 合上自己那几个键 → 整份写回（bridge.js writeConfig 的行为）。
        let mut map = read_pet_config_map(&pet);
        map.insert("mode".into(), serde_json::json!("calm"));
        std::fs::write(
            &pet,
            serde_json::to_vec_pretty(&serde_json::Value::Object(map)).unwrap(),
        )
        .unwrap();

        let mut store = Store::default();
        assert!(apply_user_config_at(&pet, &mut store));
        let s = &store.model.settings;
        assert_eq!(s.volume, 33, "CLI 写的音量不能被插件保存吞掉");
        assert_eq!(s.title_source, TitleSource::Prompt);
        assert_eq!(s.refresh_secs, 30, "插件写的 mode 生效");
    }

    #[test]
    fn unknown_pet_config_fields_ignored() {
        let dir = TempDir::new("unknown-keys");
        let pet = dir.path().join("pet-config.json");
        std::fs::write(
            &pet,
            r#"{"volume":20,"future_feature":{"nested":true},"comment":"手写的备注"}"#.as_bytes(),
        )
        .unwrap();

        let cfg = load_pet_config_at(&pet).expect("未知键不影响解析");
        assert_eq!(cfg.volume, Some(20));
        let mut store = Store::default();
        assert!(apply_pet_config_value(&mut store, &cfg));
        assert_eq!(store.model.settings.volume, 20);

        // 写盘要保留未知键：别把用户的备注和未来字段洗掉。
        write_pet_config_at(
            &pet,
            &PetConfigPatch {
                volume: Some(55),
                ..Default::default()
            },
        )
        .unwrap();
        let map = read_pet_config_map(&pet);
        assert_eq!(map.get("volume").and_then(|v| v.as_u64()), Some(55));
        assert!(map.contains_key("future_feature"), "未知字段必须原样保留");
        assert_eq!(
            map.get("comment").and_then(|v| v.as_str()),
            Some("手写的备注")
        );
    }

    #[test]
    fn concurrent_save_keeps_fields() {
        let dir = TempDir::new("lock");
        let path = dir.path().join("state.json");
        let mut base = Store::default();
        base.model.cache_version = 3; // 跳过迁移，别在并发里多写一次
        save_store_at(&path, &base).unwrap();

        // 两个进程/线程各写各的字段：没有锁就会互相覆盖。
        let released = path.clone();
        let a = std::thread::spawn(move || {
            for i in 0..20 {
                let mut s = load_store_at(&released);
                s.released_at = Some(1_000 + i);
                save_store_at(&released, &s).unwrap();
            }
        });
        let graced = path.clone();
        let b = std::thread::spawn(move || {
            for i in 0..20 {
                let mut s = load_store_at(&graced);
                s.idle_grace = Some(200 + i as u32);
                save_store_at(&graced, &s).unwrap();
            }
        });
        a.join().unwrap();
        b.join().unwrap();

        let disk: Store = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert!(disk.released_at.is_some(), "并发写不能把 released_at 弄丢");
        assert!(disk.idle_grace.is_some(), "并发写不能把 idle_grace 弄丢");
        assert!(
            !dir.path().join("state.lock").exists(),
            "锁必须在收尾时释放，不能留在状态目录里"
        );
    }

    #[test]
    fn title_source_defaults_to_session_and_prompt_is_opt_in() {
        // 旧 state.json / 旧 pet-config 里没有这个键：必须落回 session。
        let old: dsh_pet::model::Settings = serde_json::from_str(r#"{"volume":10}"#).unwrap();
        assert_eq!(old.title_source, TitleSource::Session);
        assert_eq!(old.title_source.as_str(), "session");

        let opt_in: dsh_pet::model::Settings =
            serde_json::from_str(r#"{"title_source":"prompt"}"#).unwrap();
        assert_eq!(opt_in.title_source, TitleSource::Prompt);

        // 大小写与空白由 parse 兜住；非法值不能悄悄落成 prompt。
        assert_eq!(TitleSource::parse(" PROMPT "), Some(TitleSource::Prompt));
        assert_eq!(TitleSource::parse("SESSION"), Some(TitleSource::Session));
        assert_eq!(TitleSource::parse("always"), None);

        // state.json 的完整反序列化路径也走 default。
        let store: Store =
            serde_json::from_str(r#"{"model":{"settings":{"volume":20}}}"#).unwrap();
        assert_eq!(store.model.settings.title_source, TitleSource::Session);
    }

    #[test]
    fn load_clears_legacy_announced_entries_and_keeps_hashes() {
        let dir = TempDir::new("announced");
        let path = dir.path().join("state.json");
        let mut store = Store::default();
        store.model.cache_version = 3;
        store.announced = vec![
            "dsh:session-aaaa-t1|w|要部署到哪台机器？".into(),
            "0123456789abcdef".into(),
        ];
        store.announced_at = 1;
        save_store_at(&path, &store).unwrap();

        let loaded = load_store_at(&path);
        assert_eq!(
            loaded.announced,
            vec!["0123456789abcdef".to_string()],
            "旧格式（含文案）必须清掉，只留 16 位 sha256"
        );
        let disk: Store =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            disk.announced,
            vec!["0123456789abcdef".to_string()],
            "迁移结果要落盘"
        );
        assert!(
            disk.announced_at > 1,
            "用新的 announced_at 防止按时间合并把旧条目捞回来"
        );
    }

    #[test]
    fn forget_titles_scrubs_existing_tasks() {
        let mut model = Model::default();
        let mut with_session = event(1, Status::Working);
        with_session.thread_id = Some("session-aaaa-bbbb-cccc".into());
        with_session.title = "机密需求：把客户数据库导出到 U 盘".into();
        model.apply(with_session, "dsh", 100).unwrap();

        let mut legacy = event(2, Status::Working);
        legacy.task_id = "legacy-1".into();
        legacy.thread_id = None;
        legacy.title = "另一个不能留的标题".into();
        model.apply(legacy, "dsh", 200).unwrap();

        assert_eq!(scrub_task_titles(&mut model), 2);
        let titles: Vec<&str> = model.tasks.iter().map(|t| t.event.title.as_str()).collect();
        assert!(
            titles.iter().any(|t| *t == "session-aaaa"),
            "有会话 id 的用 session-短号：{titles:?}"
        );
        assert!(
            titles.iter().any(|t| t.starts_with("task-")),
            "没有会话 id 的用 task-<序号>：{titles:?}"
        );
        assert!(
            titles.iter().all(|t| !t.contains("机密") && !t.contains("数据库")),
            "清理后不得残留正文：{titles:?}"
        );
        assert_eq!(scrub_task_titles(&mut model), 0, "再跑一次应是无操作");
    }

    #[test]
    fn recording_expiry_fires_after_ten_seconds() {
        let started = Instant::now();
        assert!(
            !recording_expired(started + Duration::from_secs(9), Some(started)),
            "9 秒还在时限内，不能复位"
        );
        assert!(
            recording_expired(started + Duration::from_secs(11), Some(started)),
            "11 秒已超过 10 秒兜底，必须复位"
        );
    }

    #[test]
    fn recording_expiry_needs_a_start_timestamp() {
        let now = Instant::now();
        assert!(!recording_expired(now, None), "没收到 RecordingStarted 就不该复位");
        assert!(
            !recording_expired(now, Some(now + Duration::from_secs(5))),
            "时间戳在未来（时钟回拨）也不能误判成超时"
        );
    }

    #[test]
    fn double_click_only_pushes_home_then_mute() {
        assert!(gesture_pushes_now(1), "单击立即推一帧回首页");
        assert!(
            !gesture_pushes_now(2),
            "双击的第二下不推：静音结果由 650ms 判定窗口结束后的 tick 推"
        );
    }

    #[test]
    fn outer_reconnect_backoff_is_capped_at_sixty_seconds() {
        assert_eq!(outer_reconnect_wait(1), 15);
        assert_eq!(outer_reconnect_wait(2), 30);
        assert_eq!(outer_reconnect_wait(3), 60);
        assert_eq!(outer_reconnect_wait(9), 60, "封顶 60 秒，不再线性增长");
    }

    /// 全目录扫描要真的降到 10 秒一次：中间那些拍只做已知文件的 metadata 快查。
    #[test]
    fn team_scan_full_scan_interval() {
        let mut scan = TeamScan::default();
        let t0 = Instant::now();
        assert!(scan.should_full_scan(t0), "首次必须走全扫，否则发现不了已有子会话");
        scan.last_full_scan = Some(t0);
        assert!(!scan.should_full_scan(t0 + Duration::from_secs(1)));
        assert!(
            !scan.should_full_scan(t0 + Duration::from_secs(9)),
            "10 秒内只做快查，不再遍历目录"
        );
        assert!(scan.should_full_scan(t0 + Duration::from_secs(10)));
    }

    /// `files` 不能无限增长：过期的子会话和已经删掉的目录都要清出去。
    #[test]
    fn team_scan_prune_drops_stale_and_missing() {
        let dir = TempDir::new("team-prune");
        let keep = dir.path().join("keep.zstd");
        let old = dir.path().join("old.zstd");
        let gone = dir.path().join("gone.zstd");
        std::fs::write(&keep, b"x").unwrap();
        std::fs::write(&old, b"x").unwrap();
        let at = now();
        let mut files: std::collections::HashMap<_, _> = std::collections::HashMap::new();
        files.insert(keep.clone(), (1u64, at, None, false));
        files.insert(old.clone(), (1u64, at - TEAM_STALE_SECS - 1, None, false));
        files.insert(gone.clone(), (1u64, at, None, false));

        TeamScan::prune(&mut files, at);

        assert!(files.contains_key(&keep), "新鲜且存在的文件要保留");
        assert!(!files.contains_key(&old), "超过 12 小时的子会话要丢掉");
        assert!(
            !files.contains_key(&gone),
            "已经不存在（目录被删）的路径要丢掉"
        );
    }

    /// 签名只依赖内容：同样内容同签名，坐标/文本/元素数量变了都要换签名，
    /// 否则「没变就不推」会把真实变化吞掉。
    #[test]
    fn scene_signature_is_stable_and_sensitive() {
        let base = serde_json::json!([
            {"type": "text", "x": 1, "y": 2, "text": "正在思考"},
            {"type": "rect", "x": 3, "y": 4, "w": 5, "h": 6}
        ]);
        let a: Vec<serde_json::Value> = serde_json::from_value(base.clone()).unwrap();
        let b: Vec<serde_json::Value> = serde_json::from_value(base).unwrap();
        assert_eq!(
            scene_signature(&a),
            scene_signature(&b),
            "内容相同必须同签名，否则每拍都白推一帧"
        );

        let mut moved = a.clone();
        moved[1]["x"] = serde_json::json!(31);
        assert_ne!(
            scene_signature(&a),
            scene_signature(&moved),
            "坐标变了签名必须变"
        );

        let mut retext = a.clone();
        retext[0]["text"] = serde_json::json!("正在等待");
        assert_ne!(
            scene_signature(&a),
            scene_signature(&retext),
            "文本变了签名必须变"
        );

        let mut added = a.clone();
        added.push(serde_json::json!({"type": "text", "text": ""}));
        assert_ne!(
            scene_signature(&a),
            scene_signature(&added),
            "元素数量变了签名必须变"
        );
    }

    /// 节流窗口：1 秒内只允许一次落盘，到了 1 秒立刻放行。
    #[test]
    fn save_window_allows_one_flush_per_second() {
        let t0 = Instant::now();
        assert!(!save_window_open(t0, t0), "同一时刻只允许一次写盘");
        assert!(!save_window_open(t0, t0 + Duration::from_millis(999)));
        assert!(save_window_open(t0, t0 + Duration::from_secs(1)));
        assert!(save_window_open(t0, t0 + Duration::from_secs(30)));
        assert!(
            !save_window_open(t0, t0 - Duration::from_secs(1)),
            "Instant 单调，回拨不会发生；饱和到 0 视为窗口未开"
        );
    }

    /// 磁盘缓存必须跟着文件走：外部进程改写 state.json 后不能返回旧快照。
    #[test]
    fn cached_load_store_follows_disk_rewrites() {
        let dir = TempDir::new("disk-cache");
        let path = dir.path().join("state.json");
        let mut first = Store::default();
        first.model.cache_version = 3;
        first.format = "gif".into();
        std::fs::write(&path, serde_json::to_vec_pretty(&first).unwrap()).unwrap();
        assert_eq!(cached_load_store_at(&path).format, "gif");

        let mut second = first.clone();
        second.format = "png-外部写入".into();
        second.target = Some("外部写入".into());
        std::fs::write(&path, serde_json::to_vec_pretty(&second).unwrap()).unwrap();

        let got = cached_load_store_at(&path);
        assert_eq!(got.format, "png-外部写入", "文件变了必须重新读取");
        assert_eq!(got.target.as_deref(), Some("外部写入"));
    }

    /// 签名比 `to_string()` 便宜多少：空闲时每拍都要比一次，报告里要引用这个数。
    /// 只打印数字 + 一个宽松上界（慢于序列化两倍才算失败），避免并行跑测试时
    /// 被调度噪声卡住。
    #[test]
    fn scene_signature_is_cheaper_than_serializing_the_scene() {
        let elements: Vec<serde_json::Value> = (0..200)
            .map(|i| {
                serde_json::json!({
                    "type": "text",
                    "x": i * 3,
                    "y": i * 5,
                    "w": 120,
                    "h": 18,
                    "size": 14,
                    "text": format!("第 {i} 行状态"),
                })
            })
            .collect();
        let rounds = 2000;

        let started = Instant::now();
        for _ in 0..rounds {
            std::hint::black_box(scene_signature(std::hint::black_box(&elements)));
        }
        let hash_ms = started.elapsed().as_secs_f64() * 1000.0;

        let started = Instant::now();
        for _ in 0..rounds {
            let text = serde_json::to_string(std::hint::black_box(&elements)).unwrap();
            std::hint::black_box(&text);
        }
        let string_ms = started.elapsed().as_secs_f64() * 1000.0;

        println!("[bench] 签名 {hash_ms:.1}ms / to_string {string_ms:.1}ms（{rounds} 次 × 200 元素）");
        assert!(
            hash_ms <= string_ms * 2.0,
            "hash 路径不应比序列化还慢：{hash_ms:.1}ms vs {string_ms:.1}ms"
        );
    }

    /// 一拍空闲 tick 的成本分解：会话扫描 + 团队快查 + 读缓存 + 布局+签名。
    /// 用于报告里的「tick 平均耗时」；上界放得很宽（单拍 >50ms 才算回归）。
    #[test]
    fn idle_tick_components_stay_cheap() {
        let dir = TempDir::new("tick-bench");
        let mut team = TeamScan::default();
        // 5 个已知子会话：快查模式每拍只 stat，不再重新解压。
        for i in 0..5 {
            let path = dir.path().join(format!("child-{i}.jsonl.zstd"));
            std::fs::write(&path, b"fake").unwrap();
            let meta = std::fs::metadata(&path).unwrap();
            team.touch(&path, meta.len(), mtime_secs(&meta));
        }
        // 一份落盘的 state.json：量「读缓存命中」这一条路的克隆成本。
        let state = dir.path().join("state.json");
        let mut seeded = Store::default();
        seeded.model.cache_version = 3;
        std::fs::write(&state, serde_json::to_vec_pretty(&seeded).unwrap()).unwrap();
        assert_eq!(cached_load_store_at(&state).format, seeded.format);

        let mut model = Model::default();
        let mut watcher = Watcher::new(dir.path().to_path_buf());
        let rounds = 200;
        let per = |started: Instant| started.elapsed().as_secs_f64() * 1000.0 / rounds as f64;

        let started = Instant::now();
        for _ in 0..rounds {
            let _ = watcher.scan();
        }
        let scan_ms = per(started);

        let started = Instant::now();
        for _ in 0..rounds {
            let _ = team.sync(&mut model, dir.path());
        }
        let team_ms = per(started);

        let started = Instant::now();
        for _ in 0..rounds {
            std::hint::black_box(cached_load_store_at(&state));
        }
        let cache_ms = per(started);

        let started = Instant::now();
        for _ in 0..rounds {
            let scene = layout::draw(&model, now());
            std::hint::black_box(scene_value_signature(std::hint::black_box(&scene)));
        }
        let draw_ms = per(started);

        let idle_ms = scan_ms + team_ms + cache_ms;
        println!(
            "[bench] 空闲一拍 {idle_ms:.2}ms = 扫描 {scan_ms:.4} + 团队快查 {team_ms:.4} + 读缓存 {cache_ms:.4}；布局+签名 {draw_ms:.2}（{rounds} 次平均）"
        );
        assert!(
            idle_ms + draw_ms < 50.0,
            "单拍成本异常：{:.2}ms（>50ms 说明又回到了每拍重活）",
            idle_ms + draw_ms
        );
    }

    /// `--trace-tick` 默认关闭时零成本（连样本都不记）；开启后累计样本并给出汇总。
    #[test]
    fn tick_trace_samples_only_when_enabled() {
        let mut off = TickTrace::from_args(&Args {
            cmd: "run".to_string(),
            flags: Vec::new(),
        });
        assert!(!off.enabled);
        assert!(off.summary().is_none());
        off.record(Instant::now(), || 1);
        assert_eq!(off.ticks, 0, "关闭时不该累计拍数");

        let mut on = TickTrace::from_args(&Args {
            cmd: "run".to_string(),
            flags: vec![("trace-tick".to_string(), "true".to_string())],
        });
        assert!(on.enabled);
        for _ in 0..12 {
            on.record(Instant::now(), || 7);
        }
        assert_eq!(on.ticks, 12);
        assert_eq!(on.samples.len(), 12);
        let summary = on.summary().expect("开启后应给出汇总");
        assert!(summary.contains("共 12 拍"), "{summary}");
        assert!(summary.contains("最大"), "{summary}");
    }
}
