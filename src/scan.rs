//! 会话扫描：`TeamScan`（agent team 子会话追踪）、`scan_into`、占用判定。
//!
//! 从 `main.rs` 原样抽出（T9 机械拆分），除可见性外未改行为。

use super::*;

/// 子智能体（agent team / subagent）是否还在跑。
///
/// 现场问题：`session-bc15bf0c…` 的父会话回合已经 `turn/end completed`，但它的
/// 两个子智能体（`origin: subagent`、header 里有 `parentSession`）还有未结束的
/// 回合——DSH 侧边栏显示「1 个子智能体运行中」，设备却显示「任务完成」。
/// 这里扫同项目目录下的子会话日志，把「有未结束回合」的父会话标出来。
/// 全量扫描会话目录的间隔：目录遍历 + 每个子会话 stat。原来是每 2 秒一次，
/// 但子会话只会在 agent team 启动/结束时增删，10 秒足够；两次全扫之间只对
/// 已知文件做 `metadata()` 快查（长度/mtime 变了才解压）。
pub(crate) const TEAM_FULL_SCAN_INTERVAL: Duration = Duration::from_secs(10);

/// 子会话日志超过这个年龄就不再跟踪（解压很贵，历史记录也没有意义）。
pub(crate) const TEAM_STALE_SECS: i64 = 12 * 3600;

/// busy 溯源的最大层数（agent team 可能多层嵌套）。
pub(crate) const TEAM_MAX_DEPTH: usize = 8;

/// 单个子会话日志的读取上限。超过就整份跳过：32MB 压缩日志解出来可能是
/// 几百 MB，扫描线程不该为此吃满内存。文件回落到上限以内会重新开始跟踪。
pub(crate) const TEAM_MAX_LOG_BYTES: u64 = 32 * 1024 * 1024;

#[derive(Default)]
pub(crate) struct TeamScan {
    /// 子日志路径 → (文件长度, mtime 秒, parentSession, 是否还有未结束回合)
    pub(crate) files: std::collections::HashMap<std::path::PathBuf, (u64, i64, Option<String>, bool)>,
    pub(crate) last_full_scan: Option<std::time::Instant>,
    pub(crate) busy: std::collections::HashMap<String, i64>,
    /// 已经警告过「文件过大」的路径：每个文件只提醒一次，别每 2 秒刷屏。
    pub(crate) warned_oversize: std::collections::HashSet<std::path::PathBuf>,
}

impl TeamScan {
    /// 该不该走一遍全目录扫描。默认是「第一次一定扫，之后每 10 秒一次」。
    pub(crate) fn should_full_scan(&self, at: std::time::Instant) -> bool {
        match self.last_full_scan {
            Some(last) => at.saturating_duration_since(last) >= TEAM_FULL_SCAN_INTERVAL,
            None => true,
        }
    }

    /// 丢掉过期（>12h）或已经不存在的子会话日志，避免 `files` 无限增长。
    /// 用 `metadata` 而不是 `is_file`：一次 stat 就能同时确认存在并刷新
    /// 长度/mtime，文件被截断时下一轮 `touch` 会重新解码。
    pub(crate) fn prune(
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
    pub(crate) fn touch(&mut self, path: &Path, len: u64, mtime: i64) {
        // 读之前先看长度：`fs::read` 会把整份文件装进内存，32MB 的压缩日志
        // 解压后可能几百 MB。超限就丢掉旧缓存并跳过（不缓存错误状态）。
        if len > TEAM_MAX_LOG_BYTES {
            self.files.remove(path);
            if self.warned_oversize.insert(path.to_path_buf()) {
                eprintln!(
                    "[scan] 子会话日志超过 {}MB，跳过（只影响 busy 判定）：{}",
                    TEAM_MAX_LOG_BYTES / 1024 / 1024,
                    path.display()
                );
            }
            return;
        }
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
    pub(crate) fn full_scan(&mut self, root: &Path, at: i64) {
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
    pub(crate) fn quick_scan(&mut self, at: i64) {
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
    pub(crate) fn refresh_busy(&mut self, model: &mut dsh_pet::model::Model, at: i64) -> bool {
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
    pub(crate) fn sync(&mut self, model: &mut dsh_pet::model::Model, root: &std::path::Path) -> bool {
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
pub(crate) fn decode_team_log(bytes: &[u8]) -> (Option<String>, bool) {
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
pub(crate) fn mtime_secs(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|m| m.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub(crate) fn scan_into(model: &mut Model, watcher: &mut Watcher) -> (Vec<TaskEvent>, bool) {
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
pub(crate) fn should_hold(model: &Model, at: i64, grace: i64) -> bool {
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
pub(crate) fn may_release(model: &Model, at: i64, grace: i64) -> bool {
    !should_hold(model, at, grace)
}

/// 会话文件多久没动算「已停笔」。释放设备前必须满足它，否则会在对方
/// 还在写日志的时候把连接撤掉，下一次扫描又把它拉起来。
pub(crate) const WRITING_WINDOW: i64 = 30;

/// 最近 `WRITING_WINDOW` 秒内还有**正在跟随的**会话日志在增长：说明那边的活没停。
/// 屏幕上的状态可能没变（同一轮里持续在跑），但这时候把设备交还，下一次扫描
/// 又会把它抢回来——先等它停笔。只认模型里跟踪的会话，已归档/子代理等不上屏的
/// 文件不能拿来续命，否则设备会被它们永久占住。
pub(crate) fn session_writing(model: &Model) -> bool {
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
