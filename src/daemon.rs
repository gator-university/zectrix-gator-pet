//! 守护与设备循环：`cmd_run` / `cmd_watch`、断线重连、录音兜底、推屏管线。
//!
//! 从 `main.rs` 原样抽出（T9 机械拆分），除可见性外未改行为。

use super::*;

/// 本次连接建立的时刻。刚连上时音频通路可能还没就绪，立刻播放会「咔」一声
/// （实测重连后紧跟着的第一声尤其明显），先让链路安静几秒再响。
pub(crate) static CONNECTED_AT: AtomicI64 = AtomicI64::new(0);

pub(crate) async fn chime(dev: &zectrix_sdk::Device, status: Status, volume: u8) -> bool {
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
pub(crate) async fn announce(dev: &Device, model: &Model, store: &mut Store, task: &Task) {
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

/// 刚连上时的静音期：音频通路还没就绪就开播，喇叭容易「咔」一声。
pub(crate) const SOUND_SETTLE: i64 = 3;

/// 录音事件的兜底时限：`RecordingStarted` 之后这么久还没 `RecordingFinished`，
/// 就认为通知丢了、自己复位（否则 `if recording { continue }` 会让扫描/推送永久停摆）。
pub(crate) const RECORDING_TIMEOUT: Duration = Duration::from_secs(10);

/// 原地重连的退避序列：第 1/2/3 次分别等 1s、2s、4s。
pub(crate) const RECONNECT_BACKOFF_SECS: [u64; 3] = [1, 2, 4];

/// 录音是否已经超时（纯函数，便于单测）。
pub(crate) fn recording_expired(now: Instant, started_at: Option<Instant>) -> bool {
    started_at.is_some_and(|start| now.saturating_duration_since(start) >= RECORDING_TIMEOUT)
}

/// 这一下点击要不要立刻推帧。单击立即推；双击的第二下不推——静音结果等
/// 650ms 判定窗口结束后由 tick 统一推一帧，否则一次双击会连闪 3 屏。
pub(crate) fn gesture_pushes_now(click_count: u8) -> bool {
    click_count <= 1
}

/// 外层长窗口重连的等待秒数：15s → 30s → 60s 封顶（原来是 15s×次数，越等越久）。
pub(crate) fn outer_reconnect_wait(fails: u32) -> u64 {
    match fails {
        0 | 1 => 15,
        2 => 30,
        _ => 60,
    }
}

/// 设备断开后在同一个 `cmd_run` 里原地重连：最多 3 次，退避 1/2/4 秒。
/// 成功返回新连接（调用方负责关掉旧句柄）；3 次都失败返回 None，交回外层长窗口。
pub(crate) async fn reconnect_in_place(target: Option<&str>) -> Option<Device> {
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

pub(crate) async fn cmd_run(args: &Args) -> Result<()> {
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
                // 只判存活不够（PID 会被系统复用），还比对启动时记录的创建时间/镜像。
                let watched = parent_guard(
                    args.get("watch-pid")
                        .or_else(|| args.get("parent"))
                        .and_then(|v| v.parse::<u32>().ok()),
                );
                if watched.is_some_and(|g| !g.alive()) {
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
pub(crate) async fn wait_for_task(
    store: &mut Store,
    watcher: &mut Watcher,
    parent: Option<&'static ParentGuard>,
    trace: &mut TickTrace,
    verbose: bool,
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
        if parent.is_some_and(|g| !g.alive()) {
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
            let progress = ev
                .progress
                .map(|p| format!("（清单 {p}%）"))
                .unwrap_or_default();
            if verbose {
                println!(
                    "[状态] {} · {}{} · 标题：{}",
                    short_task_ref(&ev),
                    ev.status.label(),
                    progress,
                    ev.title
                );
            } else {
                // 与 cmd_run 同一隐私口径：待命阶段的导入日志也不能出现标题。
                println!("[状态] {} · {}", short_task_ref(&ev), ev.status.label());
            }
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
pub(crate) fn device_hint(e: &anyhow::Error) -> Option<&'static str> {
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

pub(crate) async fn cmd_watch(args: &Args) -> Result<()> {
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
    // 快照在启动时记一次：之后每拍比对创建时间/镜像，PID 被复用也不会当成父进程。
    let parent = parent_guard(
        args.get("watch-pid")
            .or_else(|| args.get("parent"))
            .and_then(|v| v.parse::<u32>().ok()),
    );
    // `--trace-tick` 在 watch 里也生效：待命 tick 与之后每次会话（session 继承
    // 同一批 flags）都会采样；关闭时 record 是空操作。
    let mut trace = TickTrace::from_args(args);
    loop {
        if let Some(guard) = parent {
            if !guard.alive() {
                println!("DSH 已退出：宠物收尾，释放设备。");
                return Ok(());
            }
        }
        if !dsh_running_cached() {
            tokio::time::sleep(Duration::from_secs(5)).await;
            continue;
        }
        println!("检测到 DeepSeek Harness，进入待命（只读会话，不占用设备）。");
        if !wait_for_task(&mut store, &mut watcher, parent, &mut trace, args.has("verbose")).await?
        {
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

/// 扫描一次会话：只有真实状态变化才落盘、打日志、响提示音。
/// `allow_chime` 用于抑制首次导入历史时的一串提示音；`verbose` 决定状态行
/// 是否附带标题（默认只记 `session-短号` + 状态词）。
pub(crate) async fn scan_and_report(
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
            println!("[状态] {} · {}", short_task_ref(&ev), ev.status.label());
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
pub(crate) struct CardSig {
    pub(crate) status: Option<Status>,
    pub(crate) title: String,
    pub(crate) detail: String,
    pub(crate) progress: Option<u8>,
    pub(crate) ctx_pct: Option<u32>,
}

/// 取当前主卡快照。上下文百分比只在整数位变化时才算变化（见 scan_into）。
pub(crate) fn card_sig(model: &Model, at: i64) -> CardSig {
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
pub(crate) fn refresh_due(
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
pub(crate) const QUIP_INTERVAL: Duration = Duration::from_secs(2);

/// 后台花絮采样器：UIA 是跨进程 COM 调用（`current_quip` 内部还会重试 8 次、
/// 最坏阻塞 1.5 秒），**绝不能放在推送路径上**。单独起一个线程，每 2 秒最多采一次，
/// 成功结果塞进 `Arc<Mutex<Option<String>>>`；推送时只读缓存，拿锁是微秒级。
///
/// 采样线程是进程级单例：`watch` 会反复进出 `cmd_run`，不能每次会话都再起一条。
pub(crate) struct QuipSampler {
    pub(crate) cache: Arc<Mutex<Option<String>>>,
}

impl QuipSampler {
    /// 进程内唯一实例；首次调用时启动线程并**立即**采一次。
    pub(crate) fn global() -> &'static QuipSampler {
        static INSTANCE: OnceLock<QuipSampler> = OnceLock::new();
        INSTANCE.get_or_init(QuipSampler::spawn)
    }

    pub(crate) fn spawn() -> Self {
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
    pub(crate) fn cached(&self) -> Option<String> {
        self.cache.lock().ok().and_then(|slot| slot.clone())
    }
}

/// `--trace-tick` 的采样器：只在开启时记录每拍耗时（毫秒），每 10 拍打印一行，
/// 退出时给出平均 / P95 / 峰值。默认关闭时 `record` 直接返回，等于零开销。
pub(crate) struct TickTrace {
    pub(crate) enabled: bool,
    pub(crate) ticks: u64,
    /// 只留最近 600 拍（1 秒一拍 = 10 分钟）：报告要的是分布，不是全量历史。
    pub(crate) samples: std::collections::VecDeque<u64>,
}

impl TickTrace {
    pub(crate) fn from_args(args: &Args) -> Self {
        Self {
            enabled: args.has("trace-tick"),
            ticks: 0,
            samples: std::collections::VecDeque::new(),
        }
    }

    /// 记录一拍。`elements` 是惰性求值的：每 10 拍才真的渲染一次数元素，
    /// 关闭追踪时连这个闭包都不会被调用。
    pub(crate) fn record(&mut self, started: Instant, elements: impl FnOnce() -> usize) {
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

    pub(crate) fn summary(&self) -> Option<String> {
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

/// 场景签名：按元素逐个哈希字段（类型 / 坐标 / 文本 / 颜色…），不分配字符串。
///
/// 原来是 `scene.to_string()` 再比字符串：每拍都要把整棵场景树序列化一遍
/// （几十 KB、几十次分配），而这里其实只需要一个「变没变」的布尔。
pub(crate) fn scene_signature(elements: &[serde_json::Value]) -> u64 {
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
pub(crate) fn scene_value_signature(scene: &serde_json::Value) -> u64 {
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
pub(crate) fn hash_value(value: &serde_json::Value, hasher: &mut impl std::hash::Hasher) {
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
pub(crate) fn scene_element_count(model: &Model) -> usize {
    layout::draw(model, now())
        .get("elements")
        .and_then(|v| v.as_array())
        .map_or(0, Vec::len)
}

/// 只在画面签名变化时推送。签名涵盖状态、标题、进度与当前选中项，
/// 没有真实变化就一个字节都不发——电子墨水屏刷新很贵，也不该无谓闪烁。
/// 把当前模型渲染上屏；画面没变就不重刷。
///
/// 花絮（底部那句）**不再单独触发刷新**：只有主画面确实要更新时才顺便取一次
/// 后台采样好的界面状态行，把花絮并进同一帧。否则界面状态行每变一次
/// （每个工具调用都变）就会白闪一下墨水屏——这正是「每 2 秒闪一次」的另一半原因。
pub(crate) async fn push_if_changed(
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
            // 花絮原文可能是界面上的命令/路径片段：只记固定文本，原文不落 pet.log。
            println!("[花絮] 界面状态行已更新（原文不上屏、不落日志）");
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
pub(crate) async fn push_scene(
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
