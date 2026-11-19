use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Working,
    Waiting,
    Done,
    Error,
    Stopped,
    Unknown,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WaitingKind {
    Question,
    Permission,
}

impl Status {
    pub fn label(self) -> &'static str {
        match self {
            Self::Working => "正在工作",
            Self::Waiting => "等你处理",
            Self::Done => "任务完成",
            Self::Error => "遇到问题",
            Self::Stopped => "已停止",
            Self::Unknown => "状态待确认",
        }
    }
    pub fn terminal(self) -> bool {
        matches!(self, Self::Done | Self::Error | Self::Stopped)
    }
}

/// 百分比的来源。两者语义不同，界面上必须分开表述：
/// - `Agent`：Agent 自己上报的进度，可直接称为「上报进度」。
/// - `Todo`：从本机会话的任务清单（todo_write）推算的完成项占比，
///   只能称为「清单完成度」，不得冒充 Agent 上报。
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProgressSource {
    #[default]
    Agent,
    Todo,
}

/// 任务标题的来源。
///
/// - `Session`（默认）：只读 DSH 的 `session/title` 事件；没有标题时用
///   「项目目录名 + 第 N 轮」占位。**绝不读 user/message 正文。**
/// - `Prompt`：显式 opt-in，会话没有标题时退回首条用户消息的首行，并把它
///   写进 state.json 与设备。提示词可能包含敏感内容，默认关闭。
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TitleSource {
    #[default]
    Session,
    Prompt,
}
impl TitleSource {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "session" => Some(Self::Session),
            "prompt" => Some(Self::Prompt),
            _ => None,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Session => "session",
            Self::Prompt => "prompt",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Event {
    pub task_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    pub seq: u64,
    pub status: Status,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiting_kind: Option<WaitingKind>,
    pub title: String,
    #[serde(default)]
    pub detail: String,
    #[serde(default)]
    pub progress: Option<u8>,
    #[serde(default)]
    pub progress_source: ProgressSource,
    /// 清单来源时的 `(已完成, 总数)`，用于显示 `3/7` 这类可核对的分母。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress_steps: Option<(u32, u32)>,
    #[serde(default)]
    pub agent: String,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Task {
    pub event: Event,
    pub source: String,
    pub updated_at: i64,
    pub stale: bool,
    /// 这条会话自己的回合已经结束，但它的子智能体（agent team / subagent）
    /// 还有未结束的回合在跑。DSH 侧边栏会显示「N 个子智能体运行中」，
    /// 所以设备也不该把它当成「任务完成」。
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub subagent_busy: bool,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub waiting_order: u64,
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}
impl Task {
    pub fn waiting_label(&self) -> &'static str {
        match self.event.waiting_kind {
            Some(WaitingKind::Question) => "等你回答",
            Some(WaitingKind::Permission) => "等你批准",
            None => "等你处理",
        }
    }

    pub fn label(&self) -> &'static str {
        if self.subagent_busy {
            return "子智能体运行中";
        }
        if self.status() == Status::Waiting
            && self.event.waiting_kind == Some(WaitingKind::Permission)
        {
            "等待权限审批"
        } else {
            self.status().label()
        }
    }
    pub fn status(&self) -> Status {
        if self.subagent_busy {
            return Status::Working;
        }
        if self.stale && !self.event.status.terminal() {
            Status::Unknown
        } else {
            self.event.status
        }
    }
    pub fn key(&self) -> String {
        format!("{}:{}", self.source, self.event.task_id)
    }
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct Settings {
    pub follow_enabled: bool,
    pub stale_minutes: u32,
    pub sound_enabled: bool,
    pub volume: u8,
    /// 常规刷新节流：两次「内容更新」之间至少隔多少秒（0 = 不节流，每 2 秒都可刷）。
    ///
    /// 墨水屏每次刷屏会闪一下，而「正在执行命令」这类细节会频繁变化；节流后
    /// 状态切换（工作/等待/完成/出错）仍然立刻上屏，只有细节变化要等这个间隔。
    #[serde(default)]
    pub refresh_secs: u32,
    /// 任务标题来源。默认 `session`，老 state.json 没这个字段时也按它处理。
    #[serde(default)]
    pub title_source: TitleSource,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            follow_enabled: true,
            stale_minutes: 15,
            sound_enabled: true,
            volume: 90,
            // 默认不节流，保持原有「跟着 Agent 实时动」的行为；嫌闪的人用
            // `dsh-pet set --calm on` 一键切成安静模式（30 秒节流）。
            refresh_secs: 0,
            title_source: TitleSource::Session,
        }
    }
}
#[derive(Clone, Default, Debug, Deserialize, Serialize)]
pub struct Model {
    #[serde(default)]
    pub cache_version: u32,
    #[serde(default)]
    pub tasks: Vec<Task>,
    #[serde(default)]
    pub settings: Settings,
    /// 当前展示会话的上下文占用（已用 / 窗口），来自 DSH 会话日志里的
    /// token 用量与 `contextWindow`——本地精确可读、实时更新，
    /// 比手填的云端积分余额可靠，也直接决定「还能聊多久」。
    #[serde(default)]
    pub ctx_used: Option<u32>,
    #[serde(default)]
    pub ctx_size: Option<u32>,
    /// 最近一次连上设备时读到的电量（%）与 Wi-Fi 信号（dBm）。
    /// 空闲页左下角显示「电量 N% · Wi-Fi 良好/一般/弱」；设备不在手边时
    /// 没有新读数，就显示最后一次的，不编造。
    /// 最近一次有活跃任务的时间（秒）。待命页用它决定鳄鱼是 `idle` 还是 `sleep`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_active_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub battery: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub battery_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rssi: Option<i16>,
    /// 底部花絮短语。优先用推送时刻从 DeepSeek Harness 界面读到的真状态
    /// （见 `set_live_quip`）；读不到时退回内置短语池轮换。
    #[serde(default)]
    pub quip: String,
    /// 子智能体还在跑的**父会话**（键 = 父会话 id，值 = 最近一次确认时间）。
    /// 父会话自己的回合可能已经 `turn/end completed`，但 agent team 的子智能体
    /// 还有未结束的回合；DSH 界面会显示「N 个子智能体运行中」，这里跟着显示。
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub team_busy: std::collections::HashMap<String, i64>,
    #[serde(skip)]
    pub selected: Option<String>,
    #[serde(skip)]
    pub manual_page: Option<usize>,
    #[serde(skip)]
    pub demo: Option<Status>,
}

/// 拟人花絮短语池。只取动作中的幽默状态，不碰任务内容。
const QUIPS: [&str; 16] = [
    "正在搜肠刮肚，肚子已经刮完了",
    "正在绞尽脑汁，汁水四溅",
    "正在翻箱倒柜找思路",
    "正在脑内疯狂充电",
    "正在敲锣打鼓跑代码",
    "正在低头猛干，勿扰",
    "正在翻阅祖传笔记",
    "正在和 bug 斗智斗勇",
    "正在给思路抛光打蜡",
    "正在榨干最后一点脑力",
    "正在闭关修炼，请勿打扰",
    "正在灵光乍现的边缘试探",
    "正在搬运思想的砖",
    "正在苦思冥想，香火勿断",
    "正在整理脑内书架",
    "正在给灵感浇水",
];

/// 花絮池：优先读可替换的 `assets/quips.txt`（一行一句，`#` 开头为注释，
/// 超过 20 字截断），找不到或为空时退回内置池。路径优先级：
/// `DSH_PET_QUIPS` 环境变量 → exe 同目录 `assets/quips.txt` / `quips.txt`
/// → 当前目录 `assets/quips.txt`（开发仓库直接跑时）。
pub fn quip_pool() -> &'static Vec<String> {
    use std::sync::OnceLock;
    static POOL: OnceLock<Vec<String>> = OnceLock::new();
    POOL.get_or_init(|| {
        let mut candidates: Vec<std::path::PathBuf> = Vec::new();
        if let Ok(p) = std::env::var("DSH_PET_QUIPS") {
            candidates.push(p.into());
        }
        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent() {
                candidates.push(dir.join("assets").join("quips.txt"));
                candidates.push(dir.join("quips.txt"));
            }
        }
        candidates.push(std::path::PathBuf::from("assets/quips.txt"));
        for path in candidates {
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let mut out: Vec<String> = Vec::new();
            for line in text.lines() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                out.push(line.chars().take(20).collect());
            }
            if !out.is_empty() {
                return out;
            }
        }
        QUIPS.iter().map(|s| s.to_string()).collect()
    })
}

/// 轮换一条与当前不同的短语。用纳秒做伪随机即可——没必要引入 rand。
fn next_quip(current: &str) -> String {
    let pool = quip_pool();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as usize)
        .unwrap_or(0);
    let mut idx = nanos % pool.len();
    if pool[idx] == current {
        idx = (idx + 1) % pool.len();
    }
    pool[idx].clone()
}

/// 花絮在 400×300 画面上只占右下角一条（11 号字、约 214px 宽），
/// 超过 18 个汉字就会顶到左边的上下文占用标签上。
const QUIP_MAX_CHARS: usize = 20;

/// 压到一行的宽度内；超长时截断加省略号，而不是任由它撞到左边的标签。
fn fit_quip(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= QUIP_MAX_CHARS {
        return chars.into_iter().collect();
    }
    let mut out: String = chars[..QUIP_MAX_CHARS - 1].iter().collect();
    out.push('…');
    out
}
fn valid_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 160
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_:.".contains(&b))
}
pub fn clean(s: &str, max: usize) -> String {
    s.chars()
        .filter(|c| {
            !c.is_control() && !matches!(*c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        })
        .take(max)
        .collect()
}
pub const TASKS_PER_PAGE: usize = 3;

/// 主卡换人需要新任务领先这么多秒。两个会话并行时各自每几秒写一条记录，
/// 时间差一直很小；只有对方确实领先一大截（自己长时间没动）才值得换屏。
const FOCUS_HYSTERESIS: i64 = 120;

impl Model {
    /// 重建一次派生的跟随缓存（沿用上游迁移语义）；外部 Agent 事件与设置保留。
    /// 版本 2：轮次 ID 改为时间戳格式（-u<ts>），旧的序号式 ID（-t<N>）全部作废，
    /// 否则尾读重数后的新事件会被旧任务的 seq 挡在门外。
    /// 版本 3：跟随源从 DeepSeek Harness 换成 DeepSeek Harness（`dsh`），上一版的
    /// 任务缓存整批退场，免得设备停在一条早就没人跟的会话上。
    pub fn migrate_cache(&mut self) -> bool {
        if self.cache_version >= 3 {
            return false;
        }
        self.tasks.retain(|t| t.source != "dsh");
        self.cache_version = 3;
        true
    }

    pub fn apply(&mut self, mut event: Event, source: &str, at: i64) -> Result<bool> {
        if !valid_id(&event.task_id) || event.seq == 0 {
            bail!("task_id 须为字母、数字或 -_: .，seq 须大于 0")
        }
        if event.title.trim().is_empty()
            || event.title.chars().count() > 120
            || event.detail.chars().count() > 240
            || event.agent.chars().count() > 40
        {
            bail!("任务标题不能为空，标题/详情/Agent 名称最多 120/240/40 个字符")
        }
        if event.progress.is_some_and(|p| p > 100) {
            bail!("进度必须为 0–100，未知时省略")
        }
        event.title = clean(event.title.trim(), 120);
        event.detail = clean(&event.detail, 240);
        event.agent = clean(&event.agent, 40);
        if event.status != Status::Waiting {
            event.waiting_kind = None;
        }
        if event.title.is_empty() {
            bail!("任务标题无有效字符")
        }
        // 记录旧状态：花絮只在状态真正变了才换。同一个「等你处理」每 2 秒
        // 被重复上报（等待期间事件持续产生）时，换花絮会让整屏反复刷新。
        let task_id = event.task_id.clone();
        let (before, old_waiting_order) = self
            .tasks
            .iter()
            .find(|t| t.source == source && t.event.task_id == task_id)
            .map(|t| (Some(t.status()), t.waiting_order))
            .unwrap_or((None, 0));
        let entered_waiting = event.status == Status::Waiting && before != Some(Status::Waiting);
        let waiting_order = if event.status != Status::Waiting {
            0
        } else if entered_waiting {
            self.tasks
                .iter()
                .filter(|t| t.event.status == Status::Waiting)
                .map(|t| t.waiting_order)
                .max()
                .unwrap_or(0)
                .saturating_add(1)
        } else if old_waiting_order == 0 {
            0
        } else {
            old_waiting_order
        };
        if let Some(t) = self
            .tasks
            .iter_mut()
            .find(|t| t.source == source && t.event.task_id == event.task_id)
        {
            if event.seq < t.event.seq {
                return Ok(false);
            }
            if event.seq == t.event.seq {
                // 同一条记录被重新推导（守护重启后的全新扫描）：记录本身没变，
                // 但派生字段可能变了——最典型的是会话标题在会话中途被重新生成，
                // 重新扫描才第一次读到。内容有变就更新文案，完全一致才去重。
                if event.status == t.event.status
                    && (event.title != t.event.title
                        || event.detail != t.event.detail
                        || event.waiting_kind != t.event.waiting_kind)
                {
                    t.event.title = event.title;
                    t.event.detail = event.detail;
                    t.event.waiting_kind = event.waiting_kind;
                    return Ok(true);
                }
                return Ok(false);
            }
            // 会话文件会把上一轮的完成记录延迟补写进下一轮（实测存在）：
            // terminal 任务随后收到更新的非终态事件时必须复活，否则整轮从此静默。
            // 只有「终态改终态」仍然拒绝，外部 Agent 乱报完成态不受影响。
            if t.event.status.terminal() && event.status.terminal() {
                bail!("任务已结束且新事件也是终态；新任务请使用新的 task_id")
            }
            *t = Task {
                event,
                source: source.into(),
                updated_at: at,
                stale: false,
                subagent_busy: false,
                waiting_order,
            };
        } else {
            self.tasks.push(Task {
                event,
                source: source.into(),
                updated_at: at,
                stale: false,
                subagent_busy: false,
                waiting_order,
            });
        }
        if entered_waiting {
            self.manual_page = Some(0);
        }
        self.tasks
            .sort_by_key(|t| std::cmp::Reverse((t.updated_at, t.event.seq)));
        self.tasks.truncate(100);
        // 状态真的变了才换花絮：同一状态的心跳/等待重复上报不再改动画面。
        if before != Some(self.status_of(&task_id, source)) {
            self.quip = next_quip(&self.quip);
        }
        Ok(true)
    }

    /// 把一条会话从跟随里整条移除（如被识别为定时任务类型）：
    /// 该会话的所有轮次一并退场，正在上屏的也会立刻让位。
    /// 返回是否真的删掉了东西，调用方据此决定要不要刷新画面。
    pub fn forget_session(&mut self, source: &str, thread_id: &str) -> bool {
        let before = self.tasks.len();
        self.tasks.retain(|t| {
            !(t.source == source && t.event.thread_id.as_deref() == Some(thread_id))
        });
        if self.tasks.len() != before {
            // 选中项可能指向被移除的任务，留着会让翻页行为悬空。
            let selected_gone = self
                .selected
                .as_ref()
                .is_some_and(|key| self.tasks.iter().all(|t| &t.key() != key));
            if selected_gone {
                self.selected = None;
            }
            true
        } else {
            false
        }
    }

    /// 按 task_id 查当前状态（花絮轮换判定用）。
    fn status_of(&self, task_id: &str, source: &str) -> Status {
        self.tasks
            .iter()
            .find(|t| t.source == source && t.event.task_id == task_id)
            .map(|t| t.status())
            .unwrap_or(Status::Unknown)
    }

    /// 把推送时刻从 DeepSeek Harness 界面读到的真状态写上屏。
    ///
    /// 只保留拟人短句本身（去掉「准备中」前缀，超长截断），返回画面是否变了。
    /// 读界面的活由 `uia` 模块负责，这里只做落库判定，便于单测。
    pub fn set_live_quip(&mut self, quip: &str) -> bool {
        let quip = fit_quip(quip.trim());
        // 与池里短语同一标准：至少像个短句，别把空白/分隔符顶上去。
        if quip.chars().count() < 4 || !quip.chars().any(|c| c.is_alphabetic()) {
            return false;
        }
        if self.quip == quip {
            return false;
        }
        self.quip = quip;
        true
    }
    pub fn visible(&self, now: i64) -> Vec<Task> {
        let mut tasks: Vec<_> = self
            .tasks
            .iter()
            .filter(|t| t.source != "dsh" || self.settings.follow_enabled)
            .cloned()
            .collect();
        tasks.sort_by_key(|t| std::cmp::Reverse((t.updated_at, t.event.seq)));
        for t in &mut tasks {
            // 等待用户处理的这一条不算「陈旧」：它不会自己往前走，但它仍然有效——
            // 屏幕要一直停在「等你确认」，不能过 15 分钟就变成「状态待确认」，
            // 更不能因此被交还设备。陈旧只用来降级那些没人管的工作态。
            if t.event.status != Status::Waiting
                && now.saturating_sub(t.updated_at) > self.settings.stale_minutes as i64 * 60
            {
                t.stale = true;
            }
            // 会话自己的回合结束了，但它的子智能体还在跑（DSH 侧边栏显示
            // 「N 个子智能体运行中」）：仍按「工作中」显示，不能当成完成。
            let sid = t
                .event
                .thread_id
                .as_deref()
                .unwrap_or(t.event.task_id.as_str());
            t.subagent_busy = self.team_busy.contains_key(sid)
                || self
                    .team_busy
                    .keys()
                    .any(|parent| sid.starts_with(parent.as_str()));
            if t.subagent_busy {
                // 队友在跑：当作「刚刚有活动」，否则会被陈旧判定压成待确认，
                // 释放判定也会误以为「没有新记录」而把设备交还。
                t.stale = false;
                t.updated_at = t.updated_at.max(now);
            }
        }
        tasks
    }
    /// 每个聊天只保留最新一轮；旧的未结束轮次不得并排显示为并行工作。
    pub fn live(&self, now: i64) -> Vec<Task> {
        let mut seen = std::collections::HashSet::new();
        self.visible(now)
            .into_iter()
            .filter(|t| {
                if t.source == "dsh" {
                    if let Some(id) = &t.event.thread_id {
                        if !seen.insert(id.clone()) {
                            return false;
                        }
                    }
                }
                true
            })
            .collect()
    }
    pub fn running(&self, now: i64) -> Vec<Task> {
        let mut tasks: Vec<_> = self
            .live(now)
            .into_iter()
            .filter(|t| matches!(t.status(), Status::Working | Status::Waiting))
            .collect();
        // Heartbeats and waiting transitions must not move a task between rows.
        tasks.sort_by(|a, b| {
            (
                &a.source,
                a.event.thread_id.as_ref().unwrap_or(&a.event.task_id),
            )
                .cmp(&(
                    &b.source,
                    b.event.thread_id.as_ref().unwrap_or(&b.event.task_id),
                ))
        });
        tasks
    }
    /// 自动跟随「该显示的那一条」：已经上屏的任务只要还活着就不换人。
    ///
    /// 两个会话并行时（比如这边在跑、另一个项目也在跑），每来一条新记录
    /// `updated_at` 都会往后走，若每帧重算「最新的那个」，主卡就会几秒一换，
    /// 水墨屏跟着反复刷新。这里只在当前这条结束、消失时才重新挑一条。
    pub fn auto_focus(&mut self, now: i64) {
        let active = self.running(now);
        if active.is_empty() || self.manual_page.is_some() {
            return;
        }
        if self.selected.as_ref().is_some_and(|key| {
            active
                .iter()
                .any(|task| task.key() == *key && task.status() == Status::Waiting)
        }) {
            return;
        }
        let best = active
            .iter()
            .filter(|t| t.status() == Status::Working)
            .max_by_key(|t| t.updated_at)
            .or_else(|| active.iter().max_by_key(|t| t.updated_at));
        let Some(best) = best else {
            return;
        };
        if let Some(key) = &self.selected {
            if let Some(cur) = active.iter().find(|t| &t.key() == key) {
                // 只有新任务明显更「热」才换人：两个会话交替写记录时，
                // 时间差只有几秒，换人就是来回闪屏。
                if best.updated_at.saturating_sub(cur.updated_at) <= FOCUS_HYSTERESIS {
                    return;
                }
            }
        }
        self.selected = Some(best.key());
    }

    pub fn current(&self, now: i64) -> Option<Task> {
        if let Some(status) = self.demo {
            return Some(Task {
                event: Event {
                    task_id: "demo".into(),
                    thread_id: None,
                    seq: 1,
                    status,
                    waiting_kind: None,
                    title: "整理今天的待办".into(),
                    detail: "状态演示，不代表真实任务".into(),
                    progress: if status == Status::Working {
                        Some(60)
                    } else {
                        None
                    },
                    progress_source: ProgressSource::Agent,
                    progress_steps: None,
                    agent: "演示".into(),
                },
                source: "demo".into(),
                updated_at: now,
                stale: false,
                subagent_busy: false,
                waiting_order: 0,
            });
        }
        let active = self.running(now);
        let tasks = if active.len() > 1 {
            active
        } else {
            self.visible(now)
        };
        if let Some(key) = &self.selected {
            if let Some(t) = tasks.iter().find(|t| &t.key() == key) {
                return Some(t.clone());
            }
        }
        let tasks = self.live(now);
        // 有人正在干活就显示它（最新的那一轮）；没人干活时才让位给等待确认的。
        // 旧顺序把 Waiting 无条件排在 Working 前面，一条几分钟前就处理完的等待
        // 会长期霸屏，看起来像「宠物不跟了」——实测设备一直停在别的会话的
        // 「等你处理」，而这边正常在跑。
        tasks
            .iter()
            .filter(|t| t.status() == Status::Working)
            .max_by_key(|t| t.updated_at)
            .or_else(|| {
                tasks
                    .iter()
                    .filter(|t| t.status() == Status::Waiting)
                    .max_by_key(|t| t.updated_at)
            })
            .or_else(|| tasks.first())
            .cloned()
    }
    /// 当前任务是否需要用户回电脑处理（等待选择）。
    pub fn needs_action(&self, now: i64) -> bool {
        self.current(now)
            .map(|t| t.status() == Status::Waiting)
            .unwrap_or(false)
    }
    /// 设备上的显示顺序：Waiting 永远置顶（最该被看见的是「有事等你处理」），
    /// 其余按最近更新时间倒序。翻页与渲染必须用同一个顺序，否则按键会「翻回原页」。
    pub fn display_order(&self, now: i64) -> Vec<Task> {
        let mut tasks = self.running(now);
        tasks.sort_by(|a, b| {
            match (a.status() == Status::Waiting, b.status() == Status::Waiting) {
                (true, true) => b
                    .waiting_order
                    .cmp(&a.waiting_order)
                    .then_with(|| a.key().cmp(&b.key())),
                (true, false) => std::cmp::Ordering::Less,
                (false, true) => std::cmp::Ordering::Greater,
                (false, false) => b
                    .updated_at
                    .cmp(&a.updated_at)
                    .then_with(|| a.key().cmp(&b.key())),
            }
        });
        tasks
    }

    pub fn cycle(&mut self, step: i32, now: i64) {
        self.demo = None;
        // 单个活跃任务时上下键无事可做：翻到历史任务只会让人困惑。
        let active = self.display_order(now);
        if active.len() <= 1 {
            self.manual_page = None;
            return;
        }
        if active.len() > TASKS_PER_PAGE {
            let pages = active.len().div_ceil(TASKS_PER_PAGE);
            let page = self
                .manual_page
                .unwrap_or_else(|| {
                    self.selected
                        .as_ref()
                        .and_then(|key| active.iter().position(|t| &t.key() == key))
                        .unwrap_or(0)
                        / TASKS_PER_PAGE
                })
                .min(pages.saturating_sub(1));
            let next = (page as i32 + step).rem_euclid(pages as i32) as usize;
            self.manual_page = Some(next);
            self.selected = Some(active[next * TASKS_PER_PAGE].key());
            return;
        }
        self.manual_page = None;
        let tasks = active;
        if tasks.is_empty() {
            self.selected = None;
            return;
        }
        let key = self.current(now).map(|t| t.key());
        let idx = tasks.iter().position(|t| Some(t.key()) == key).unwrap_or(0) as i32;
        self.selected = Some(tasks[(idx + step).rem_euclid(tasks.len() as i32) as usize].key());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn ev(seq: u64, status: Status) -> Event {
        Event {
            task_id: "test".into(),
            thread_id: None,
            seq,
            status,
            waiting_kind: (status == Status::Waiting).then_some(WaitingKind::Question),
            title: "真实任务".into(),
            detail: String::new(),
            progress: None,
            progress_source: ProgressSource::Agent,
            progress_steps: None,
            agent: "Agent".into(),
        }
    }
    #[test]
    fn rebuilds_legacy_cache_once_without_losing_agent_tasks_or_settings() {
        let mut m = Model::default();
        m.settings.follow_enabled = true;
        m.settings.volume = 35;
        m.apply(ev(1, Status::Stopped), "dsh", 100).unwrap();
        m.apply(ev(1, Status::Done), "agent", 100).unwrap();
        assert!(m.migrate_cache());
        assert_eq!(m.tasks.len(), 1);
        assert_eq!(m.tasks[0].source, "agent");
        assert!(m.settings.follow_enabled);
        assert_eq!(m.settings.volume, 35);
        m.apply(ev(2, Status::Working), "dsh", 101).unwrap();
        let mut restored: Model = serde_json::from_slice(&serde_json::to_vec(&m).unwrap()).unwrap();
        assert!(!restored.migrate_cache());
        assert_eq!(restored.tasks.len(), 2);
    }
    #[test]
    fn running_shows_multiple_chats_but_not_old_turns_or_stale_work() {
        let mut m = Model::default();
        m.settings.follow_enabled = true;
        let mut e = ev(1, Status::Working);
        e.thread_id = Some("chat-a".into());
        m.apply(e.clone(), "dsh", 10).unwrap();
        e.task_id = "new-turn".into();
        e.seq = 2;
        e.status = Status::Done;
        m.apply(e.clone(), "dsh", 10).unwrap();
        assert!(m.running(11).is_empty());
        e.task_id = "other".into();
        e.thread_id = Some("chat-b".into());
        e.status = Status::Working;
        m.apply(e.clone(), "dsh", 12).unwrap();
        e.task_id = "third".into();
        e.thread_id = Some("chat-c".into());
        e.status = Status::Waiting;
        m.apply(e, "dsh", 13).unwrap();
        assert_eq!(m.running(14).len(), 2);
        assert_eq!(m.running(14)[0].event.task_id, "other");
        assert_eq!(m.running(14)[1].status(), Status::Waiting);
        // 有会话在跑就显示在跑的那条；等待确认的排它后面。
        assert_eq!(m.current(14).unwrap().event.task_id, "other");
        assert_eq!(m.current(14).unwrap().status(), Status::Working);
        m.selected = Some("dsh:new-turn".into());
        assert_eq!(m.current(14).unwrap().status(), Status::Working);
        let mut update = m.running(14)[0].event.clone();
        update.seq += 1;
        update.status = Status::Waiting;
        m.apply(update, "dsh", 15).unwrap();
        assert_eq!(m.running(15)[0].event.task_id, "other");
        assert!(m.running(2000).len() == 2);
        // 等待确认不会自己过期：20 分钟后仍留在列表里——屏幕必须一直显示它，
        // 直到有人处理或这一轮真正结束。
        assert!(m.running(2000).iter().all(|t| t.status() == Status::Waiting));
    }
    #[test]
    fn auto_focus_keeps_the_card_still_until_it_ends() {
        // 两个会话并行时，谁写一条新记录谁就「最新」。如果每帧重算，主卡会
        // 几秒一换、水墨屏被反复刷白；已经上屏的那条只要还在跑就不该换。
        let mut m = Model::default();
        m.settings.follow_enabled = true;
        let mut a = ev(1, Status::Working);
        a.task_id = "a".into();
        a.thread_id = Some("ta".into());
        m.apply(a, "dsh", 100).unwrap();
        m.auto_focus(100);
        assert_eq!(m.selected.as_deref(), Some("dsh:a"));

        let mut b = ev(1, Status::Working);
        b.task_id = "b".into();
        b.thread_id = Some("tb".into());
        m.apply(b, "dsh", 110).unwrap();
        m.auto_focus(110);
        assert_eq!(
            m.current(110).unwrap().event.task_id,
            "a",
            "已经上屏的任务还在跑，不许因为别人更新就换"
        );

        // A 结束：主卡换到还在跑的 B。
        let mut done = ev(2, Status::Done);
        done.task_id = "a".into();
        done.thread_id = Some("ta".into());
        m.apply(done, "dsh", 120).unwrap();
        m.auto_focus(120);
        assert_eq!(m.current(120).unwrap().event.task_id, "b");
    }

    #[test]
    fn a_live_task_beats_an_older_waiting_one() {
        // 实测：A 会话几分钟前弹过确认（后来已放行），B 会话正在跑；
        // 旧逻辑无条件优先 Waiting，设备就一直停在 A 的「等你处理」上，
        // 看起来像「宠物不跟了」。有人干活时必须显示干活的。
        let mut m = Model::default();
        m.settings.follow_enabled = true;
        let mut waiting = ev(1, Status::Waiting);
        waiting.task_id = "old".into();
        waiting.thread_id = Some("chat-a".into());
        waiting.detail = "有写入操作等你确认 · 请回电脑点允许".into();
        m.apply(waiting, "dsh", 100).unwrap();

        let mut working = ev(2, Status::Working);
        working.task_id = "live".into();
        working.thread_id = Some("chat-b".into());
        m.apply(working, "dsh", 200).unwrap();
        assert_eq!(m.current(210).unwrap().event.task_id, "live");

        // 没人干活时才轮到等待确认的那条——它才是需要人处理的。
        let mut done = ev(3, Status::Done);
        done.task_id = "live".into();
        done.thread_id = Some("chat-b".into());
        m.apply(done, "dsh", 300).unwrap();
        assert_eq!(m.current(310).unwrap().event.task_id, "old");
        assert_eq!(m.current(310).unwrap().status(), Status::Waiting);
    }

    #[test]
    fn waiting_order_survives_heartbeats_and_new_waits_move_first() {
        let mut model = Model::default();
        let mut old = ev(1, Status::Waiting);
        old.task_id = "old-wait".into();
        model.apply(old, "agent", 100).unwrap();

        let mut new = ev(1, Status::Waiting);
        new.task_id = "new-wait".into();
        model.apply(new, "agent", 200).unwrap();

        let mut heartbeat = ev(2, Status::Waiting);
        heartbeat.task_id = "old-wait".into();
        model.apply(heartbeat, "agent", 300).unwrap();

        let order: Vec<_> = model
            .display_order(301)
            .into_iter()
            .map(|task| task.event.task_id)
            .collect();
        assert_eq!(order, ["new-wait", "old-wait"]);
    }

    #[test]
    fn reentering_waiting_moves_task_ahead_of_existing_waits() {
        let mut model = Model::default();
        let mut first = ev(1, Status::Waiting);
        first.task_id = "first".into();
        model.apply(first, "agent", 100).unwrap();

        let mut other = ev(1, Status::Waiting);
        other.task_id = "other".into();
        model.apply(other, "agent", 200).unwrap();

        let mut resumed = ev(2, Status::Working);
        resumed.task_id = "first".into();
        model.apply(resumed.clone(), "agent", 300).unwrap();
        resumed.seq += 1;
        resumed.status = Status::Waiting;
        model.apply(resumed, "agent", 400).unwrap();

        let order: Vec<_> = model
            .display_order(401)
            .into_iter()
            .map(|task| task.event.task_id)
            .collect();
        assert_eq!(order, ["first", "other"]);
    }

    #[test]
    fn legacy_cache_without_wait_metadata_remains_readable() {
        let model: Model = serde_json::from_str(
            r#"{"cache_version":3,"tasks":[{"event":{"task_id":"legacy","seq":1,"status":"waiting","title":"旧缓存"},"source":"agent","updated_at":100,"stale":false}]}"#,
        )
        .unwrap();

        let task = &model.tasks[0];
        assert_eq!(task.event.waiting_kind, None);
        assert_eq!(task.waiting_order, 0);
        assert_eq!(task.waiting_label(), "等你处理");
        assert_eq!(model.display_order(101)[0].event.task_id, "legacy");
    }

    #[test]
    fn page_navigation_clamps_to_the_visible_page_after_tasks_disappear() {
        let mut model = Model::default();
        for index in 0..10 {
            let mut event = ev(1, Status::Working);
            event.task_id = format!("page-{index}");
            model.apply(event, "agent", 100 + index).unwrap();
        }

        for _ in 0..3 {
            model.cycle(1, 200);
        }
        assert_eq!(model.manual_page, Some(3));

        model.tasks.retain(|task| task.event.task_id != "page-0");
        let expected = model.display_order(200)[0].key();
        model.cycle(1, 200);

        assert_eq!(model.manual_page, Some(0));
        assert_eq!(model.selected.as_deref(), Some(expected.as_str()));
    }

    #[test]
    fn terminal_task_revives_on_later_nonterminal_event() {
        let mut m = Model::default();
        // 真实场景：上一轮的 completed 记录延迟落盘，排在新一轮 user 之后，
        // 同一 task 先被判完成；随后的工具活动必须能把任务复活。
        assert!(m.apply(ev(1, Status::Working), "dsh", 10).unwrap());
        assert!(m.apply(ev(2, Status::Done), "dsh", 11).unwrap());
        assert!(m.apply(ev(3, Status::Working), "dsh", 12).unwrap());
        assert_eq!(m.current(13).unwrap().status(), Status::Working);
        // 终态改终态仍然拒绝
        assert!(m.apply(ev(4, Status::Error), "dsh", 13).unwrap());
        assert!(m.apply(ev(5, Status::Done), "dsh", 14).is_err());
    }
    #[test]
    fn ordered_idempotent_and_terminal() {
        let mut m = Model::default();
        assert!(m.apply(ev(2, Status::Working), "agent", 10).unwrap());
        assert!(!m.apply(ev(1, Status::Done), "agent", 11).unwrap());
        assert!(m.apply(ev(3, Status::Done), "agent", 12).unwrap());
        // 完成态之后回到工作态必须复活（补写记录场景），再报完成则拒绝。
        assert!(m.apply(ev(4, Status::Working), "agent", 13).unwrap());
        assert!(m.apply(ev(5, Status::Done), "agent", 14).unwrap());
        assert!(m.apply(ev(6, Status::Stopped), "agent", 15).is_err());
        assert_eq!(m.current(16).unwrap().status(), Status::Done);
    }
    #[test]
    fn stale_never_becomes_success_and_unknown_progress_stays_unknown() {
        let mut m = Model::default();
        m.apply(ev(1, Status::Working), "agent", 10).unwrap();
        let t = m.current(1000).unwrap();
        assert_eq!(t.status(), Status::Unknown);
        assert_eq!(t.event.progress, None);
        m.apply(ev(2, Status::Done), "agent", 1001).unwrap();
        assert_eq!(m.current(3000).unwrap().status(), Status::Done);
    }
    #[test]
    fn sources_are_isolated_and_demo_is_not_persisted() {
        let mut m = Model::default();
        m.settings.follow_enabled = false;
        m.apply(ev(1, Status::Working), "dsh", 10).unwrap();
        assert!(m.current(11).is_none());
        m.apply(ev(1, Status::Waiting), "agent", 11).unwrap();
        m.demo = Some(Status::Done);
        assert_eq!(m.current(12).unwrap().source, "demo");
        let saved = serde_json::to_string(&m).unwrap();
        let restored: Model = serde_json::from_str(&saved).unwrap();
        assert_eq!(restored.current(12).unwrap().status(), Status::Waiting);
    }

    #[test]
    fn forget_session_removes_every_turn_and_releases_the_device() {
        let mut m = Model::default();
        let mut e = ev(1, Status::Working);
        e.thread_id = Some("chat-a".into());
        m.apply(e.clone(), "dsh", 10).unwrap();
        e.task_id = "turn-2".into();
        e.seq = 2;
        m.apply(e, "dsh", 11).unwrap();
        let mut other = ev(3, Status::Working);
        other.thread_id = Some("chat-b".into());
        m.apply(other, "dsh", 12).unwrap();
        m.selected = Some("dsh:turn-2".into());

        assert!(m.forget_session("dsh", "chat-a"));
        assert!(
            m.tasks.iter().all(|t| t.event.thread_id.as_deref() != Some("chat-a")),
            "该会话的所有轮次都要退场"
        );
        assert_eq!(m.selected, None, "指向被移除任务的选中项必须清掉");
        assert_eq!(m.current(13).unwrap().event.thread_id.as_deref(), Some("chat-b"));
        assert!(!m.forget_session("dsh", "chat-a"), "重复清理不应再报变化");
        assert!(!m.forget_session("dsh", "chat-x"), "不存在的会话不算变化");
    }

    #[test]
    fn live_quip_wins_but_still_fits_one_line() {
        let mut m = Model::default();
        assert!(m.set_live_quip("你在思考大局，我在打磨细节"));
        assert_eq!(m.quip, "你在思考大局，我在打磨细节");
        // 同一条重复采到不算变化，否则同一帧会被反复推。
        assert!(!m.set_live_quip("你在思考大局，我在打磨细节"));
        // 界面花絮比内置池长，超宽要截断到 20 字（末位省略号），
        // 不能顶到左边的上下文占用标签上。
        assert!(m.set_live_quip("这是一条特别特别特别特别特别特别长的界面花絮短语"));
        assert_eq!(m.quip.chars().count(), 20);
        assert!(m.quip.ends_with('…'));
        // 只有分隔符/空白时不采信，保留原来的句子。
        let kept = m.quip.clone();
        assert!(!m.set_live_quip("  ·· "));
        assert_eq!(m.quip, kept);
    }
}
