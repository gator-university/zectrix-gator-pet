//! `main.rs` 的单元测试。集中放在一处：它们共享夹具（事件、临时目录、
//! `Model` 构造），拆到各模块会重复三份；被测项都已 `pub(crate)`。

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

// ---- T9 补齐：配置校验/整体生效、state 合并、team busy、释放边界 ----

/// 用 JSON 构造一份插件配置：与写盘前校验走同一条 `pet_config_from_obj`。
fn pet_cfg(value: serde_json::Value) -> (PetConfig, Vec<String>) {
    let obj = value
        .as_object()
        .cloned()
        .expect("测试配置必须是 JSON 对象");
    let mut warnings = Vec::new();
    let cfg = pet_config_from_obj(&obj, &mut warnings);
    (cfg, warnings)
}

fn read_store(path: &Path) -> Store {
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

#[test]
fn pet_config_rejects_unknown_mode_but_keeps_other_fields() {
    let (cfg, warnings) = pet_cfg(serde_json::json!({"mode":"turbo","volume":10}));
    assert_eq!(cfg.mode, None, "未知 mode 必须丢弃，不能自己猜一个近似值");
    assert_eq!(cfg.volume, Some(10), "坏字段只影响自己，别拖累同一条配置");
    assert!(
        warnings.iter().any(|w| w.contains("mode")),
        "必须提示少了哪个字段：{warnings:?}"
    );

    let mut store = Store::default();
    assert!(apply_pet_config_value(&mut store, &cfg));
    assert_eq!(
        store.model.settings.refresh_secs, 0,
        "丢弃 mode 后回落到默认节流，而不是停在上一档"
    );
    assert_eq!(store.model.settings.volume, 10);
}

#[test]
fn pet_config_rejects_unsupported_format_without_touching_current_one() {
    let (cfg, warnings) = pet_cfg(serde_json::json!({"format":"gray8"}));
    assert_eq!(cfg.format, None);
    assert!(warnings.iter().any(|w| w.contains("format")), "{warnings:?}");

    let mut store = Store::default();
    assert!(!apply_pet_config_value(&mut store, &cfg));
    assert_eq!(store.format, "mono", "非法格式不能把当前屏幕模式改坏");
    assert_eq!(store.format_set_at, 0, "没生效就不该更新格式时间戳");
}

#[test]
fn pet_config_missing_fields_fall_back_to_rust_defaults() {
    let (cfg, warnings) = pet_cfg(serde_json::json!({"volume":30}));
    assert!(warnings.is_empty(), "缺字段是正常情况，不该报警：{warnings:?}");
    let mut store = Store::default();
    store.model.settings.volume = 99;
    store.model.settings.stale_minutes = 99;
    store.model.settings.sound_enabled = false;

    assert!(apply_pet_config_value(&mut store, &cfg));
    let expected = Settings {
        volume: 30,
        ..Settings::default()
    };
    assert!(
        settings_equal(&store.model.settings, &expected),
        "配置里没写的字段必须回落到 Rust 默认值：{:?}",
        store.model.settings
    );
}

#[test]
fn pet_config_mode_preset_wins_over_plain_refresh() {
    let (cfg, _) = pet_cfg(serde_json::json!({"mode":"calm","refresh_secs":0}));
    assert_eq!(
        settings_from_pet_config(&cfg).refresh_secs,
        30,
        "mode 是预设，显式 refresh_secs 只是兜底"
    );

    let (cfg, _) = pet_cfg(serde_json::json!({"mode":"实时"}));
    assert_eq!(settings_from_pet_config(&cfg).refresh_secs, 0, "中文别名同样识别");

    let (cfg, _) = pet_cfg(serde_json::json!({"refresh_secs":7}));
    assert_eq!(settings_from_pet_config(&cfg).refresh_secs, 7);
}

#[test]
fn config_write_rejects_invalid_field_before_touching_disk() {
    let dir = TempDir::new("set-bad-format");
    let path = dir.path().join("pet-config.json");
    let patch = PetConfigPatch {
        format: Some("gray8".into()),
        ..Default::default()
    };
    let err = write_pet_config_at(&path, &patch).unwrap_err();
    assert!(
        err.to_string().contains("拒绝写入"),
        "写盘前必须用读端同一套校验：{err}"
    );
    assert!(!path.exists(), "校验失败不得落盘，否则守护会读到坏配置");
}

#[test]
fn save_merge_prefers_the_newer_format_stamp() {
    let dir = TempDir::new("merge-format");
    let path = dir.path().join("state.json");
    std::fs::write(&path, br#"{"format":"gray16","format_set_at":100}"#).unwrap();

    // 守护手里的快照比磁盘旧：保留磁盘上的格式，否则 `set --format` 会被盖回去。
    let mut older = Store::default();
    older.format = "mono".into();
    older.format_set_at = 50;
    save_store_at(&path, &older).unwrap();
    let disk = read_store(&path);
    assert_eq!(disk.format, "gray16", "旧快照不能盖掉磁盘上的新格式");
    assert_eq!(disk.format_set_at, 100);

    // 反过来：内存里的更新，磁盘旧值不能把它改回去。
    let mut newer = Store::default();
    newer.format = "mono".into();
    newer.format_set_at = 200;
    save_store_at(&path, &newer).unwrap();
    let disk = read_store(&path);
    assert_eq!(disk.format, "mono");
    assert_eq!(disk.format_set_at, 200);
}

#[test]
fn save_merge_prefers_the_newer_announced_stamp() {
    let dir = TempDir::new("merge-announced");
    let path = dir.path().join("state.json");
    std::fs::write(
        &path,
        br#"{"announced":["0123456789abcdef"],"announced_at":200}"#,
    )
    .unwrap();

    let mut older = Store::default();
    older.announced = vec!["fedcba9876543210".into()];
    older.announced_at = 100;
    save_store_at(&path, &older).unwrap();
    let disk = read_store(&path);
    assert_eq!(
        disk.announced,
        vec!["0123456789abcdef".to_string()],
        "磁盘上刚响过的提醒不能被慢半拍的快照盖回去（盖回去 = 下轮重连再响）"
    );
    assert_eq!(disk.announced_at, 200);

    let mut newer = Store::default();
    newer.announced = vec!["fedcba9876543210".into()];
    newer.announced_at = 300;
    save_store_at(&path, &newer).unwrap();
    let disk = read_store(&path);
    assert_eq!(disk.announced, vec!["fedcba9876543210".to_string()]);
    assert_eq!(disk.announced_at, 300);
}

#[test]
fn save_merge_fills_runtime_fields_from_disk_but_keeps_explicit_ones() {
    let dir = TempDir::new("merge-fill");
    let path = dir.path().join("state.json");
    std::fs::write(
        &path,
        br#"{"target":"10.1.2.3","released_at":9,"idle_grace":120}"#,
    )
    .unwrap();

    // 守护那份快照没带这些字段：从磁盘补回来，别把设备地址/释放时间写没。
    save_store_at(&path, &Store::default()).unwrap();
    let disk = read_store(&path);
    assert_eq!(disk.target.as_deref(), Some("10.1.2.3"));
    assert_eq!(disk.released_at, Some(9));
    assert_eq!(disk.idle_grace, Some(120));

    // 内存里显式改过的值优先。
    let mut explicit = Store::default();
    explicit.target = Some("192.168.1.9".into());
    explicit.released_at = Some(77);
    explicit.idle_grace = Some(30);
    save_store_at(&path, &explicit).unwrap();
    let disk = read_store(&path);
    assert_eq!(disk.target.as_deref(), Some("192.168.1.9"));
    assert_eq!(disk.released_at, Some(77));
    assert_eq!(disk.idle_grace, Some(30));
}

#[test]
fn save_never_overwrites_a_state_file_it_cannot_parse() {
    let dir = TempDir::new("save-corrupt");
    let path = dir.path().join("state.json");
    std::fs::write(&path, b"{bad").unwrap();

    // 别的进程写了一半 JSON：即使调用方没带 degraded 标记也不能覆盖。
    save_store_at(&path, &Store::default()).unwrap();
    assert_eq!(
        std::fs::read(&path).unwrap(),
        b"{bad",
        "解析不了的文件是用户唯一的现场，不能拿默认值盖掉"
    );
    assert!(
        dir.path().join("state.recovered.json").is_file(),
        "只允许写 state.recovered.json 留给用户手工恢复"
    );
}

#[test]
fn team_scan_busy_propagates_to_known_task_and_clears() {
    let mut model = model_with(1, Status::Working, 100);
    model.tasks[0].event.thread_id = Some("main".into());

    let mut team = TeamScan::default();
    let log = PathBuf::from("proj").join("child1").join("session.v4.jsonl.zstd");
    team.files
        .insert(log.clone(), (0, 100, Some("main".into()), true));

    assert!(team.refresh_busy(&mut model, 100));
    assert!(
        model.team_busy.contains_key("main"),
        "子会话还有未结束回合，主任务必须标成忙碌：{:?}",
        model.team_busy
    );
    assert_eq!(team.busy.get("main"), Some(&100));

    // 子会话回合结束：busy 消失，任务完成时不能一直显示「子智能体运行中」。
    team.files
        .insert(log, (0, 100, Some("main".into()), false));
    assert!(team.refresh_busy(&mut model, 101));
    assert!(model.team_busy.is_empty(), "{:?}", model.team_busy);
}

#[test]
fn team_scan_busy_climbs_to_the_top_level_task() {
    let mut model = model_with(1, Status::Working, 100);
    model.tasks[0].event.thread_id = Some("boss".into());

    let mut team = TeamScan::default();
    team.files.insert(
        PathBuf::from("proj/child1/session.v4.jsonl.zstd"),
        (0, 100, Some("mid".into()), true),
    );
    team.files.insert(
        PathBuf::from("proj/mid/session.v4.jsonl.zstd"),
        (0, 100, Some("boss".into()), false),
    );

    assert!(team.refresh_busy(&mut model, 100));
    assert_eq!(
        model.team_busy.get("boss"),
        Some(&100),
        "多层子会话必须一路溯到最上层任务：{:?}",
        model.team_busy
    );
    assert!(
        !model.team_busy.contains_key("mid"),
        "中间层会话不是任务，别把侧边栏标错"
    );
}

#[test]
fn should_hold_is_exclusive_at_the_grace_boundary() {
    let model = model_with(1, Status::Done, 200);
    assert!(should_hold(&model, 259, 60), "最后一笔活动后不足 grace 秒");
    assert!(!should_hold(&model, 260, 60), "刚好满 grace 秒就该释放");
    assert!(!should_hold(&model, 200, 0), "grace=0 立刻释放");

    // 还在跑的任务无视 grace：设备不能被交还。
    let running = model_with(1, Status::Working, 200);
    assert!(should_hold(&running, 261, 60));
}
