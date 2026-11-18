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
