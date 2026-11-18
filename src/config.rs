//! 用户配置：`pet-config.json` 的解析、字段校验、合并与生效。
//!
//! 从 `main.rs` 原样抽出（T9 机械拆分），除可见性外未改行为。

use super::*;

/// 由 DSH 插件读写、宠物进程热读的唯一用户配置源：
/// %LOCALAPPDATA%\zectrix-dsh-pet\pet-config.json。
///
/// 插件设置页、`dsh-pet set`、设备手势改的都是这一个文件；
/// 守护每拍比较 mtime，改完 ≤2 秒生效，不用重启。`state.json` 里那份
/// `model.settings` 只是快照，读设置时不再参考它（旧版本的值会在首次启动时
/// 一次性迁移过来）。
pub(crate) fn pet_config_path() -> PathBuf {
    data_dir().join("pet-config.json")
}

#[derive(Debug, Default, Clone)]
pub(crate) struct PetConfig {
    pub(crate) follow: Option<bool>,
    pub(crate) sound: Option<bool>,
    pub(crate) volume: Option<u8>,
    pub(crate) target: Option<String>,
    pub(crate) format: Option<String>,
    /// realtime / balance / calm / minimal
    pub(crate) mode: Option<String>,
    pub(crate) refresh_secs: Option<u32>,
    pub(crate) stale_minutes: Option<u32>,
    /// 标题来源：session（默认，只读 session/title）/ prompt（显式 opt-in）。
    pub(crate) title_source: Option<TitleSource>,
}

/// 一次写入 `pet-config.json` 的字段集合：只有这里声明的键能被写进文件，
/// 这份结构体本身就是**写盘白名单**（值类型连同范围校验一起约束住了）。
#[derive(Debug, Default, Clone)]
pub(crate) struct PetConfigPatch {
    pub(crate) follow: Option<bool>,
    pub(crate) sound: Option<bool>,
    pub(crate) volume: Option<u8>,
    pub(crate) target: Option<String>,
    pub(crate) format: Option<String>,
    pub(crate) mode: Option<String>,
    pub(crate) refresh_secs: Option<u32>,
    pub(crate) stale_minutes: Option<u32>,
    pub(crate) title_source: Option<TitleSource>,
    /// 显式删掉 `mode`。CLI 单独改 refresh 时必须清掉它，
    /// 否则面板写下的模式会在下一拍把这项又盖回去。
    pub(crate) clear_mode: bool,
}

impl PetConfigPatch {
    /// 只产出这次真正要改的键；没设的键保持文件里的原值。
    pub(crate) fn to_map(&self) -> serde_json::Map<String, serde_json::Value> {
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

    pub(crate) fn is_empty(&self) -> bool {
        self.to_map().is_empty() && !self.clear_mode
    }
}

/// 插件设置页可选的四种模式（含中文别名）。
pub(crate) const PET_CONFIG_MODES: [&str; 8] = [
    "realtime", "实时", "balance", "平衡", "calm", "安静", "minimal", "极简",
];

/// 插件每 2 秒热读一次配置：坏字段会一直在，不能每拍刷一行日志。
/// 只有文件内容变了才重新提示。键 = (路径, 内容哈希)。
pub(crate) static PET_CONFIG_WARN: std::sync::Mutex<Option<(PathBuf, u64)>> = std::sync::Mutex::new(None);

pub(crate) fn pet_config_warn_changed(path: &Path, bytes: &[u8]) -> bool {
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
pub(crate) fn pet_bool_field(
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

pub(crate) fn pet_u32_field(
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

pub(crate) fn pet_str_field(
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
pub(crate) fn quarantine_pet_config(path: &Path, why: &str) {
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
pub(crate) fn pet_config_from_obj(
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
pub(crate) fn load_pet_config_at(path: &Path) -> Option<PetConfig> {
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
pub(crate) type PetConfigCacheEntry = (PathBuf, Option<(u64, u32, u64)>, Option<PetConfig>);

pub(crate) static PET_CONFIG_CACHE: std::sync::Mutex<Option<PetConfigCacheEntry>> =
    std::sync::Mutex::new(None);

pub(crate) fn pet_config_cache_lock() -> std::sync::MutexGuard<'static, Option<PetConfigCacheEntry>> {
    match PET_CONFIG_CACHE.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// 文件指纹：mtime（纳秒级）+ 长度。两者都没变就认为内容没变。
pub(crate) fn pet_config_stamp(path: &Path) -> Option<(u64, u32, u64)> {
    let meta = std::fs::metadata(path).ok()?;
    let modified = meta.modified().ok()?;
    let since = modified.duration_since(UNIX_EPOCH).ok()?;
    Some((since.as_secs(), since.subsec_nanos(), meta.len()))
}

pub(crate) fn invalidate_pet_config_cache() {
    *pet_config_cache_lock() = None;
}

/// 走 mtime 缓存读配置：文件没变就不再解析一遍。
pub(crate) fn load_pet_config_cached(path: &Path) -> Option<PetConfig> {
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
pub(crate) fn read_pet_config_map(path: &Path) -> serde_json::Map<String, serde_json::Value> {
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
pub(crate) fn write_pet_config_at(path: &Path, patch: &PetConfigPatch) -> Result<()> {
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
pub(crate) fn mode_preset(mode: &str) -> u32 {
    match mode {
        "realtime" | "实时" => 0,
        "balance" | "平衡" => 10,
        "minimal" | "极简" => 120,
        _ => 30,
    }
}

/// 从刷新节流反推模式显示名（日志与诊断用，和插件设置页一致）。
pub(crate) fn mode_label(refresh_secs: u32) -> &'static str {
    match refresh_secs {
        0 => "实时",
        120.. => "极简",
        30..=45 => "安静",
        _ => "平衡",
    }
}

/// 由配置生成完整的设置：缺字段一律回落到 Rust 默认值（`Settings::default`），
/// 不再拿 state.json 里那半份旧快照来补。
pub(crate) fn settings_from_pet_config(c: &PetConfig) -> Settings {
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
pub(crate) fn settings_equal(a: &Settings, b: &Settings) -> bool {
    a.follow_enabled == b.follow_enabled
        && a.stale_minutes == b.stale_minutes
        && a.sound_enabled == b.sound_enabled
        && a.volume == b.volume
        && a.refresh_secs == b.refresh_secs
        && a.title_source == b.title_source
}

/// 把一份解析好的配置套进 store：设置**整体替换**（缺字段走默认），
/// 设备地址与屏幕格式按需更新。返回是否生效了变化。
pub(crate) fn apply_pet_config_value(store: &mut Store, c: &PetConfig) -> bool {
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
pub(crate) fn migrate_state_settings_to_pet_config(path: &Path, store: &Store) -> bool {
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
pub(crate) fn apply_user_config(store: &mut Store) -> bool {
    apply_user_config_at(&pet_config_path(), store)
}

pub(crate) fn apply_user_config_at(path: &Path, store: &mut Store) -> bool {
    migrate_state_settings_to_pet_config(path, store);
    let Some(cfg) = load_pet_config_cached(path) else {
        return false;
    };
    apply_pet_config_value(store, &cfg)
}
