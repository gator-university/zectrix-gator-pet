//! 状态持久化：`Store` 结构、读写与合并、磁盘缓存、写盘节流、`state.lock`。
//!
//! 从 `main.rs` 原样抽出（T9 机械拆分），除可见性外未改行为。

use super::*;

/// 本机持久化：只存运行时状态缓存，不含任何会话正文。
///
/// 用户设置（跟随/提示音/音量/刷新/标题来源/设备地址/屏幕格式）
/// 的唯一真源是 `pet-config.json`；这里的 `model.settings` / `target` / `format`
/// 只是**快照**（面板与预览读它，不必去解析配置），写入时不再参与「谁更权威」
/// 的合并——每拍 `apply_user_config` 会从配置重新生成。
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Store {
    #[serde(default)]
    pub(crate) model: Model,
    #[serde(default)]
    pub(crate) target: Option<String>,
    #[serde(default = "default_format")]
    pub(crate) format: String,
    /// 屏幕格式最后一次被显式修改的时刻（毫秒）。父子进程各持一份快照，
    /// 合并时按它取新——否则守护回写会把面板刚选的 mono 又盖回 gray16。
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub(crate) format_set_at: u64,
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
    pub(crate) announced: Vec<String>,
    /// `announced` 最后一次写入的时刻。父子两个进程各持一份 state.json 快照，
    /// 合并时按时间取新——否则父进程回写会把子进程刚响过的指纹盖回旧值，
    /// 下一轮重连又响一次（实测一次等待响 4 遍）。
    #[serde(default, skip_serializing_if = "is_zero")]
    pub(crate) announced_at: i64,
    /// 上次交还设备的时刻（秒）。释放后安静一会儿再允许重新占用，
    /// 避免「释放→新记录→立刻重占→再释放」的抖动（实测 20 分钟 13 轮）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) released_at: Option<i64>,
    /// 守护模式下，最后一个任务结束到交还设备之间保留多久（秒）。规则是
    /// 「任务完成后 5 分钟内没有新任务就解除占用」。`None` = 还没人设过，
    /// 跟随 `default_idle_grace()`——这样「显式设成 300」和「默认 300」能区分开，
    /// 否则写入合并时会把它当成默认值又让磁盘旧值盖回去。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) idle_grace: Option<u32>,
    /// 本次加载是否读到了损坏的 state.json。只存在内存里（不落盘）：
    /// 真时 `save_store` 拒绝覆盖磁盘上的 state.json，只写 `state.recovered.json`，
    /// 直到下一次成功解析干净文件才清除——否则一份坏文件会让默认值把
    /// 42KB 的任务/提醒/设置整份冲掉。
    #[serde(skip)]
    pub(crate) degraded: bool,
}

/// 任务完成后多久没有新任务就交还设备：5 分钟。
pub(crate) fn default_idle_grace() -> u32 {
    300
}

pub(crate) fn default_format() -> String {
    // 默认 mono（黑白 1 位）：设备实测整屏刷新约 1.1 秒，而 gray16 要 5 秒——
    // 16 灰阶会走完整的「多段去残影」波形，看起来就是连闪好几次。mono 的
    // 文字有网点感（Floyd–Steinberg 抖动），但刷新快、闪得少，日常更舒服。
    // 想要更细腻可以 `set --format gray16`（或面板「高级 / 诊断」里切）。
    "mono".into()
}

pub(crate) fn is_zero(v: &i64) -> bool {
    *v == 0
}

pub(crate) fn is_zero_u64(v: &u64) -> bool {
    *v == 0
}

/// 兼容旧版 `state.json` 里的 `"announced": "…"`（单个字符串）与新版数组。
/// 直接换成 `Vec<String>` 会让旧文件整份解析失败——那会把任务缓存一起丢掉，
/// 相当于把所有历史重放一遍，代价太大。
pub(crate) fn de_announced<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
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
pub(crate) const ANNOUNCED_KEEP: usize = 12;

/// 交还设备后，设备要大约 8 秒才释放独占租约：这段时间内重连只会拿到
/// 「设备忙碌」，还会把日志刷满。
pub(crate) const RELEASE_LEASE: i64 = 8;

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
    pub(crate) fn idle_grace(&self) -> i64 {
        self.idle_grace.unwrap_or_else(default_idle_grace) as i64
    }
}

/// 读 `state.json` 的 stamp：mtime（秒 + 纳秒）+ 长度。三者任一变化都算「换了
/// 内容」；文件不存在/读不了 metadata 时返回 `(None, 0)`。
pub(crate) fn state_stamp(path: &Path) -> (Option<(u64, u32)>, u64) {
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
pub(crate) struct DiskStateCache {
    pub(crate) path: PathBuf,
    pub(crate) mtime: Option<(u64, u32)>,
    pub(crate) len: u64,
    pub(crate) store: Store,
}

pub(crate) static DISK_STATE_CACHE: Mutex<Option<DiskStateCache>> = Mutex::new(None);

pub(crate) fn disk_state_cache_lock() -> std::sync::MutexGuard<'static, Option<DiskStateCache>> {
    match DISK_STATE_CACHE.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    }
}

pub(crate) fn load_store() -> Store {
    cached_load_store()
}

/// 命中缓存时零解析；返回的是快照克隆，调用方随便改。
pub(crate) fn cached_load_store() -> Store {
    cached_load_store_at(&state_path())
}

pub(crate) fn cached_load_store_at(path: &Path) -> Store {
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
pub(crate) fn refresh_disk_state_cache(path: &Path, store: &Store) {
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
pub(crate) fn corrupt_path_for(path: &Path) -> PathBuf {
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
pub(crate) fn sibling_json(path: &Path, role: &str) -> PathBuf {
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "state".into());
    path.with_file_name(format!("{stem}.{role}.json"))
}

/// `forget` 需要一并清理的旁路文件：上一版备份、损坏现场（可能有多份）、
/// 降级恢复文件、守护日志。只返回实际存在的路径，顺序稳定（备份→恢复→日志→损坏）。
pub(crate) fn forget_leftovers(path: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for candidate in [
        sibling_json(path, "bak"),
        sibling_json(path, "recovered"),
        path.with_file_name("watch.log"),
    ] {
        if candidate.exists() {
            out.push(candidate);
        }
    }
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "state".into());
    let prefix = format!("{stem}.corrupt-");
    if let Some(parent) = path.parent() {
        if let Ok(entries) = std::fs::read_dir(parent) {
            let mut corrupt: Vec<PathBuf> = entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| {
                    p.file_name()
                        .map(|n| {
                            let n = n.to_string_lossy();
                            n.starts_with(&prefix) && n.ends_with(".json")
                        })
                        .unwrap_or(false)
                })
                .filter(|p| p.exists())
                .collect();
            corrupt.sort();
            out.extend(corrupt);
        }
    }
    out
}

/// 同目录 tmp → `sync_all` → rename 覆盖。Windows 下目标被杀软/索引/别的进程
/// 短暂占用时 rename 会失败，此时回退 copy + remove，保证内容仍然落盘。
pub(crate) fn write_json_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
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
pub(crate) fn load_store_at(path: &Path) -> Store {
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
pub(crate) fn save_store_at(path: &Path, store: &Store) -> Result<()> {
    save_store_at_inner(path, store, true)
}

/// 与 [`save_store_at`] 相同，但 `backup=false` 时不把旧版滚成 `state.bak.json`。
///
/// `forget --titles` 专用：旧 state 里正是要抹掉的标题，滚成备份等于没清理。
pub(crate) fn save_store_at_no_backup(path: &Path, store: &Store) -> Result<()> {
    save_store_at_inner(path, store, false)
}

fn save_store_at_inner(path: &Path, store: &Store, backup: bool) -> Result<()> {
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
    // 后悔药，不能挡住主保存。`forget --titles` 走 no_backup，避免旧标题留在备份里。
    if backup && path.exists() {
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
pub(crate) const SAVE_MIN_INTERVAL: Duration = Duration::from_secs(1);

/// 纯函数：距上次落盘是否已经过了节流窗口（测试直接对着它断言）。
pub(crate) fn save_window_open(last_flush: Instant, at: Instant) -> bool {
    at.saturating_duration_since(last_flush) >= SAVE_MIN_INTERVAL
}

pub(crate) struct SaveThrottle {
    pub(crate) last_flush: Instant,
    pub(crate) dirty: bool,
    /// 窗口内被压下的最新一份快照。后到的那份覆盖前一份：store 是累积的，
    /// 丢中间几份没事，丢了最后一份才会丢字段。
    pub(crate) pending: Option<Store>,
}

impl SaveThrottle {
    pub(crate) fn new() -> Self {
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

pub(crate) static SAVE_THROTTLE: OnceLock<Mutex<SaveThrottle>> = OnceLock::new();

pub(crate) fn save_throttle_lock() -> std::sync::MutexGuard<'static, SaveThrottle> {
    let cell = SAVE_THROTTLE.get_or_init(|| Mutex::new(SaveThrottle::new()));
    match cell.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// 保存（节流）：1 秒内至多真正写一次盘。窗口内的请求压进 `pending`，由
/// 下一拍的 `flush_pending_store` 或退出时的强制保存落盘。写入本身仍是
/// tmp + `sync_all` + rename，不会为了省 IO 牺牲掉掉电安全。
pub(crate) fn save_store(store: &Store) -> Result<()> {
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
pub(crate) fn save_store_now(store: &Store) -> Result<()> {
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
pub(crate) fn flush_pending_store() -> Result<()> {
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
pub(crate) struct FileLock {
    pub(crate) path: PathBuf,
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
pub(crate) fn acquire_file_lock(path: &Path, timeout: std::time::Duration) -> Option<FileLock> {
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
