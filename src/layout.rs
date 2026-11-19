use crate::model::{Model, ProgressSource, Status, TASKS_PER_PAGE};
use crate::sprite;
use serde_json::{json, Value};

/// 品牌行由一行变两行后，分隔线从 y=30 下移到 y=34。分隔线以下的内容整体
/// 下移 2px：各组元素之间的相对位置不变，同时让 22px 的状态大字与分隔线
/// 之间留出呼吸空间，底部两态信息也不会被挤出画布。
const BODY_SHIFT: i32 = 2;

fn rect(e: &mut Vec<Value>, x: i32, y: i32, w: i32, h: i32, fill: Option<u8>) {
    let mut v = json!({"type":"rect","x":x,"y":y,"w":w,"h":h});
    if let Some(f) = fill {
        v["fill"] = json!(f);
    }
    e.push(v);
}
/// 圆角矩形（黑底白字徽章用）。
fn rect_r(e: &mut Vec<Value>, x: i32, y: i32, w: i32, h: i32, r: i32, fill: Option<u8>) {
    let mut v = json!({"type":"rect","x":x,"y":y,"w":w,"h":h,"radius":r});
    if let Some(f) = fill {
        v["fill"] = json!(f);
    }
    e.push(v);
}
/// 黑底白字徽章里的文字（白色粗体）。
fn text_white(e: &mut Vec<Value>, s: &str, x: i32, y: i32, size: i32, width: i32) {
    e.push(json!({"type":"text","content":s,"x":x,"y":y,"size":size,"width_limit":width,"weight":"bold","color":255}));
}
fn text(e: &mut Vec<Value>, s: &str, x: i32, y: i32, size: i32, width: i32) {
    e.push(json!({"type":"text","content":s,"x":x,"y":y,"size":size,"width_limit":width,"weight":if size>=24 {"bold"} else {"regular"}}));
}
fn text_fit(e: &mut Vec<Value>, s: &str, x: i32, y: i32, size: i32, width: i32) {
    e.push(json!({"type":"text","content":s,"x":x,"y":y,"size":size,"width_limit":width,"truncate":true,"weight":if size>=24 {"bold"} else {"regular"}}));
}
fn text_right(e: &mut Vec<Value>, s: &str, x: i32, y: i32, size: i32, width: i32) {
    e.push(json!({"type":"text","content":s,"x":x,"y":y,"size":size,"width_limit":width,"align":"right","weight":"regular"}));
}
fn title_lines(value: &str, width: usize, max: usize) -> Vec<String> {
    // 先按「词」切：连续的 ASCII 字母数字（含 - _ . / :）算一个词，避免
    // `workbuddy` 被拦腰截成 `workb` / `uddy`；中文与标点每字一个 token。
    let mut tokens: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut cur_ascii = false;
    for ch in value.chars() {
        let ascii_word =
            ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | '/' | ':' | '@');
        if ascii_word {
            if !cur_ascii && !cur.is_empty() {
                tokens.push(std::mem::take(&mut cur));
            }
            cur.push(ch);
            cur_ascii = true;
        } else {
            // ASCII 词到这就结束；中文/标点每个字都是一个可断行点（不能整段当一词，
            // 否则「阅读 producer-board-kit 并规划团队开发方式」会在前半段就换行，
            // 右边白白空出一大块）。
            if cur_ascii && !cur.is_empty() {
                tokens.push(std::mem::take(&mut cur));
            }
            cur_ascii = false;
            tokens.push(ch.to_string());
        }
    }
    if !cur.is_empty() {
        tokens.push(cur);
    }
    let score =
        |s: &str| -> f32 { s.chars().map(|c| if c.is_ascii() { 0.6 } else { 2.0 }).sum() };
    let mut lines: Vec<String> = vec![String::new()];
    let mut used = 0.0f32;
    for token in tokens {
        let tw = score(&token);
        // 单个 token 就超过一行（超长 URL 之类）：退回按字符硬切。
        if tw > width as f32 {
            for ch in token.chars() {
                let cw = if ch.is_ascii() { 0.6 } else { 2.0 };
                if used + cw > width as f32 && !lines.last().unwrap().is_empty() {
                    if lines.len() == max {
                        truncate_last(&mut lines);
                        return lines;
                    }
                    lines.push(String::new());
                    used = 0.0;
                }
                lines.last_mut().unwrap().push(ch);
                used += cw;
            }
            continue;
        }
        if used + tw > width as f32 && !lines.last().unwrap().is_empty() {
            if lines.len() == max {
                truncate_last(&mut lines);
                return lines;
            }
            lines.push(String::new());
            used = 0.0;
        }
        // 行首的空白丢掉，避免下一行以空格开头。
        let token = if lines.last().unwrap().is_empty() {
            token.trim_start().to_string()
        } else {
            token
        };
        used += score(&token);
        lines.last_mut().unwrap().push_str(&token);
    }
    for line in &mut lines {
        *line = line.trim_end().to_string();
    }
    lines
}

fn truncate_last(lines: &mut [String]) {
    if let Some(last) = lines.last_mut() {
        last.pop();
        last.push('…');
    }
}

/// 多任务概览的行布局：`(行顶 y, 标题可用像素宽度)`。
/// 鳄鱼从 y=152 起占右下角 (300,152)–(390,272)：行顶落在 152 附近或以下的行
/// 只用到鳄鱼左侧的 268px（20+268=288 < 300），最上面的行在鳄鱼上方，可用满
/// 整行宽度 368px（20+368=388，右边留 12px）。最后一页只有 1 项时仍沿用首行
/// 位置与整行宽度，不切成单任务详情。
fn multi_row_geometry(visible: usize) -> Vec<(i32, i32)> {
    match visible {
        2 => vec![(88, 368), (181, 268)],
        3 => vec![(76, 368), (143, 268), (210, 268)],
        _ => vec![(76, 368)],
    }
}

/// 状态图标：墨水屏上比文字更快认出来。选的都是中文系统字体覆盖的字符。
/// 「完成」例外：字体里没有像样的对勾（`✓` 缺字、`√` 是根号），所以对勾
/// 用 `draw_check` 直接画矢量线，不走字体。
pub fn status_icon(status: Option<Status>) -> &'static str {
    match status {
        Some(Status::Working) => "●",
        Some(Status::Waiting) => "！",
        Some(Status::Done) => "", // 完成用矢量对勾，见 draw_check
        Some(Status::Error) => "×",
        Some(Status::Stopped) => "■",
        Some(Status::Unknown) | None => "?",
    }
}

fn line(e: &mut Vec<Value>, x1: i32, y1: i32, x2: i32, y2: i32, width: i32) {
    e.push(json!({
        "type": "line",
        "x1": x1, "y1": y1, "x2": x2, "y2": y2,
        "width": width, "fill": 0
    }));
}

/// 矢量对勾：从左下出发到拐点，再拉向右上；圆头粗线，和文字基线对齐。
fn draw_check(e: &mut Vec<Value>, x: i32, baseline: i32, size: i32) {
    let s = size as f32;
    let th = (s * 0.14).round().max(2.0) as i32;
    let mid_x = x + (s * 0.32) as i32;
    let mid_y = baseline - (s * 0.12) as i32;
    line(
        e,
        x,
        baseline - (s * 0.44) as i32,
        mid_x,
        mid_y,
        th,
    );
    line(
        e,
        mid_x,
        mid_y,
        x + (s * 0.98) as i32,
        baseline - (s * 0.86) as i32,
        th,
    );
}

/// 画一个状态图标，返回后续文字应该从哪个 x 开始。
fn status_glyph(
    e: &mut Vec<Value>,
    status: Option<Status>,
    x: i32,
    baseline: i32,
    size: i32,
) -> i32 {
    if status == Some(Status::Done) {
        draw_check(e, x, baseline, size);
        x + (size as f32 * 1.28) as i32
    } else {
        text(e, status_icon(status), x, baseline, size, size * 2);
        x + (size as f32 * 1.05) as i32
    }
}
/// 千分位格式化：2023.15 → "2,023.15"（与设置页显示一致）。
/// 左下角的上下文占用：`上下文 40% · 剩 181K`。
/// 百分比取整、剩余按 K 取整——墨水屏上小数点没有意义，
/// 而「还剩多少」比「已用多少」更能决定该不该新开会话。
fn ctx_label(model: &Model) -> Option<String> {
    let (used, size) = match (model.ctx_used, model.ctx_size) {
        (Some(u), Some(s)) if s > 0 => (u, s),
        _ => return None,
    };
    let pct = ((used as f64 / size as f64) * 100.0).round() as u32;
    let left = size.saturating_sub(used);
    let left = if left >= 1000 {
        format!("剩 {}K", (left as f64 / 1000.0).round() as u32)
    } else {
        "剩 <1K".to_string()
    };
    Some(format!("上下文 {pct}% · {left}"))
}

pub fn draw(model: &Model, now: i64) -> Value {
    let t = model.current(now);
    let state = t.as_ref().map(|t| t.status());
    let working = state == Some(Status::Working);
    let mut e = vec![];

    // 品牌头：中文主行 + 英文副行，右上角是当前模式徽章（演示时显示「演示」）。
    // 中文 12px、基线 16；英文 9px、基线 28——英文是副行，视觉权重低于中文、
    // 高于底部注脚。两行都在徽章（y 6..26）下方、品牌行分隔线（y=34）上方。
    text(&mut e, "鳄鱼大宠物", 16, 16, 12, 168);
    text(&mut e, "Zetrix / DSH pet", 16, 28, 9, 168);
    let mode = if model.demo.is_some() {
        "演示"
    } else {
        match model.settings.refresh_secs {
            0 => "实时",
            120.. => "极简",
            30..=45 => "安静",
            _ => "平衡",
        }
    };
    rect_r(&mut e, 322, 6, 62, 20, 6, Some(0));
    text_white(&mut e, mode, 336, 21, 13, 36);
    rect(&mut e, 16, 34, 368, 0, None);

    // Waiting 永远置顶；顺序与 model.cycle 的翻页顺序保持一致。
    let active = model.display_order(now);
    let tasks: Vec<&crate::model::Task> = active.iter().collect();
    // 完成画面固定 5 秒，然后回待命画面（设备是否交还由 main 的 grace 规则决定）。
    let done_recent = t
        .as_ref()
        .is_some_and(|t| t.status() == Status::Done && now.saturating_sub(t.updated_at) < 5);
    let idle = tasks.is_empty() && !done_recent;
    let needs_action = state == Some(Status::Waiting);
    let multi = tasks.len() > 1 && model.demo.is_none();

    if multi {
        let pages = tasks.len().div_ceil(TASKS_PER_PAGE);
        let page = model
            .manual_page
            .unwrap_or_else(|| {
                model
                    .selected
                    .as_ref()
                    .and_then(|key| tasks.iter().position(|t| &t.key() == key))
                    .unwrap_or(0)
                    / TASKS_PER_PAGE
            })
            .min(pages.saturating_sub(1));
        let pending = tasks.iter().filter(|task| task.status() == Status::Waiting).count();
        let heading = format!("{} 个任务 · 待处理 {pending}", tasks.len());
        text(&mut e, &heading, 20, 52 + BODY_SHIFT, 13, 310);
        if pages > 1 {
            text_right(
                &mut e,
                &format!("{}/{}", page + 1, pages),
                320,
                52 + BODY_SHIFT,
                12,
                64,
            );
        }
        // 每页最多 3 项；行高由可见行数决定（2 项时一行给两行标题的空间）。
        let first = page * TASKS_PER_PAGE;
        let visible = &tasks[first..(first + TASKS_PER_PAGE).min(tasks.len())];
        for (i, (row_top, width)) in multi_row_geometry(visible.len()).into_iter().enumerate() {
            let Some(task) = visible.get(i) else { break };
            let y = row_top + BODY_SHIFT;
            // 标题单行，超宽按像素截断并保留省略号；下半部分收窄到鳄鱼左侧。
            text_fit(
                &mut e,
                &task.event.title,
                20,
                y + 14,
                16,
                width,
            );
            if task.status() == Status::Waiting {
                let label = task.waiting_label();
                let bar_width = if task.event.waiting_kind.is_some() { 68 } else { 72 };
                rect_r(&mut e, 20, y + 25, bar_width, 18, 3, Some(0));
                text_white(&mut e, label, 27, y + 38, 11, bar_width - 14);
            } else {
                let tx = status_glyph(&mut e, Some(task.status()), 20, y + 35, 12);
                let avail = width - (tx - 20);
                text_fit(
                    &mut e,
                    &task_status_text(task),
                    tx,
                    y + 35,
                    11,
                    avail,
                );
            }
        }
    } else if idle {
        // 待命页：没有任务时不占屏也不编造内容，只说「我在等活」。
        text(&mut e, "待命中", 20, 58 + BODY_SHIFT, 22, 220);
        text(
            &mut e,
            if model.settings.follow_enabled {
                "DSH 在线，等你派活"
            } else {
                "跟随已关闭"
            },
            20,
            88 + BODY_SHIFT,
            15,
            368,
        );
        text(&mut e, "有任务时自动上屏", 20, 142 + BODY_SHIFT, 13, 368);
    } else if let Some(cur) = tasks.first().copied().or(t.as_ref()) {
        // 已经完成的任务不在 display_order 里（那只排「正在跑/等你处理」），
        // 但完成后的五秒停留页仍要显示它，所以退回 current() 给出的那一条。
        let headline = cur.label();
        let tx = status_glyph(&mut e, state, 20, 58 + BODY_SHIFT, 22);
        text(&mut e, headline, tx, 58 + BODY_SHIFT, 22, 300 - (tx - 20));
        let title = t.as_ref().map(|t| t.event.title.as_str()).unwrap_or(
            if model.settings.follow_enabled {
                "DSH 跟随已开启"
            } else {
                "本机 DSH 跟随已关闭"
            },
        );
        for (line, title) in title_lines(title, 25, 3).iter().enumerate() {
            text(&mut e, title, 20, 82 + BODY_SHIFT + line as i32 * 19, 16, 368);
        }
        // AskUserQuestion 上屏：只显示题干 + 「回电脑处理」提示，不列选项。
        let ask = t
            .as_ref()
            .filter(|t| t.status() == Status::Waiting)
            .map(|t| t.event.detail.clone())
            .filter(|d| {
                !d.is_empty() && !d.starts_with("DSH 等待") && !d.starts_with("权限审批")
            });
        let detail = t
            .as_ref()
            .map(|t| {
                if t.stale && !t.event.status.terminal() {
                    "一段时间未收到更新".to_string()
                } else if t.event.detail.starts_with("权限审批") {
                    t.event.detail.clone()
                } else if t.status() == Status::Waiting {
                    match &ask {
                        Some(q) => q.clone(),
                        None => "请回到电脑确认".to_string(),
                    }
                } else if t.status() == Status::Done {
                    "本轮任务已结束".to_string()
                } else if t.status() == Status::Working {
                    "持续跟踪任务执行".to_string()
                } else {
                    t.event.detail.clone()
                }
            })
            .unwrap_or_else(|| {
                if model.settings.follow_enabled {
                    "在本机 DSH 中开始一个任务".to_string()
                } else {
                    "请在网页开启跟随或接入 Agent".to_string()
                }
            });
        text(
            &mut e,
            &title_lines(&detail, 40, 1).join(""),
            20,
            142 + BODY_SHIFT,
            13,
            280,
        );
        // 题干放不下时的第二行（指引框上方，基线 226+2 不碰到框顶 236+2）。
        if let Some(q) = &ask {
            let lines = title_lines(q, 32, 2);
            if lines.len() > 1 {
                text(&mut e, &lines[1], 20, 226 + BODY_SHIFT, 13, 220);
            }
        }
        if needs_action {
            rect_r(&mut e, 18, 236 + BODY_SHIFT, 122, 24, 7, Some(0));
            text_white(&mut e, "请回到电脑确认", 30, 253 + BODY_SHIFT, 14, 106);
        }
        // 需要人处理时不画进度区：那条指引才是这一刻唯一要说的事。
        let progress = t.as_ref().filter(|_| !needs_action).and_then(|t| {
            t.event
                .progress
                .map(|p| (p, t.event.progress_source, t.event.progress_steps))
        });
        if let Some((p, source, steps)) = progress {
            rect(&mut e, 20, 156 + BODY_SHIFT, 256, 13, None);
            if p > 0 {
                rect(
                    &mut e,
                    23,
                    159 + BODY_SHIFT,
                    (250u32 * p as u32 / 100) as i32,
                    7,
                    Some(0),
                );
            }
            let label = match (source, steps) {
                (ProgressSource::Todo, Some((done, total))) => {
                    format!("任务清单 {done}/{total} · {p}%")
                }
                (ProgressSource::Todo, None) => format!("任务清单完成度 {p}%"),
                (ProgressSource::Agent, _) => format!("Agent 上报进度 {p}%"),
            };
            text(&mut e, &label, 20, 184 + BODY_SHIFT, 12, 264);
        } else if working {
            let activity = t
                .as_ref()
                .map(|t| t.event.detail.as_str())
                .filter(|s| !s.is_empty())
                .unwrap_or("处理中，暂无活动明细");
            text(
                &mut e,
                &title_lines(&format!("当前实施状态：{activity}"), 36, 1).join(""),
                20,
                162 + BODY_SHIFT,
                14,
                270,
            );
        }
    }

    // 鳄鱼固定在右下角 (300,152)，多任务布局为素材留出空间。
    let action = if multi {
        if tasks.iter().any(|task| task.status() == Status::Waiting) {
            "waiting"
        } else {
            "working"
        }
    } else if idle {
            // 连续 30 分钟没有活跃任务才睡觉；never-active 的新装状态保持 idle。
            if model
                .last_active_at
                .is_some_and(|l| now.saturating_sub(l) > 1800)
            {
                "sleep"
            } else {
                "idle"
            }
        } else if needs_action {
            "waiting"
        } else if working {
            "working"
        } else if done_recent {
            "done"
        } else if matches!(state, Some(Status::Error) | Some(Status::Stopped)) {
            "error"
        } else {
            "idle"
    };
    // v2 素材就是 90×120，按 1:1 画；不再放大 3×（SPEC FR-GATOR-03）。
    sprite::draw_action(&mut e, 300, 150 + BODY_SHIFT, action, 1);

    rect(&mut e, 16, 271 + BODY_SHIFT, 368, 0, None);
    // 底部两态：有任务 = 上下文占用 + 花絮；空闲 = 电量/Wi-Fi + 等你派活。
    if idle {
        let left = match (model.battery, model.rssi) {
            (Some(b), Some(r)) => format!("电量 {b}% · Wi-Fi {}", wifi_word(r)),
            (Some(b), None) => format!("电量 {b}% · Wi-Fi 未知"),
            _ => "设备未连接".to_string(),
        };
        text(&mut e, &left, 16, 285 + BODY_SHIFT, 11, 190);
        text_right(&mut e, "等你派活", 170, 285 + BODY_SHIFT, 11, 214);
    } else {
        let source = match t
            .as_ref()
            .map(|t| (t.source.as_str(), t.event.agent.as_str()))
        {
            Some(("demo", _)) => "演示数据".to_string(),
            Some(("dsh", _)) => match ctx_label(model) {
                Some(label) => label,
                None => "DSH 本机".to_string(),
            },
            Some((_, agent)) if !agent.is_empty() => agent.to_string(),
            _ => "Agent 上报".to_string(),
        };
        text(&mut e, &source, 16, 285 + BODY_SHIFT, 11, 190);
        let quip = if model.demo.is_some() {
            "演示画面".to_string()
        } else if !model.quip.is_empty()
            && crate::model::quip_pool()
                .iter()
                .any(|q| q == &model.quip)
        {
            model.quip.clone()
        } else {
            // 状态文件里可能是旧版本留下的句子（或 quips.txt 已被替换）：
            // 不在当前池里就换一句，不要显示过时文案。
            let pool = crate::model::quip_pool();
            let pick = if pool.is_empty() {
                "跟随中".to_string()
            } else {
                pool[0].clone()
            };
            pick
        };
        text_right(
            &mut e,
            if needs_action { "设备仅作提示" } else { quip.as_str() },
            170,
            285 + BODY_SHIFT,
            11,
            214,
        );
    }
    json!({"canvas":{"w":400,"h":300},"elements":e})
}

/// 每行状态短语：进度百分比 > 活动描述 > 纯状态名。
fn task_status_text(task: &crate::model::Task) -> String {
    match task.event.progress {
        Some(p) => format!("{} · {}%", task.label(), p),
        None => {
            if task.status() == Status::Working && !task.event.detail.is_empty() {
                format!("{} · {}", task.label(), task.event.detail)
            } else {
                task.label().into()
            }
        }
    }
}

/// RSSI → 人话。设备贴近路由器约 -40，隔个房间 -60 上下，-70 以下就弱了。
fn wifi_word(rssi: i16) -> &'static str {
    if rssi >= -55 {
        "良好"
    } else if rssi >= -68 {
        "一般"
    } else {
        "弱"
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    /// 场景里内容匹配的第一条文本元素。
    fn text_el<'a>(scene: &'a Value, content: &str) -> &'a Value {
        scene["elements"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["type"] == "text" && e["content"].as_str() == Some(content))
            .unwrap_or_else(|| panic!("场景里缺少文本「{content}」"))
    }

    /// 文本的近似墨迹竖范围：基线往上一个字号（中文字面），往下 1/4 字号（降部）。
    /// 用来断言文字没有压到横向分隔线上。
    fn ink_span(el: &Value) -> (i64, i64) {
        let y = el["y"].as_i64().unwrap();
        let size = el["size"].as_i64().unwrap_or(12);
        (y - size, y + size / 4)
    }

    /// 整宽横向分隔线（rect, w=368, h=0）的 y 坐标：品牌行下、底部各一条。
    fn separator_ys(scene: &Value) -> Vec<i64> {
        scene["elements"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| {
                e["type"] == "rect" && e["w"].as_i64() == Some(368) && e["h"].as_i64() == Some(0)
            })
            .map(|e| e["y"].as_i64().unwrap())
            .collect()
    }

    /// 三段式版面（品牌行 / 版心 / 底部信息）的分界必须干净：文字不压整宽分隔线，
    /// 也不越出 400×300 画布。
    fn assert_bands_clear(scene: &Value) {
        let seps = separator_ys(scene);
        assert_eq!(seps.len(), 2, "每条场景都应有品牌行与底部两条分隔线");
        for el in scene["elements"].as_array().unwrap() {
            if el["type"] != "text" {
                continue;
            }
            let (top, bottom) = ink_span(el);
            assert!(top >= 0, "文字「{}」顶部越出画布：{top}", el["content"]);
            assert!(bottom < 300, "文字「{}」底部越出画布", el["content"]);
            for sep in &seps {
                assert!(
                    bottom < *sep || top > *sep,
                    "文字「{}」压到分隔线 y={sep}（墨迹 {top}..{bottom}）",
                    el["content"]
                );
            }
        }
    }

    #[test]
    fn fixed_rows_survive_heartbeats_selection_and_time() {
        for count in [2usize, 3, 4, 5, 9, 10] {
            let mut m = Model::default();
            for i in 0..count {
                m.apply(
                    crate::model::Event {
                        task_id: format!("task-{i:03}"),
                        thread_id: None,
                        seq: 1,
                        status: Status::Working,
                        waiting_kind: None,
                        title: format!("任务名称{i}"),
                        detail: String::new(),
                        progress: None,
                        progress_source: crate::model::ProgressSource::Agent,
                        progress_steps: None,
                        agent: "Test".into(),
                    },
                    "agent",
                    1,
                )
                .unwrap();
            }
            // apply 会轮换右下角花絮短语；本测试只关心行序，先冻结它。
            m.quip = "测试花絮".into();
            let baseline = draw(&m, 1);
            assert_eq!(baseline, draw(&m, 60), "no automatic rotation");
            let mut heartbeat = m.running(1)[0].event.clone();
            heartbeat.seq = 2;
            m.apply(heartbeat, "agent", 61).unwrap();
            m.quip = "测试花絮".into();
            assert_eq!(baseline, draw(&m, 61), "heartbeat must not reorder rows");
            let pages = count.div_ceil(TASKS_PER_PAGE);
            for page in 0..pages {
                let v = draw(&m, 62);
                // 多任务列表也不能压品牌行/底部分隔线，最多 3 行 + 两条状态行。
                assert_bands_clear(&v);
                let e = v["elements"].as_array().unwrap();
                let titles: Vec<_> = e
                    .iter()
                    .filter(|e| {
                        e["content"]
                            .as_str()
                            .is_some_and(|s| s.starts_with("任务名称"))
                    })
                    .collect();
                assert_eq!(
                    titles.len(),
                    (count - page * TASKS_PER_PAGE).min(TASKS_PER_PAGE)
                );
                for (row, title) in titles.iter().enumerate() {
                    assert_eq!(
                        title["content"],
                        format!("任务名称{}", page * TASKS_PER_PAGE + row)
                    );
                }
                assert!(titles
                    .windows(2)
                    .all(|w| w[0]["y"].as_i64() < w[1]["y"].as_i64()));
                assert!(!v.to_string().contains('%'));
                assert!(e.len() <= 400, "单帧元素 {} 超过 400", e.len());
                if pages > 1 {
                    m.cycle(1, 62);
                }
            }
            assert_eq!(baseline, draw(&m, 62), "page navigation wraps");
            m.selected = Some(m.running(62)[1].key());
            if count <= TASKS_PER_PAGE {
                assert_eq!(
                    baseline,
                    draw(&m, 62),
                    "selection keeps all rows visible"
                );
            } else {
                m.selected = None;
                m.cycle(-1, 62);
                assert!(draw(&m, 62)
                    .to_string()
                    .contains(&format!("{}/{}", pages, pages)));
            }
        }
    }
    #[test]
    fn no_progress_shows_activity_text_instead_of_bar() {
        let mut m = Model::default();
        m.apply(
            crate::model::Event {
                task_id: "task-1".into(),
                thread_id: None,
                seq: 1,
                status: Status::Working,
                waiting_kind: None,
                title: "接入宠物".into(),
                detail: "正在执行命令".into(),
                progress: None,
                progress_source: crate::model::ProgressSource::Todo,
                progress_steps: None,
                agent: "DeepSeek Harness".into(),
            },
            "dsh",
            1,
        )
        .unwrap();
        let v = draw(&m, 2);
        let s = v.to_string();
        assert!(
            s.contains("当前实施状态：正在执行命令"),
            "进度区应显示活动描述"
        );
        assert!(s.contains("持续跟踪任务执行"), "detail 行不应与进度区重复");
        assert!(!s.contains('%'), "无清单时不得出现百分比");

        // 有清单时仍是进度条 + 可核对分母，且不再出现状态文字。
        let mut ev = m.current(2).unwrap().event;
        ev.seq = 2;
        ev.progress = Some(43);
        ev.progress_steps = Some((3, 7));
        m.apply(ev, "dsh", 2).unwrap();
        let s = draw(&m, 3).to_string();
        assert!(s.contains("任务清单 3/7 · 43%"));
        assert!(!s.contains("当前实施状态"));
    }

    #[test]
    fn bottom_bar_shows_credits_and_quip() {
        let mut m = Model::default();
        m.apply(
            crate::model::Event {
                task_id: "task-1".into(),
                thread_id: None,
                seq: 1,
                status: Status::Working,
                waiting_kind: None,
                title: "接入宠物".into(),
                detail: String::new(),
                progress: None,
                progress_source: crate::model::ProgressSource::Todo,
                progress_steps: None,
                agent: "DeepSeek Harness".into(),
            },
            "dsh",
            1,
        )
        .unwrap();
        // 没有上下文数据时保持原标签；花絮短语随 apply 已换好、且来自短语池。
        let s = draw(&m, 2).to_string();
        assert!(s.contains("DSH 本机"), "无上下文数据回退原标签");
        assert!(!s.contains("上下选任务"), "按键提示已下线");
        assert!(
            crate::model::quip_pool().iter().any(|q| q == &m.quip),
            "花絮必须来自短语池"
        );

        // 有上下文数据时左下角换成占用百分比与剩余，右下角显示花絮。
        m.ctx_used = Some(119222);
        m.ctx_size = Some(300000);
        let s = draw(&m, 2).to_string();
        assert!(
            s.contains("上下文 40% · 剩 181K"),
            "左下角应显示上下文占用：{s}"
        );
        assert!(s.contains(&m.quip), "右下角应显示花絮短语");
    }

    #[test]
    fn ctx_label_shows_percent_and_headroom() {
        let mut m = Model {
            ctx_used: Some(119222),
            ctx_size: Some(300000),
            ..Model::default()
        };
        assert_eq!(ctx_label(&m).as_deref(), Some("上下文 40% · 剩 181K"));
        m.ctx_used = Some(299600);
        assert_eq!(ctx_label(&m).as_deref(), Some("上下文 100% · 剩 <1K"));
        m.ctx_size = None;
        assert_eq!(ctx_label(&m), None, "没有窗口大小就不编造百分比");
    }

    #[test]
    fn waiting_shows_question_and_call_to_action() {
        let mut m = Model::default();
        m.apply(
            crate::model::Event {
                task_id: "task-1".into(),
                thread_id: None,
                seq: 1,
                status: Status::Waiting,
                waiting_kind: Some(crate::model::WaitingKind::Question),
                title: "接入宠物".into(),
                detail: "采用哪种推送策略？".into(),
                progress: None,
                progress_source: crate::model::ProgressSource::Todo,
                progress_steps: None,
                agent: "DeepSeek Harness".into(),
            },
            "dsh",
            1,
        )
        .unwrap();
        let v = draw(&m, 2).to_string();
        assert!(v.contains("采用哪种推送策略？"), "题干应上屏");
        assert!(v.contains("请回到电脑确认"), "要有回电脑的指引");
        assert!(v.contains("设备仅作提示"), "底部提示改为仅提示");
        assert!(!v.contains("【"), "不再渲染选项高亮");
        assert!(!v.contains("上下选选项"), "不再提示用上下键选选项");
    }

    #[test]
    fn layouts_obey_hub_limits() {
        let mut m = Model::default();
        for s in [
            Status::Working,
            Status::Waiting,
            Status::Done,
            Status::Error,
            Status::Stopped,
            Status::Unknown,
        ] {
            m.demo = Some(s);
            let scene = draw(&m, 100);
            let e = scene["elements"].as_array().unwrap();
            // 90×120 鳄鱼素材贪婪矩形化后，已出图的演示页实测 245–280 个场景元素
            //（素材 208–274，版式约 10）；unknown 演示 286，仍在 400 上限内。
            assert!(e.len() <= 400, "单帧元素 {} 超过 400", e.len());
            // 品牌行分隔线与底部分隔线把版面切成三段：品牌两行 / 版心 / 底部信息。
            assert_bands_clear(&scene);
            for el in e {
                if el["type"] == "line" {
                    // 矢量图标（完成的对勾）：坐标是 x1/y1/x2/y2，同样必须在画布内。
                    assert!(el["x1"].as_i64().unwrap() >= 0);
                    assert!(el["y1"].as_i64().unwrap() >= 0);
                    assert!(el["x2"].as_i64().unwrap() < 400);
                    assert!(el["y2"].as_i64().unwrap() < 300);
                    assert!(el["width"].as_i64().unwrap() >= 1);
                    continue;
                }
                assert!(el["x"].as_i64().unwrap() >= 0);
                assert!(el["y"].as_i64().unwrap() < 300);
            }
        }
    }

    /// 鳄鱼矩形位于右下角。
    fn sprite_rects(scene: &Value) -> Vec<&Value> {
        scene["elements"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| {
                e["type"] == "rect"
                    && e["x"].as_i64().unwrap_or(0) >= 300
                    && e["y"].as_i64().unwrap_or(0) >= 152
            })
            .collect()
    }

    #[test]
    fn multi_task_overview_keeps_the_gator_and_avoids_its_area() {
        for count in [2usize, 3, 4] {
            let mut model = Model::default();
            for i in 0..count {
                model
                    .apply(
                        crate::model::Event {
                            task_id: format!("multi-{i}"),
                            thread_id: None,
                            seq: 1,
                            status: Status::Working,
                            waiting_kind: None,
                            title: format!("检查多个任务和墨水屏鳄鱼布局 {i}"),
                            detail: String::new(),
                            progress: None,
                            progress_source: crate::model::ProgressSource::Agent,
                            progress_steps: None,
                            agent: "Test".into(),
                        },
                        "agent",
                        100 + i as i64,
                    )
                    .unwrap();
            }

            let scene = draw(&model, 200);
            assert!(sprite_rects(&scene).len() > 20, "{count} 个任务仍应显示鳄鱼");
            let mut expected = Vec::new();
            sprite::draw_action(&mut expected, 300, 152, "working", 1);
            assert_eq!(sprite_rects(&scene), sprite_rects(&json!({"elements": expected})));
            let texts = scene["elements"].as_array().unwrap();
            let titles: Vec<_> = texts
                .iter()
                .filter(|element| {
                    element["type"] == "text"
                        && element["content"]
                            .as_str()
                            .is_some_and(|content| content.starts_with("检查多个任务"))
                })
                .collect();
            assert_eq!(titles.len(), count.min(TASKS_PER_PAGE));
            for title in titles {
                let right = title["x"].as_i64().unwrap() + title["width_limit"].as_i64().unwrap();
                assert!(right <= 288 || title["y"].as_i64().unwrap() < 152);
                assert_eq!(title["truncate"], true, "标题由字体度量负责截断");
            }
        }
    }

    #[test]
    fn multi_task_waits_are_highlighted_and_reflected_across_pages() {
        let mut model = Model::default();
        for (id, status, waiting_kind) in [
            ("question", Status::Waiting, Some(crate::model::WaitingKind::Question)),
            ("permission", Status::Waiting, Some(crate::model::WaitingKind::Permission)),
            ("working", Status::Working, None),
            ("working-2", Status::Working, None),
        ] {
            model
                .apply(
                    crate::model::Event {
                        task_id: id.into(),
                        thread_id: None,
                        seq: 1,
                        status,
                        waiting_kind,
                        title: format!("任务 {id}"),
                        detail: String::new(),
                        progress: None,
                        progress_source: crate::model::ProgressSource::Agent,
                        progress_steps: None,
                        agent: "Test".into(),
                    },
                    "agent",
                    100,
                )
                .unwrap();
        }

        let first_page = draw(&model, 200);
        let first_text = first_page.to_string();
        assert!(first_text.contains("4 个任务 · 待处理 2"));
        for label in ["等你回答", "等你批准"] {
            assert!(first_text.contains(label), "缺少等待类型 {label}");
            let inverse = first_page["elements"].as_array().unwrap().iter().any(|el| {
                el["type"] == "text" && el["content"] == label && el["color"] == 255
            });
            assert!(inverse, "{label} 必须以白字显示在黑底状态条上");
        }

        model.cycle(1, 200);
        let second_page = draw(&model, 200);
        let mut expected = Vec::new();
        sprite::draw_action(&mut expected, 300, 152, "waiting", 1);
        let expected_scene = json!({"elements": expected});
        assert_eq!(
            sprite_rects(&second_page),
            sprite_rects(&expected_scene),
            "当前页只有工作任务时，鳄鱼仍反映其他页的等待事项"
        );

        for id in ["question", "permission"] {
            let mut resolved = model
                .tasks
                .iter()
                .find(|task| task.event.task_id == id)
                .unwrap()
                .event
                .clone();
            resolved.seq += 1;
            resolved.status = Status::Working;
            resolved.waiting_kind = None;
            model.apply(resolved, "agent", 300).unwrap();
        }
        let all_working = draw(&model, 301);
        let mut expected = Vec::new();
        sprite::draw_action(&mut expected, 300, 152, "working", 1);
        assert_eq!(
            sprite_rects(&all_working),
            sprite_rects(&json!({"elements": expected})),
            "全部等待事项解除后，鳄鱼应恢复工作动作"
        );
    }

    #[test]
    fn manual_page_survives_progress_update() {
        let mut model = Model::default();
        for i in 0..4 {
            model
                .apply(
                    crate::model::Event {
                        task_id: format!("page-{i}"),
                        thread_id: None,
                        seq: 1,
                        status: Status::Working,
                        waiting_kind: None,
                        title: format!("任务 {i}"),
                        detail: String::new(),
                        progress: None,
                        progress_source: crate::model::ProgressSource::Agent,
                        progress_steps: None,
                        agent: "Test".into(),
                    },
                    "agent",
                    100 + i as i64,
                )
                .unwrap();
        }

        model.cycle(1, 200);
        let selected = model.selected.clone().unwrap();
        let task = model
            .tasks
            .iter()
            .find(|task| task.key() == selected)
            .unwrap();
        let mut update = task.event.clone();
        update.seq += 1;
        model.apply(update, "agent", 300).unwrap();
        model.auto_focus(300);

        assert!(draw(&model, 300).to_string().contains("2/2"));

        let mut new_wait = model
            .tasks
            .iter()
            .find(|task| task.event.task_id == "page-1")
            .unwrap()
            .event
            .clone();
        new_wait.seq += 1;
        new_wait.status = Status::Waiting;
        new_wait.waiting_kind = Some(crate::model::WaitingKind::Question);
        model.apply(new_wait, "agent", 400).unwrap();
        model.auto_focus(400);

        let first_page = draw(&model, 400).to_string();
        assert!(first_page.contains("1/2"), "新等待应把设备带回首页");
        assert!(first_page.contains("待处理 1"));
    }

    #[test]
    fn sprite_clears_footer_separator() {
        let mut m = Model::default();
        for s in [
            Status::Working,
            Status::Waiting,
            Status::Done,
            Status::Error,
            Status::Stopped,
            Status::Unknown,
        ] {
            m.demo = Some(s);
            let scene = draw(&m, 100);
            let footer_sep = *separator_ys(&scene).last().expect("底部分隔线");
            let sprite = sprite_rects(&scene);
            assert!(
                sprite.len() > 20,
                "演示状态 {s:?} 没画出鳄鱼（{} 个矩形）",
                sprite.len()
            );
            for r in sprite {
                let bottom = r["y"].as_i64().unwrap() + r["h"].as_i64().unwrap();
                assert!(
                    bottom <= footer_sep,
                    "演示状态 {s:?} 鳄鱼压到底部分隔线：bottom={bottom}, sep={footer_sep}"
                );
            }
        }
    }

    #[test]
    fn header_is_brand_only_and_idle_footer_reports_device() {
        let mut m = Model::default();
        m.settings.follow_enabled = true;
        m.battery = Some(88);
        m.rssi = Some(-60);
        let scene = draw(&m, 10_000);
        let v = scene.to_string();
        assert!(v.contains("鳄鱼大宠物"), "左上保留中文品牌词");
        assert!(v.contains("Zetrix / DSH pet"), "英文产品名作为副行上屏");
        assert!(!v.contains("AI 工作搭子"), "副标题必须删掉");
        assert!(v.contains("待命中"));
        assert!(v.contains("电量 88% · Wi-Fi 一般"));
        assert!(v.contains("等你派活"));

        // 品牌行几何：中文主行 → 英文副行 → 分隔线 → 状态大字，逐层向下。
        let zh = text_el(&scene, "鳄鱼大宠物");
        let en = text_el(&scene, "Zetrix / DSH pet");
        let headline = text_el(&scene, "待命中");
        assert_eq!(zh["size"].as_i64(), Some(12), "中文主行 12px");
        assert_eq!(en["size"].as_i64(), Some(9), "英文副行 9px");
        let (zh_y, en_y) = (zh["y"].as_i64().unwrap(), en["y"].as_i64().unwrap());
        assert!(zh_y < en_y, "英文基线必须在中文基线之下：{zh_y} / {en_y}");

        // 右上角模式徽章底边之下才有英文副行，两者不叠。
        let badge = scene["elements"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["type"] == "rect" && e["x"].as_i64() == Some(322))
            .expect("右上角模式徽章必须存在");
        assert!(
            en_y > badge["y"].as_i64().unwrap() + badge["h"].as_i64().unwrap(),
            "英文基线必须在徽章之下"
        );

        // 分隔线夹在英文副行（含降部）与状态大字之间。
        let seps = separator_ys(&scene);
        assert_eq!(seps.len(), 2, "品牌行与底部各一条分隔线");
        let (brand_sep, footer_sep) = (seps[0], seps[1]);
        assert!(en_y < brand_sep, "英文副行必须在品牌行分隔线之上");
        assert!(
            ink_span(zh).1 < brand_sep && ink_span(en).1 < brand_sep,
            "两行品牌文字都必须完整落在分隔线之上"
        );
        assert!(
            brand_sep < headline["y"].as_i64().unwrap(),
            "分隔线必须在状态大字之上"
        );
        assert!(brand_sep < 40, "分隔线紧跟品牌行，不能侵占版心：{brand_sep}");

        // 底部两态信息仍在底部分隔线之下、画布之内。
        let footer = text_el(&scene, "等你派活");
        assert!(ink_span(footer).0 > footer_sep);
        assert!(ink_span(footer).1 < 300, "底部注脚必须留在画布内");
    }

    #[test]
    fn done_screen_hands_over_to_idle_after_five_seconds() {
        let mut m = Model::default();
        m.apply(
            crate::model::Event {
                task_id: "task-1".into(),
                thread_id: None,
                seq: 1,
                status: Status::Done,
                waiting_kind: None,
                title: "整理今天的待办".into(),
                detail: String::new(),
                progress: None,
                progress_source: crate::model::ProgressSource::Agent,
                progress_steps: None,
                agent: "Test".into(),
            },
            "agent",
            1_000,
        )
        .unwrap();
        m.quip = "测试花絮".into();
        let done = draw(&m, 1_004).to_string();
        assert!(done.contains("任务完成"), "完成后先停留");
        let idle = draw(&m, 1_006).to_string();
        assert!(idle.contains("待命中"), "5 秒后回待命页");
        assert!(!idle.contains("任务完成"));
    }

    #[test]
    fn waiting_task_is_pinned_first_and_pages_show_numbers() {
        let mut m = Model::default();
        for (i, status) in [
            Status::Working,
            Status::Working,
            Status::Waiting,
            Status::Working,
        ]
        .into_iter()
        .enumerate()
        {
            m.apply(
                crate::model::Event {
                    task_id: format!("task-{i}"),
                    thread_id: None,
                    seq: 1,
                    status,
                    waiting_kind: (status == Status::Waiting)
                        .then_some(crate::model::WaitingKind::Question),
                    title: format!("任务{}", i),
                    detail: String::new(),
                    progress: None,
                    progress_source: crate::model::ProgressSource::Agent,
                    progress_steps: None,
                    agent: "Test".into(),
                },
                "agent",
                100 + i as i64,
            )
            .unwrap();
        }
        m.quip = "测试花絮".into();
        let page1 = draw(&m, 200);
        let s = page1.to_string();
        assert!(s.contains("4 个任务 · 待处理 1"), "页头显示全局待处理数量");
        assert!(s.contains("1/2"), "四个任务要显示页码");
        // Waiting 置顶：第一行必须是等待中的任务。
        let first_title = page1["elements"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["content"].as_str().is_some_and(|s| s.starts_with("任务")))
            .next()
            .and_then(|e| e["content"].as_str())
            .unwrap()
            .to_string();
        assert_eq!(first_title, "任务2", "等待任务必须排第一");
        m.cycle(1, 200);
        let page2_scene = draw(&m, 200);
        assert!(sprite_rects(&page2_scene).len() > 20, "最后一页仍显示鳄鱼");
        let page2 = page2_scene.to_string();
        assert!(page2.contains("4 个任务 · 待处理 1"));
        assert!(page2.contains("2/2"));
        assert!(page2.contains("任务0"), "第二页显示剩下的一条");
    }

    #[test]
    fn quips_file_is_editable_and_lines_fit_the_footer() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/quips.txt");
        let text = std::fs::read_to_string(path).expect("assets/quips.txt 必须存在");
        let lines: Vec<&str> = text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .collect();
        assert!(!lines.is_empty(), "花絮文件不能为空");
        for line in lines {
            assert!(
                line.chars().count() <= 20,
                "花絮超过 20 字会在底部串行：{line}"
            );
        }
    }
}
