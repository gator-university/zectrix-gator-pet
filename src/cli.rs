//! 命令行：参数解析、`cmd_*` 子命令与 `dispatch`。
//!
//! 从 `main.rs` 原样抽出（T9 机械拆分），除可见性外未改行为。

use super::*;

pub(crate) struct Args {
    pub(crate) cmd: String,
    pub(crate) flags: Vec<(String, String)>,
}

impl Args {
    pub(crate) fn parse(argv: Vec<String>) -> Self {
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

    pub(crate) fn get(&self, name: &str) -> Option<&str> {
        self.flags
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    pub(crate) fn has(&self, name: &str) -> bool {
        self.flags.iter().any(|(k, _)| k == name)
    }
}

pub(crate) fn parse_status(value: &str) -> Result<Option<Status>> {    Ok(match value.trim().to_ascii_lowercase().as_str() {
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

/// `on` / `off` / `true` / `false` / `1` / `0` / `开` / `关`。
pub(crate) fn parse_onoff(value: &str) -> Result<bool> {
    Ok(match value.trim().to_ascii_lowercase().as_str() {
        "on" | "true" | "1" | "yes" | "开" => true,
        "off" | "false" | "0" | "no" | "关" => false,
        other => anyhow::bail!("只能是 on / off，收到 {other}"),
    })
}

pub(crate) fn onoff_label(value: bool) -> &'static str {
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
pub(crate) fn cmd_set(args: &Args) -> Result<()> {
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

pub(crate) async fn cmd_preview(args: &Args) -> Result<()> {
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

pub(crate) async fn cmd_sessions(_args: &Args) -> Result<()> {
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

pub(crate) async fn cmd_check(args: &Args) -> Result<()> {
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
pub(crate) fn confirm(prompt: &str) -> bool {
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
pub(crate) async fn cmd_init(args: &Args) -> Result<()> {
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

pub(crate) async fn cmd_push(args: &Args) -> Result<()> {
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

pub(crate) async fn cmd_sound(args: &Args) -> Result<()> {
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

pub(crate) fn usage() {
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
///
/// 净化后的状态直接覆盖 state.json（不滚 `state.bak.json`），随后删掉可能
/// 残留旧标题的备份、损坏现场、恢复文件与 `watch.log`。状态文件曾损坏时仍
/// 遵守降级保护：只写 `state.recovered.json`，不覆盖、也不销毁损坏现场。
pub(crate) fn cmd_forget_titles() -> Result<()> {
    forget_titles_at(&state_path())
}

pub(crate) fn forget_titles_at(path: &Path) -> Result<()> {
    let mut degraded = false;
    if !path.exists() {
        println!("没有已保存的状态。");
    } else {
        let mut store = load_store_at(path);
        let replaced = scrub_task_titles(&mut store.model);
        let fingerprints = store.announced.len();
        store.announced.clear();
        store.announced_at = now().max(1);
        save_store_at_no_backup(path, &store)?;
        degraded = store.degraded;
        if degraded {
            println!(
                "[清理] 状态文件曾损坏：已写 {}，损坏现场与恢复文件保持不变（不覆盖 state.json）。",
                sibling_json(path, "recovered").display()
            );
        }
        println!(
            "[清理] 标题已替换 {replaced} 条（无会话 id 用 task-<序号>），清空提醒指纹 {fingerprints} 条：{}",
            path.display()
        );
    }
    cleanup_forget_leftovers(path, degraded);
    println!("提示：pet.log / 设备上的旧画面属于历史输出，不会被这条命令改写。");
    Ok(())
}

/// `forget` 无参数：删掉整份 state.json，并清理同一批旁路文件。
pub(crate) fn forget_all_at(path: &Path) {
    match std::fs::remove_file(path) {
        Ok(()) => println!("已删除 {}", path.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => println!("没有已保存的状态。"),
        Err(e) => eprintln!("删除失败: {e}"),
    }
    cleanup_forget_leftovers(path, false);
}

/// 删除 forget 的旁路文件并逐项输出。降级时保留损坏现场与恢复文件：
/// 它们是 T3 的恢复依据，销毁比留着更糟。
fn cleanup_forget_leftovers(path: &Path, keep_evidence: bool) {
    for leftover in forget_leftovers(path) {
        let name = leftover
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        if keep_evidence && (name.contains(".corrupt-") || name.ends_with(".recovered.json")) {
            println!(
                "[清理] 保留 {}（降级保护：不销毁损坏现场）",
                leftover.display()
            );
            continue;
        }
        match std::fs::remove_file(&leftover) {
            Ok(()) => println!("已删除 {}", leftover.display()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => eprintln!("[清理] 删除 {} 失败（忽略）: {e}", leftover.display()),
        }
    }
}

pub(crate) async fn dispatch(args: Args) -> Result<()> {
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
            forget_all_at(&state_path());
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
