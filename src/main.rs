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

mod store;
use store::*;
mod config;
use config::*;
mod scan;
use scan::*;
mod daemon;
use daemon::*;
mod cli;
use cli::*;

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
        // 绝对路径：相对名会走当前目录/PATH 查找，工作目录里放个同名 exe
        // 就能劫持这次探测。SystemRoot 取不到时退回默认安装位置。
        let tasklist = std::env::var("SystemRoot")
            .map(|root| PathBuf::from(root).join("System32").join("tasklist.exe"))
            .unwrap_or_else(|_| PathBuf::from(r"C:\Windows\System32\tasklist.exe"));
        let out = std::process::Command::new(tasklist)
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
        targets.push(programs.join("Startup").join("gator-pet.vbs"));
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

/// 被监视进程的镜像名。插件把 DSH Host 的 pid 传进来（正常是
/// `DeepSeek Harness.exe`）；`dsh-pet.exe` 用于旧版自守护/手工传入宠物自身 pid
/// 的场景。名单只用于启动时提示，不据此拒绝运行。
#[cfg(windows)]
const EXPECTED_PARENT_IMAGES: [&str; 2] = ["deepseek harness.exe", "dsh-pet.exe"];

/// 快照一次被监视进程：是否存活 + 创建时间（FILETIME）+ 镜像完整路径。
/// `None` = 进程已退出/打不开。
#[cfg(windows)]
fn probe_process(pid: u32) -> Option<(Option<u64>, Option<String>)> {
    use windows::Win32::Foundation::{CloseHandle, FILETIME};
    use windows::Win32::System::Threading::{
        GetExitCodeProcess, GetProcessTimes, OpenProcess, QueryFullProcessImageNameW,
        PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    const STILL_ACTIVE: u32 = 259;
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut code = 0u32;
        if GetExitCodeProcess(handle, &mut code).is_err() || code != STILL_ACTIVE {
            let _ = CloseHandle(handle);
            return None;
        }
        let mut created = FILETIME::default();
        let mut exited = FILETIME::default();
        let mut kernel = FILETIME::default();
        let mut user = FILETIME::default();
        // 拿不到创建时间时返回 None（调用方退回只看存活），不因此判死。
        let created = GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user)
            .ok()
            .map(|_| (created.dwHighDateTime as u64) << 32 | created.dwLowDateTime as u64);
        let mut buf = [0u16; 2048];
        let mut size = buf.len() as u32;
        let image = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            windows::core::PWSTR(buf.as_mut_ptr()),
            &mut size,
        )
        .ok()
        .map(|_| String::from_utf16_lossy(&buf[..size as usize]));
        let _ = CloseHandle(handle);
        Some((created, image))
    }
}

/// 父进程身份快照。只看存活时，DSH 退出后 Windows 复用同一 PID 会让宠物
/// 一直等一个陌生进程；启动时把创建时间（和镜像名）记一次，之后每拍对比。
#[cfg(windows)]
#[derive(Clone)]
pub(crate) struct ParentGuard {
    pid: u32,
    created: Option<u64>,
    image: Option<String>,
}

#[cfg(windows)]
impl ParentGuard {
    fn resolve(pid: u32) -> Self {
        let probe = probe_process(pid);
        let created = probe.as_ref().and_then(|(c, _)| *c);
        let image = probe.as_ref().and_then(|(_, i)| i.clone());
        if created.is_none() {
            // 票面要求：拿不到创建时间时退回「只看存活」，只警告一次，不拒绝启动。
            eprintln!(
                "[watch] 拿不到父进程（pid {pid}）的创建时间，退回只看存活；PID 复用防护不可用"
            );
        }
        if let Some(img) = image.as_deref().and_then(image_file_name) {
            if !EXPECTED_PARENT_IMAGES
                .iter()
                .any(|e| img.eq_ignore_ascii_case(e))
            {
                eprintln!(
                    "[watch] 父进程镜像为 {img}（预期 DeepSeek Harness.exe 或 dsh-pet.exe），仍按创建时间校验"
                );
            }
        }
        Self {
            pid,
            created,
            image: image.as_deref().and_then(image_file_name),
        }
    }

    /// 存活且身份未变。创建时间拿不到时退化为只看存活。
    pub(crate) fn alive(&self) -> bool {
        let Some((created, image)) = probe_process(self.pid) else {
            return false;
        };
        let Some(expected) = self.created else {
            return true;
        };
        if created != Some(expected) {
            return false;
        }
        match (&self.image, image.as_deref().and_then(image_file_name)) {
            (Some(a), Some(b)) => a.eq_ignore_ascii_case(&b),
            _ => true,
        }
    }
}

#[cfg(windows)]
fn image_file_name(full: &str) -> Option<String> {
    Path::new(full)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .filter(|n| !n.is_empty())
}

/// 进程内唯一的父进程快照：启动时解析一次，三处检测（watch 主循环、
/// 待命 tick、会话 tick）共用，避免每个 tick 重新记录「当前创建时间」。
#[cfg(windows)]
static PARENT_GUARD: OnceLock<Option<ParentGuard>> = OnceLock::new();

#[cfg(windows)]
pub(crate) fn parent_guard(pid: Option<u32>) -> Option<&'static ParentGuard> {
    PARENT_GUARD.get_or_init(|| pid.map(ParentGuard::resolve)).as_ref()
}

fn render(model: &Model) -> Result<Vec<u8>> {
    let fonts = Fonts::load()?;
    let scene = layout::draw(model, now());
    raster::to_png(&scene, &fonts)
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

// 测试单独放一个文件：夹具（事件、临时目录、Model 构造）由各模块的测试共用，
// 拆到每个模块会重复三份；被测项都已是 `pub(crate)`。
#[cfg(test)]
mod tests;

