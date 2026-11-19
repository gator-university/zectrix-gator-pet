//! 本机 DeepSeek Harness 跟随：只读 `~/.dsh/sessions/<项目>/<会话目录>/session.v4.jsonl.zstd`。
//!
//! 这是对本机会话日志格式的适配，不是 DSH 的稳定状态 API。默认只提取生命周期
//! 元数据（轮次开始/结束、结构化提问、权限确认、待办清单条目、会话标题、上下文
//! 占用），不读取、不保存、不上传任何对话正文、推理内容、工具参数或代码。
//!
//! 唯一的例外是显式设置的 `title_source=prompt`：会话没有 `session/title` 时，
//! 取首条用户消息的首行当作兜底标题（会写进 state.json 与设备），并在等待提问
//! 时显示题干。两条路径都必须由用户显式开启；默认 `session` 模式下用户消息正文
//! 与提问题干一律不读取、不上屏。
//!
//! 状态映射（全部来自明确记录，不做推测）：
//! - `turn/start` → 正在工作（DSH 每轮开始都写一条，是唯一的轮次起点）
//! - 未答复的 `ask_user_question` 工具调用 → 等你回答
//! - 未处理完的 `approval/asked` → 等你确认（权限卡）
//! - `turn/end` `reason.kind=completed` → 任务完成
//! - `turn/end` `reason.kind=aborted` / 被中断的助手消息 → 已停止
//! - 失败轮次 → 遇到问题
//! - 超过 `stale_minutes` 无更新 → 由模型层降级为「状态待确认」，绝不判成功
//!
//! 普通文本提问、失败的工具调用、未知工具都不会被猜成等待或失败。
//!
//! ## 文件格式
//!
//! 每个会话目录里有一份 `session.v4.jsonl.zstd`：**逐帧追加**的 zstd 流，每一帧
//! 解码后是一段或多段 JSON Lines，每行一条事件记录：
//!
//! ```text
//! {"type":"turn/start","seq":4,"time":1794747174905,"data":{"turn":1}}
//! {"type":"tool/call","seq":22,"time":...,"data":{"callId":"call_00_…","name":"pwsh",…}}
//! {"type":"turn/end","seq":536,"time":...,"data":{"turn":1,"reason":{"kind":"completed"}}}
//! ```
//!
//! 尾读按「完整帧」推进：解不出完整帧的尾巴留到下次，绝不丢字节；文件被重建
//! （变短）时从头重新推导。记录自带单调递增的 `seq`，直接用它做事件序号——跨进程
//! 重启、跨补写都稳定，不需要自己数字节偏移。

use crate::model::{clean, Event, ProgressSource, Status, WaitingKind};
use anyhow::Result;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{Cursor as IoCursor, Read};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// 单文件首次读取上限：避免启动时阻塞在历史超大文件上。
/// 只读尾部意味着首帧可能从半个 zstd 帧开始——`decode_frames` 会跳过第一个魔数
/// 之前无法解码的字节，因此尾部截断是安全的。
const MAX_INITIAL: u64 = 8 * 1024 * 1024;
/// 最多跟踪的会话数，与上游一致。
const MAX_FILES: usize = 24;

fn sort_session_files(files: &mut [(PathBuf, i64)]) {
    files.sort_by(|a, b| {
        b.1.cmp(&a.1)
            .then_with(|| a.0.cmp(&b.0))
    });
}
/// 只有最近活跃的会话才在首轮导入时上屏，避免历史任务堆满列表。
const IMPORT_WINDOW: i64 = 12 * 3600;
/// 「完成」要静默这么久才上报：流式落盘时 turn/end 之后可能紧跟下一轮，
/// 太早认完成会让墨水屏来回闪。
const DONE_SETTLE: i64 = 8;
/// zstd 帧魔数（小端 0xFD2FB528）。
const MAGIC: [u8; 4] = [0x28, 0xB5, 0x2F, 0xFD];

/// 单次解压输出的硬上限。日志是逐帧追加的，正常帧只有几十 KB；这个上限只
/// 用来挡住「高压缩比 / 恶意构造」的帧把内存打爆，超出部分直接截断。
const MAX_DECODED_BYTES: u64 = 64 * 1024 * 1024;

/// `decode_all_frames` 最多消费的帧数：再多的历史日志也不值得整份解开。
const MAX_FRAMES: usize = 4096;

/// 截断只警告一次：守护每拍都可能重读，刷日志没有意义。
static DECODE_TRUNCATED_WARNED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

fn warn_decode_truncated() {
    if !DECODE_TRUNCATED_WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        eprintln!("[dsh] 会话日志过大，解压结果已截断（单次上限 64MB / 4096 帧）");
    }
}
/// 尾巴连续多少拍解不开就跳过：正常追加每拍都会增长；连续不动还解不开，
/// 说明写进程崩在半帧上，后面的帧必须靠跳过它才能接上。
const STALL_SKIP: u32 = 3;
/// 没有标题时用项目目录名，最多这么长。
const TITLE_MAX: usize = 60;

/// 文件尾部记录的类型：状态由「最后一条生命周期记录」决定。
#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum Last {
    #[default]
    None,
    Working,
    Done,
    Stopped,
    Error,
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn mtime(path: &Path) -> i64 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 会话 ID 里不允许出现的字符换成 `-`（Event.task_id / thread_id 只接受
/// 字母数字与 `-_:.`）。
fn safe_id(raw: &str) -> String {
    raw.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | ':' | '.') {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// 会话目录名 → 项目显示名。DSH 把 cwd 编码成 `--C-Users-x-Proj--` 这种形式，
/// 但会话头里带原始 `cwd`，优先用它的末段。读不到时退回目录名。
fn project_name(cwd: &str, dir: &Path) -> String {
    let from_cwd = Path::new(cwd)
        .file_name()
        .and_then(|x| x.to_str())
        .filter(|x| !x.is_empty());
    let name = from_cwd
        .or_else(|| dir.file_name().and_then(|x| x.to_str()))
        .unwrap_or("本机任务");
    clean(name, TITLE_MAX)
}

/// 工具名 → 中文活动描述。只看工具名元数据，不读参数、不读正文。
/// reasoning 不改活动——它太频繁，会让画面在「思考」和「执行」之间反复闪。
fn activity_label(name: &str) -> String {
    let n = name.to_ascii_lowercase().replace(['_', '-'], "");
    let label = match n.as_str() {
        "read" | "view" | "readfile" | "openfile" | "readimage" | "list" | "ls" => "正在阅读文件",
        "edit" | "write" | "multiedit" | "strreplace" | "createfile" | "editfile"
        | "writefile" | "applypatch" | "patch" => "正在修改文件",
        "bash" | "powershell" | "shell" | "cmd" | "pwsh" | "runcommand" | "executecommand"
        | "terminal" => "正在执行命令",
        "grep" | "glob" | "search" | "findfile" | "searchfiles" => "正在搜索代码",
        "websearch" | "webfetch" | "browser" | "fetchurl" => "正在查阅资料",
        "todowrite" | "taskcreate" | "taskupdate" | "tasklist" | "teamtaskcreate"
        | "teamtaskupdate" | "teamtasklist" | "teamtaskget" | "creategoal" | "updategoal"
        | "getgoal" => "正在安排任务",
        "skill" => "正在调用技能",
        "subagent" | "agentsubagent" | "subagentfork" | "workflow" | "spawnagent"
        | "spawnteammate" | "interruptagent" | "listagents" | "waitagent" => "正在委派子任务",
        "sendmessage" | "messagecolleague" | "team" | "speakinchannel" => "正在协作沟通",
        "present" | "deliverable" => "正在整理交付物",
        "schedulecreate" | "scheduleedit" | "schedulelist" | "scheduleview" => "正在安排定时任务",
        "pluginmanager" | "installbundle" => "正在管理插件",
        "joboutput" | "joblist" | "jobkill" => "正在查看后台任务",
        "loadworkspacedependencies" => "正在准备环境",
        _ => "正在处理任务",
    };
    label.to_string()
}

/// `ask_user_question` 的参数：只取第一问的题干（用户明确要求知道为什么要回电脑）。
/// **只在 `title_source=prompt` 时调用**；默认模式下提问题干根本不进内存，
/// 等待画面固定显示「有 1 个问题待确认」。即便调用也只取题干并设长度上限，
/// 选项/描述/正文一律不读。
fn ask_text(args: &str) -> String {
    let Ok(v) = serde_json::from_str::<Value>(args) else {
        return String::new();
    };
    let q = &v["questions"][0];
    clean(q["question"].as_str().unwrap_or("").trim(), 60)
}

/// 任务清单写入工具：`todo_write`。
fn is_todo_write(name: &str) -> bool {
    name.to_ascii_lowercase().replace('_', "") == "todowrite"
}

/// 这些工具代表 Agent 正在等人做选择（DSH 里提问工具叫 `ask_user_question`，
/// 老会话里出现过 `question`，都算）。
fn is_ask_tool(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().replace(['-', '_'], "").as_str(),
        "askuserquestion" | "question"
    )
}

/// 从一条用户消息里取「这一问问了什么」：第一行非空文本，限长。
/// **只在 `title_source=prompt` 时调用**，默认模式下用户消息正文不参与标题。
fn user_title(data: &Value) -> Option<String> {
    let text = data["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|p| p["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let line = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with('<'))?;
    let t = clean(line, 36);
    if t.trim().is_empty() {
        None
    } else {
        Some(t)
    }
}

/// 记录时间（毫秒）→ 秒；拿不到就用文件修改时间兜底。
fn record_time(v: &Value, fallback: i64) -> i64 {
    v["time"]
        .as_i64()
        .map(|ms| ms / 1000)
        .filter(|s| *s > 0)
        .unwrap_or(fallback)
}

#[derive(Default)]
struct Cursor {
    /// 已消费到的文件字节偏移（= `tail` 之前的全部完整帧）。
    offset: u64,
    /// 尚未组成完整帧的尾巴，留到下次拼接。
    tail: Vec<u8>,
    /// 尾巴连续解不开的拍数（见 `STALL_SKIP`）。
    stall: u32,
    /// 会话头里的 id / cwd / delegationDepth。
    id: String,
    cwd: String,
    /// 子代理会话（delegationDepth > 0）：不上屏、不占设备。
    child: bool,
    title: String,
    turn_title: Option<String>,
    turn: u32,
    task: Option<String>,
    last: Last,
    /// 最近一次工具活动的中文描述（只来自工具名）。
    activity: String,
    /// 提问题干（等待解除后不再使用）。
    asking: String,
    /// callId / 确认 id → 等待种类；拿到结果或确认结果即移除。
    waiting: HashMap<String, WaitingKind>,
    /// 本轮待办清单：序号 → 是否已完成。只存完成状态，正文一概不保存。
    todos: BTreeMap<usize, bool>,
    ctx_used: Option<u32>,
    ctx_window: Option<u32>,
    emitted: Option<(Status, String, Option<u8>, String)>,
    /// 本会话第一条记录的时间（秒）：同一项目里按它排「第 N 轮」。
    started_at: i64,
    seen_at: i64,
    seen_seq: u64,
}

impl Cursor {
    fn feed(&mut self, v: &Value, at: i64, stem: &str, allow_prompt: bool) {
        if self.started_at == 0 && at > 0 {
            self.started_at = at;
        }
        let data = &v["data"];
        match v["type"].as_str().unwrap_or("") {
            "session" => {
                // 会话头是**唯一不带 `data` 包装**的记录：字段直接在顶层。
                let id = data["id"].as_str().or_else(|| v["id"].as_str());
                if let Some(id) = id {
                    self.id = id.to_string();
                }
                let cwd = data["cwd"].as_str().or_else(|| v["cwd"].as_str());
                if let Some(cwd) = cwd {
                    self.cwd = cwd.to_string();
                }
                let depth = data["delegationDepth"]
                    .as_i64()
                    .or_else(|| v["delegationDepth"].as_i64())
                    .unwrap_or(0);
                self.child = depth > 0;
            }
            "turn/start" => {
                let turn = data["turn"].as_u64().unwrap_or(0) as u32;
                // 轮次号理论上单调递增；异常数据退回自增，避免两轮撞同一个 task_id。
                self.turn = if turn > self.turn { turn } else { self.turn + 1 };
                let stem = if self.id.is_empty() { stem } else { &self.id };
                self.task = Some(safe_id(&format!("{stem}-t{}", self.turn)));
                self.turn_title = None;
                self.last = Last::Working;
                self.waiting.clear();
                self.todos.clear();
                self.asking.clear();
                self.activity.clear();
                // 新一轮必须重新上屏：去重键不含 task_id，否则同标题同状态的下一轮会被吞掉。
                self.emitted = None;
            }
            "turn/end" => {
                let kind = data["reason"]["kind"].as_str().unwrap_or("");
                self.last = match kind {
                    // 正常收尾；`max-tokens` 是输出被上限截断，`forked` 是会话分叉，
                    // 这一轮都不会再自己推进了，都按「完成」显示。
                    "completed" | "max-tokens" | "forked" => Last::Done,
                    // aborted 的 reason 细分（user/parent/disposed/hook/legacy）只影响日志，
                    // 屏幕上一律是「已停止」；`interrupted` 是进程死后补写的收尾。
                    "aborted" | "interrupted" | "cancelled" | "canceled" => Last::Stopped,
                    // blocked：批处理被 pre-step 监听者拒了，什么都没执行。
                    "blocked" => Last::Stopped,
                    "failed" | "error" => Last::Error,
                    // 未知原因按「还在跑」处理：绝不因为读不懂就判成功。
                    _ => Last::Working,
                };
            }
            "assistant/message" => {
                if data["interrupted"].as_bool().unwrap_or(false) {
                    self.last = Last::Stopped;
                } else if self.last == Last::Working {
                    // 助手消息只是过程中间态：完成与否由 turn/end 说了算。
                }
                if let Some(usage) = data["usage"].as_object() {
                    self.absorb_usage(usage);
                }
            }
            "assistant/attempt" => {
                // 流式 chunk 里带精确的 contextTokens / contextWindow。
                if let Some(stream) = data["stream"].as_array() {
                    for item in stream.iter().rev().take(4) {
                        if let Some(usage) = item["chunk"]["usage"].as_object() {
                            self.absorb_usage(usage);
                        }
                    }
                }
            }
            "tool/call" => {
                let name = data["name"].as_str().unwrap_or("");
                let call_id = data["callId"].as_str().unwrap_or("");
                self.last = Last::Working;
                if is_ask_tool(name) {
                    // 提问不是「活动」：它有自己的等待状态。题干只在 prompt
                    // 模式读取；默认模式连内存都不留。
                    if !call_id.is_empty() {
                        self.waiting
                            .insert(call_id.to_string(), WaitingKind::Question);
                    }
                    if allow_prompt {
                        let text = ask_text(data["arguments"].as_str().unwrap_or(""));
                        if !text.is_empty() {
                            self.asking = text;
                        }
                    }
                } else {
                    self.activity = activity_label(name);
                }
                if is_todo_write(name) && !call_id.is_empty() {
                    // 清单与工具是否已返回无关，参数里就是最新全量清单。
                    if let Some(args) = data["arguments"].as_str() {
                        self.feed_todos(args);
                    }
                }
            }
            "tool/result" => {
                let call_id = data["toolCallId"]
                    .as_str()
                    .or_else(|| data["message"]["toolCallId"].as_str())
                    .unwrap_or("");
                // 提问的「定时模式」会先产出一个 `{"pending":true}` 的回执，但问题
                // 仍然可回答：这种情况不能把等待清掉，要等用户真的答了。
                let still_pending = data["message"]["content"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|c| c["text"].as_str())
                    .any(|t| t.contains("\"pending\":true") || t.contains("\"pending\": true"));
                if !call_id.is_empty() && !still_pending {
                    self.waiting.remove(call_id);
                }
                self.last = Last::Working;
            }
            "approval/asked" => {
                if let Some(id) = data["id"].as_str() {
                    self.waiting
                        .insert(id.to_string(), WaitingKind::Permission);
                }
                self.last = Last::Working;
            }
            "approval/decided" => {
                if let Some(id) = data["id"].as_str() {
                    self.waiting.remove(id);
                }
                self.last = Last::Working;
            }
            "todo/write" => {
                if let Some(items) = data["todos"].as_array() {
                    self.todos = todo_state(items);
                }
            }
            "session/title" => {
                if let Some(t) = data["title"].as_str() {
                    let t = clean(t.trim(), TITLE_MAX);
                    if !t.is_empty() {
                        self.title = t;
                    }
                }
            }
            "request/context" => {
                if let Some(w) = data["contextWindow"].as_i64() {
                    if w > 0 {
                        self.ctx_window = Some(w as u32);
                    }
                }
            }
            "user/message" => {
                // 只有真人输入（source.kind=user）且显式开启 prompt 模式时才配当
                // 兜底标题；审批策略变更、任务通知这些是宿主注入的假 user，不算需求。
                let source_kind = data["source"]["kind"].as_str().unwrap_or("");
                if allow_prompt && source_kind == "user" && self.turn_title.is_none() {
                    self.turn_title = user_title(data);
                }
                // 定时提问的答复：这才是「提问已经有人答了」的权威信号。
                if source_kind == "user-question-reply" {
                    self.waiting
                        .retain(|_, kind| *kind != WaitingKind::Question);
                    self.asking.clear();
                }
            }
            "agent/inbox/spliced" => {
                // 有消息排进「下一轮」：turn/end 之后紧跟排队输入时别急着报完成，
                // 下一轮马上就会开始，屏幕不该先闪一下「完成」再翻回「工作中」。
                let target = data["target"].as_str().unwrap_or("");
                let queued = data["inserted"]
                    .as_array()
                    .is_some_and(|items| !items.is_empty());
                if target == "next-turn" && queued {
                    self.last = Last::Working;
                }
            }
            "compaction/start" | "compaction/end" | "workspace/changes" | "deliverables/presented"
            | "job/registered" | "job/settled" => {
                // 这些只说明还在动，不改状态；`seen_at` 由上层统一刷新。
            }
            _ => {}
        }
        let _ = at;
    }

    /// 吸收一份 token 用量。`inputTokens + cacheReadTokens` 就是这次请求实际
    /// 塞进上下文的 token 数（与 DSH 流式 chunk 里的 `contextTokens` 对得上），
    /// 除以 `contextWindow` 就是左下角那个百分比。
    fn absorb_usage(&mut self, usage: &serde_json::Map<String, Value>) {
        let n = |k: &str| usage.get(k).and_then(Value::as_i64).filter(|v| *v >= 0);
        if let Some(ctx) = n("contextTokens") {
            self.ctx_used = Some(ctx as u32);
        } else if let Some(input) = n("inputTokens") {
            // inputTokens 只是「没命中缓存的那部分」，命中缓存的上下文另记在
            // cacheReadTokens / cacheWriteTokens 里，三者相加才是这次请求的 prompt 量。
            let read = n("cacheReadTokens").unwrap_or(0);
            let write = n("cacheWriteTokens").unwrap_or(0);
            self.ctx_used = Some((input + read + write) as u32);
        }
        if let Some(w) = n("contextWindow").filter(|w| *w > 0) {
            self.ctx_window = Some(w as u32);
        }
    }

    /// `todo/write` 是全量清单：当前完成项 ÷ 总数。取消项不占分母，
    /// 否则清单永远到不了 100%。条目的正文一律不保存、不上屏。
    fn feed_todos(&mut self, args: &str) {
        let Ok(v) = serde_json::from_str::<Value>(args) else {
            return;
        };
        let Some(items) = v["todos"].as_array() else {
            return;
        };
        self.todos = todo_state(items);
    }

    /// 清单完成度：返回 `(百分比, 已完成, 总数)`；Agent 没写过清单就没有值。
    fn progress(&self) -> Option<(u8, u32, u32)> {
        let total = self.todos.len() as u32;
        if total == 0 {
            return None;
        }
        let done = self.todos.values().filter(|d| **d).count() as u32;
        let pct = ((done * 100 + total / 2) / total).min(100) as u8;
        Some((pct, done, total))
    }

    fn status(&self) -> Option<Status> {
        self.task.as_ref()?;
        if self.child {
            return None;
        }
        if !self.waiting.is_empty() {
            return Some(Status::Waiting);
        }
        Some(match self.last {
            Last::Done => Status::Done,
            Last::Stopped => Status::Stopped,
            Last::Error => Status::Error,
            // 轮次已开始、还没有 turn/end：正在工作。
            Last::Working | Last::None => Status::Working,
        })
    }

    /// 是否有权限确认在等（区别于提问），等待画面据此换文案。
    fn waiting_kind(&self) -> Option<WaitingKind> {
        if self.waiting.values().any(|k| *k == WaitingKind::Permission) {
            Some(WaitingKind::Permission)
        } else if self.waiting.values().any(|k| *k == WaitingKind::Question) {
            Some(WaitingKind::Question)
        } else {
            None
        }
    }

    /// 是否可以把当前状态推上去：「完成」要等记录静默，
    /// 工作中/等待中不受影响。
    fn settled(&mut self, immediate: bool) -> bool {
        if self.status() != Some(Status::Done) || immediate {
            return true;
        }
        now_secs().saturating_sub(self.seen_at) >= DONE_SETTLE
    }

    fn event(
        &mut self,
        at: i64,
        seq: u64,
        fallback: &str,
        stem: &str,
        allow_prompt: bool,
    ) -> Option<(Event, i64)> {
        let status = self.status()?;
        let task = self.task.clone()?;
        let title = if !self.title.is_empty() {
            self.title.clone()
        } else if allow_prompt {
            match self.turn_title.as_deref() {
                Some(t) if !t.trim().is_empty() => t.to_string(),
                _ => fallback.to_string(),
            }
        } else {
            // 默认模式：标题只来自 session/title，没有就用「项目名 + 第 N 轮」。
            fallback.to_string()
        };
        let progress = self.progress();
        let waiting_kind = self.waiting_kind();
        let detail = match status {
            Status::Waiting if waiting_kind == Some(WaitingKind::Permission) => {
                "有操作等你确认 · 请回电脑点允许".to_string()
            }
            // 题干只在显式开启 prompt 模式时外传；默认固定文案，不带提问正文。
            Status::Waiting if allow_prompt && !self.asking.is_empty() => self.asking.clone(),
            Status::Waiting => "有 1 个问题待确认".to_string(),
            Status::Working => self.activity.clone(),
            _ => String::new(),
        };
        if let Some((s, t, p, d)) = &self.emitted {
            if *s == status && *t == title && *p == progress.map(|x| x.0) && *d == detail {
                return None;
            }
        }
        self.emitted = Some((status, title.clone(), progress.map(|x| x.0), detail.clone()));
        let _ = stem;
        Some((
            Event {
                task_id: task,
                thread_id: if self.id.is_empty() {
                    None
                } else {
                    Some(self.id.clone())
                },
                seq,
                status,
                waiting_kind,
                title,
                detail,
                progress: progress.map(|x| x.0),
                progress_source: ProgressSource::Todo,
                progress_steps: progress.map(|x| (x.1, x.2)),
                agent: "DeepSeek Harness".into(),
            },
            at,
        ))
    }
}

/// 从 `todo/write` 的条目数组算出「序号 → 是否完成」。
/// 只认状态字符串：completed/done 记完成；cancelled 一类直接不放进分母；
/// pending / in_progress / 未知一律算未完成，不猜。
fn todo_state(items: &[Value]) -> BTreeMap<usize, bool> {
    let mut out = BTreeMap::new();
    for (i, item) in items.iter().enumerate() {
        match item["status"].as_str().unwrap_or("") {
            "cancelled" | "canceled" | "deleted" | "removed" => {}
            "completed" | "done" => {
                out.insert(i, true);
            }
            _ => {
                out.insert(i, false);
            }
        }
    }
    out
}

/// 在一段字节里找 zstd 帧魔数。
fn find_magic(buf: &[u8], from: usize) -> Option<usize> {
    buf.get(from..)?
        .windows(MAGIC.len())
        .position(|w| w == MAGIC)
        .map(|i| i + from)
}

/// 解压整份会话日志（zstd 多帧拼接成一个字节流）。给「子智能体是否还在跑」
/// 这类需要完整读一份日志的调用方用；普通轮询仍然走增量尾巴。
pub fn decode_all_frames(buf: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut pos = 0;
    let mut frames = 0usize;
    while pos < buf.len() {
        if frames >= MAX_FRAMES || out.len() as u64 >= MAX_DECODED_BYTES {
            warn_decode_truncated();
            break;
        }
        let (chunk, used) = decode_frames(&buf[pos..]);
        if used == 0 {
            break;
        }
        let room = (MAX_DECODED_BYTES as usize).saturating_sub(out.len());
        if chunk.len() > room {
            out.extend_from_slice(&chunk[..room]);
            warn_decode_truncated();
            break;
        }
        out.extend_from_slice(&chunk);
        pos += used;
        frames += 1;
    }
    out
}

/// 解一段（可能含多帧的）zstd 数据，最多取 `cap` 字节输出。
///
/// 用 `Read::take` 而不是先 `decode_all` 再截：后者会先把完整输出分配出来，
/// 高压缩比日志（1MB 压缩 → 几 GB 明文）足以把进程打爆。输入不完整时仍然
/// 返回 `Err`，让 `decode_frames` 继续往前退候选。
fn decode_capped(bytes: &[u8], cap: u64) -> std::io::Result<Vec<u8>> {
    let decoder = zstd::Decoder::new(IoCursor::new(bytes))?;
    let mut out = Vec::new();
    decoder.take(cap).read_to_end(&mut out)?;
    Ok(out)
}

/// 解出 `buf` 里**完整** zstd 帧的内容，返回拼接后的字节与已消费长度。
///
/// 逐帧追加的日志里，最后一帧可能正写到一半。做法：先尝试整段解码；失败就
/// 退到「最后一个魔数之前」再试——那一帧就是没写完的尾巴。解码尾巴之前的部分
/// 若还失败（魔数在压缩数据里偶然出现），继续往前退，最多退到起点。
fn decode_frames(buf: &[u8]) -> (Vec<u8>, usize) {
    if buf.is_empty() {
        return (Vec::new(), 0);
    }
    // 首帧之前可能残留半帧（首次只读尾部）：从第一个魔数开始。
    let mut start = 0;
    if !buf.starts_with(&MAGIC) {
        start = find_magic(buf, 0).unwrap_or(buf.len());
        if start >= buf.len() {
            return (Vec::new(), buf.len());
        }
    }
    let mut ends: Vec<usize> = Vec::new();
    let mut pos = start;
    while let Some(found) = find_magic(buf, pos + 1) {
        ends.push(found);
        pos = found;
    }
    ends.push(buf.len());
    // 从最长的候选开始试：正常情况下第一枪（整段）就中。
    for end in ends.iter().rev().copied() {
        if end <= start {
            break;
        }
        if let Ok(decoded) = decode_capped(&buf[start..end], MAX_DECODED_BYTES) {
            if decoded.len() as u64 >= MAX_DECODED_BYTES {
                warn_decode_truncated();
            }
            return (decoded, end);
        }
    }
    // 一帧都解不开：保留尾巴，等更多数据。`start` 之前的残字节直接丢掉
    // （它们只能来自被截断的半帧，永远补不齐）。
    (Vec::new(), start)
}

/// 侧边栏「已归档的会话」：`<profile>/storages/workspace.json` 的
/// `global.archivedSessionIds`。归档只写这份索引，会话日志还完整躺在磁盘上——
/// 不挡掉的话，一条被归档时正卡在提问上的会话会永远算「等你处理」，
/// 把真正在跑的任务挤下屏幕。缓存 15 秒，归档不是高频操作。
fn archived_ids(profile: &Path) -> HashSet<String> {
    use std::time::{Duration, Instant};
    static CACHE: std::sync::OnceLock<std::sync::Mutex<(Instant, HashSet<String>)>> =
        std::sync::OnceLock::new();
    let cache = CACHE.get_or_init(|| {
        std::sync::Mutex::new((Instant::now() - Duration::from_secs(3600), HashSet::new()))
    });
    let Ok(mut guard) = cache.lock() else {
        return HashSet::new();
    };
    if guard.0.elapsed() < Duration::from_secs(15) {
        return guard.1.clone();
    }
    let mut set = HashSet::new();
    if let Ok(bytes) = std::fs::read(profile.join("storages").join("workspace.json")) {
        if let Ok(v) = serde_json::from_slice::<Value>(&bytes) {
            if let Some(ids) = v["global"]["archivedSessionIds"].as_array() {
                for id in ids.iter().filter_map(Value::as_str) {
                    set.insert(id.to_string());
                }
            }
        }
    }
    *guard = (Instant::now(), set.clone());
    set
}

pub struct Watcher {
    root: PathBuf,
    cursors: HashMap<PathBuf, Cursor>,
    deleted: HashSet<String>,
    /// 只看指定会话（会话 id 或目录名）：调试与预览单会话状态用。
    only: Option<String>,
    /// `title_source=prompt`：允许读用户消息首行与提问题干当标题/详情。
    /// 默认 false，标题只来自 `session/title`。
    allow_prompt: bool,
}

impl Watcher {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            cursors: HashMap::new(),
            deleted: HashSet::new(),
            only: None,
            allow_prompt: false,
        }
    }

    /// 显式开启「提示词兜底标题」。由 `Settings::title_source` 决定，默认关闭。
    pub fn prompt_titles(mut self, on: bool) -> Self {
        self.allow_prompt = on;
        self
    }

    /// 每拍同步设置（`set` 改设置后无需重启）。
    pub fn use_prompt_titles(&mut self, on: bool) {
        self.allow_prompt = on;
    }

    /// 测试注入：把某个会话标记成「已删除」。
    pub fn mark_deleted(&mut self, id: &str) {
        let id = id.trim();
        if !id.is_empty() {
            self.deleted.insert(id.to_string());
        }
    }

    /// 已删除的会话：跟随层要把它们从模型里请出去，否则画面会停在旧任务上。
    pub fn deleted(&self) -> &HashSet<String> {
        &self.deleted
    }

    pub fn only(mut self, id: &str) -> Self {
        let id = id.trim();
        if !id.is_empty() {
            self.only = Some(id.to_string());
        }
        self
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 每个会话最后一条已消费记录的时间（秒），按 task_id 索引。
    /// 屏幕状态可能一直没变（同一轮里持续在跑），但日志是新的——占用闸门必须
    /// 据此续期，否则会在任务正忙时到点把设备交还出去。
    pub fn activity(&self) -> HashMap<String, i64> {
        self.cursors
            .values()
            .filter_map(|c| c.task.clone().map(|t| (t, c.seen_at)))
            .collect()
    }

    /// 当前展示会话的上下文占用（已用 / 窗口）。读不到就没有值，
    /// 屏幕上退回「DeepSeek Harness 本机」。
    pub fn context_of(&self, id: &str) -> Option<(u32, u32)> {
        self.cursors
            .values()
            .find(|c| c.id == id)
            .and_then(|c| c.ctx_used.zip(c.ctx_window))
            .filter(|(u, w)| *u > 0 && *w > 0)
    }

    /// 同一项目目录下按会话第一条记录时间排第几轮（从 1 开始）。
    /// 没有会话标题时的兜底就用它，比读用户消息正文安全得多。
    fn rank_in_project(&self, path: &Path) -> u32 {
        let Some(me) = self.cursors.get(path) else {
            return 1;
        };
        if me.started_at <= 0 {
            return 1;
        }
        let parent = path.parent();
        let ahead = self
            .cursors
            .iter()
            .filter(|(other, cursor)| {
                other.as_path() != path
                    && other.parent() == parent
                    && cursor.started_at > 0
                    && cursor.started_at < me.started_at
            })
            .count();
        (ahead + 1) as u32
    }

    /// 默认模式的无标题占位：「项目目录名 第 N 轮」。
    fn fallback_title(&self, path: &Path) -> String {
        let Some(cursor) = self.cursors.get(path) else {
            return "本机任务".to_string();
        };
        let dir = path.parent().unwrap_or(Path::new("."));
        let name = project_name(&cursor.cwd, dir);
        clean(
            &format!("{name} 第{}轮", self.rank_in_project(path)),
            TITLE_MAX,
        )
    }

    /// 扫全部会话，返回本轮新出现的状态事件。
    pub fn scan(&mut self) -> Result<Vec<(Event, i64)>> {
        let now = now_secs();
        let allow_prompt = self.allow_prompt;
        // 侧边栏归档的会话整条忽略：日志还在，但人已经不跟这条任务了。
        // `<profile>/sessions` 的上级就是 profile 根（`~/.dsh`）。
        if let Some(profile) = self.root.parent() {
            self.deleted.extend(archived_ids(profile));
        }
        let deleted = self.deleted.clone();
        self.cursors.retain(|_, c| {
            !deleted.contains(&c.id) && !deleted.contains(c.task.as_deref().unwrap_or(""))
        });
        let files = self.files()?;
        self.cursors
            .retain(|p, _| files.iter().any(|(f, _)| f == p));
        let mut out = vec![];
        for (path, modified) in files {
            let stem = path
                .parent()
                .and_then(|p| p.file_name())
                .and_then(|s| s.to_str())
                .unwrap_or("session")
                .to_string();
            let fresh = !self.cursors.contains_key(&path);
            let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            {
                let cursor = self.cursors.entry(path.clone()).or_default();
                if fresh && size > MAX_INITIAL {
                    // 只读尾部：跳过被截断的半帧由 decode_frames 负责。
                    cursor.offset = size - MAX_INITIAL;
                    cursor.tail.clear();
                }
                if size < cursor.offset {
                    // 文件被截断或重建：状态必须重新推导，不能沿用旧进度。
                    let task = cursor.task.take();
                    *cursor = Cursor {
                        task,
                        ..Cursor::default()
                    };
                }
            }
            // 文件没长大的话这一拍无事可做：不必 open/seek/read（每个会话每秒
            // 一次全量读取是空闲时的主要 syscall 开销）。`tail` 里的半帧留着，
            // 等下次真的追加字节时再一起解。
            if !fresh && size == self.cursors[&path].offset {
                continue;
            }
            let mut chunk = vec![];
            {
                let offset = self.cursors[&path].offset;
                use std::io::{Seek, SeekFrom};
                let mut f = std::fs::File::open(&path)?;
                f.seek(SeekFrom::Start(offset))?;
                f.read_to_end(&mut chunk)?;
            }
            if chunk.is_empty() && !fresh {
                continue;
            }
            // 不变式：`offset` 是「已经读进内存的字节末尾」，`tail` 是其中还没解出
            // 完整帧的尾巴。于是解码起点 = offset - tail.len()，读完后 offset 一律
            // 推进到「起点 + 缓冲长度」——否则下一拍会从尾巴起点重读，把旧字节
            // 拼进新块里（半帧永远解不出来）。
            let carry_len = self.cursors[&path].tail.len();
            let base = self.cursors[&path].offset.saturating_sub(carry_len as u64);
            let mut buf = std::mem::take(&mut self.cursors.get_mut(&path).unwrap().tail);
            buf.extend_from_slice(&chunk);
            let end = base + buf.len() as u64;
            let (decoded, consumed) = decode_frames(&buf);
            {
                let c = self.cursors.get_mut(&path).unwrap();
                c.offset = end;
                if consumed == 0 && !buf.is_empty() {
                    // 半帧还没写完：等下一拍；连续几拍都这样说明写进程崩在半帧上，
                    // 跳过这段坏尾巴，后面的帧才能接上。
                    c.stall += 1;
                    if c.stall >= STALL_SKIP {
                        let skip_to = find_magic(&buf, 1).unwrap_or(buf.len());
                        c.tail = buf[skip_to..].to_vec();
                        c.stall = 0;
                    } else {
                        c.tail = buf;
                    }
                    continue;
                }
                c.stall = 0;
                c.tail = buf[consumed..].to_vec();
            }
            let text = String::from_utf8_lossy(&decoded);
            let stale_import = fresh && now.saturating_sub(modified) > IMPORT_WINDOW;
            let mut last_at = modified;
            let mut last_seq = 0u64;
            for line in text.split('\n') {
                if line.trim().is_empty() {
                    continue;
                }
                let Ok(v) = serde_json::from_str::<Value>(line) else {
                    continue; // 半行或损坏记录直接跳过，不猜状态
                };
                let at = record_time(&v, modified);
                let seq = v["seq"].as_u64().unwrap_or(0);
                last_at = at;
                last_seq = seq.max(last_seq);
                {
                    let c = self.cursors.get_mut(&path).unwrap();
                    c.feed(&v, at, &stem, allow_prompt);
                    c.seen_at = at;
                    c.seen_seq = seq;
                }
                // 增量扫描必须逐条上报：同一批里可能先后出现「完成」和「下一轮开始」，
                // 合并成一个事件会丢掉完成提示音。首次导入是历史回放，只报最终状态。
                if !fresh && !stale_import {
                    let fallback = self.fallback_title(&path);
                    let c = self.cursors.get_mut(&path).unwrap();
                    if c.settled(false) {
                        if let Some(ev) = c.event(at, seq.max(1), &fallback, &stem, allow_prompt) {
                            out.push(ev);
                        }
                    }
                }
            }
            if fresh {
                let fallback = self.fallback_title(&path);
                if stale_import {
                    // 陈旧会话只建立游标，不上屏：避免把几天前的任务当成当前状态。
                    let c = self.cursors.get_mut(&path).unwrap();
                    c.emitted = c.status().map(|s| {
                        (
                            s,
                            fallback.clone(),
                            c.progress().map(|p| p.0),
                            c.activity.clone(),
                        )
                    });
                } else if self
                    .cursors
                    .get_mut(&path)
                    .map(|c| c.settled(true))
                    .unwrap_or(false)
                {
                    let c = self.cursors.get_mut(&path).unwrap();
                    if let Some(ev) =
                        c.event(last_at.max(modified), last_seq.max(1), &fallback, &stem, allow_prompt)
                    {
                        out.push(ev);
                    }
                }
                if !stale_import {
                    let c = self.cursors.get_mut(&path).unwrap();
                    c.seen_at = last_at.max(modified);
                    c.seen_seq = last_seq.max(1);
                }
            }
        }

        // 没有新记录时也要有一次判定：被去抖压住的「完成」只有在静默到期后
        // 才轮得到上报，而上面那个循环在没有新字节时根本不会解码。
        let mut idle: Vec<PathBuf> = self.cursors.keys().cloned().collect();
        idle.sort();
        for path in idle {
            let stem = path
                .parent()
                .and_then(|p| p.file_name())
                .and_then(|s| s.to_str())
                .unwrap_or("session")
                .to_string();
            let fallback = self.fallback_title(&path);
            let Some(c) = self.cursors.get_mut(&path) else {
                continue;
            };
            if c.seen_at <= 0 || c.seen_seq == 0 || !c.settled(false) {
                continue;
            }
            let at = c.seen_at;
            let seq = c.seen_seq;
            if let Some(ev) = c.event(at, seq, &fallback, &stem, allow_prompt) {
                out.push(ev);
            }
        }
        Ok(out)
    }

    /// 收集所有会话日志：`<项目>/<会话目录>/session.v4.jsonl.zstd`。
    /// 优先取最高版本；同一会话目录只认一份。
    fn files(&self) -> Result<Vec<(PathBuf, i64)>> {
        let mut out = vec![];
        let projects = match std::fs::read_dir(&self.root) {
            Ok(rd) => rd,
            Err(e) => {
                anyhow::bail!("未找到 DeepSeek Harness 会话目录 {}: {e}", self.root.display())
            }
        };
        for project in projects.flatten() {
            let proj_dir = project.path();
            if !proj_dir.is_dir() {
                continue;
            }
            for entry in std::fs::read_dir(&proj_dir).into_iter().flatten().flatten() {
                let session_dir = entry.path();
                if !session_dir.is_dir() {
                    continue;
                }
                let dir_name = session_dir
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("")
                    .to_string();
                // 子代理会话的目录是裸 uuid，主会话是 `session-<uuid>`。子代理不在
                // 侧边栏里出现，宠物也不该跟它——它是主会话里的一次委派，不是一条
                // 独立任务。（会话头里的 delegationDepth 再兜一层，双保险。）
                if !dir_name.starts_with("session-") {
                    continue;
                }
                if self.deleted.contains(&dir_name) {
                    continue;
                }
                if let Some(only) = &self.only {
                    if dir_name != *only {
                        continue;
                    }
                }
                // session.v*.jsonl.zstd：取版本号最大的那个。
                let mut best: Option<(u32, PathBuf)> = None;
                for f in std::fs::read_dir(&session_dir).into_iter().flatten().flatten() {
                    let path = f.path();
                    let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
                    let Some(version) = session_version(name) else {
                        continue;
                    };
                    if !path.is_file() {
                        continue;
                    }
                    if best.as_ref().is_none_or(|(v, _)| version > *v) {
                        best = Some((version, path));
                    }
                }
                if let Some((_, path)) = best {
                    out.push((path.clone(), mtime(&path)));
                }
            }
        }
        sort_session_files(&mut out);
        out.truncate(MAX_FILES);
        Ok(out)
    }
}

/// `session.v4.jsonl.zstd` → `Some(4)`；`session.jsonl.zstd`（v0）→ `Some(0)`。
fn session_version(file_name: &str) -> Option<u32> {
    let rest = file_name.strip_prefix("session")?;
    let rest = rest.strip_suffix(".jsonl.zstd").or_else(|| rest.strip_suffix(".jsonl"))?;
    if rest.is_empty() {
        return Some(0);
    }
    rest.strip_prefix(".v")?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Model;
    use serde_json::json;
    use std::io::Write;

    #[test]
    fn session_file_order_is_stable_when_modification_times_match() {
        let mut files = vec![
            (PathBuf::from("session-c"), 100),
            (PathBuf::from("session-b"), 100),
            (PathBuf::from("session-old"), 99),
            (PathBuf::from("session-a"), 100),
        ];

        sort_session_files(&mut files);

        assert_eq!(
            files,
            [
                (PathBuf::from("session-a"), 100),
                (PathBuf::from("session-b"), 100),
                (PathBuf::from("session-c"), 100),
                (PathBuf::from("session-old"), 99),
            ]
        );
    }

    fn temp_root(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("dsppet-{tag}-{nanos}"));
        std::fs::create_dir_all(root.join("--C-demo-proj--").join("session-aaaa")).unwrap();
        root
    }

    fn log_path(root: &Path) -> PathBuf {
        root.join("--C-demo-proj--")
            .join("session-aaaa")
            .join("session.v4.jsonl.zstd")
    }

    /// 追加一帧：真实日志就是「一段 JSONL 压成一帧追加」。
    fn append(root: &Path, lines: &[Value]) {
        let mut text = String::new();
        for l in lines {
            text.push_str(&serde_json::to_string(l).unwrap());
            text.push('\n');
        }
        let frame = zstd::encode_all(text.as_bytes(), 3).unwrap();
        let path = log_path(root);
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .unwrap();
        f.write_all(&frame).unwrap();
    }

    fn session_header() -> Value {
        json!({"type":"session","version":4,"id":"session-aaaa","cwd":"C:/work/demo",
               "createdAt":1_700_000_000_000i64,"delegationDepth":0})
    }

    fn turn_start(turn: u32, ts: i64) -> Value {
        json!({"type":"turn/start","seq":10+turn,"time":ts,"data":{"turn":turn}})
    }

    fn turn_end(turn: u32, kind: &str, ts: i64) -> Value {
        json!({"type":"turn/end","seq":90+turn,"time":ts,"data":{"turn":turn,"reason":{"kind":kind}}})
    }

    fn tool_call(name: &str, id: &str, ts: i64) -> Value {
        json!({"type":"tool/call","seq":50,"time":ts,
               "data":{"turn":1,"step":1,"callId":id,"name":name,"arguments":"{}"}})
    }

    fn tool_result(id: &str, ts: i64) -> Value {
        json!({"type":"tool/result","seq":51,"time":ts,
               "data":{"turn":1,"step":1,"toolCallId":id,"message":{"role":"tool","toolCallId":id}}})
    }

    #[test]
    fn follows_a_real_turn_lifecycle() {
        let root = temp_root("lifecycle");
        let mut w = Watcher::new(root.clone());
        append(
            &root,
            &[
                session_header(),
                json!({"type":"session/title","seq":2,"time":1_700_000_000_100i64,
                       "data":{"title":"接入宠物状态"}}),
                turn_start(1, 1_700_000_000_200),
            ],
        );
        let first = w.scan().unwrap();
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].0.status, Status::Working);
        assert_eq!(first[0].0.title, "接入宠物状态");
        assert_eq!(first[0].0.task_id, "session-aaaa-t1");
        assert_eq!(first[0].0.thread_id.as_deref(), Some("session-aaaa"));

        append(&root, &[turn_end(1, "completed", 1_700_000_005_000)]);
        let done = w.scan().unwrap();
        assert_eq!(done.last().unwrap().0.status, Status::Done);
        assert_eq!(done.last().unwrap().0.task_id, "session-aaaa-t1");

        // 下一轮立刻开始：新一轮重新上屏，完成态换成工作态。
        append(&root, &[turn_start(2, 1_700_000_010_000)]);
        let again = w.scan().unwrap();
        assert_eq!(again.last().unwrap().0.status, Status::Working);
        assert_eq!(again.last().unwrap().0.task_id, "session-aaaa-t2");
    }

    #[test]
    fn other_turn_end_kinds_are_mapped_conservatively() {
        // 输出被上限截断：这一轮不会再自己推进，按完成显示。
        let root = temp_root("maxtokens");
        let mut w = Watcher::new(root.clone());
        append(&root, &[session_header(), turn_start(1, 1_700_000_000_000)]);
        w.scan().unwrap();
        append(&root, &[turn_end(1, "max-tokens", 1_700_000_001_000)]);
        assert_eq!(w.scan().unwrap().last().unwrap().0.status, Status::Done);

        // 批处理被 pre-step 拒掉：什么都没执行，不能显示成完成。
        let root = temp_root("blocked");
        let mut w = Watcher::new(root.clone());
        append(&root, &[session_header(), turn_start(1, 1_700_000_000_000)]);
        w.scan().unwrap();
        append(&root, &[turn_end(1, "blocked", 1_700_000_001_000)]);
        assert_eq!(w.scan().unwrap().last().unwrap().0.status, Status::Stopped);
    }

    #[test]
    fn timed_question_keeps_waiting_until_the_reply_arrives() {
        let root = temp_root("timed-question");
        let mut w = Watcher::new(root.clone());
        append(&root, &[session_header(), turn_start(1, 1_700_000_000_000)]);
        w.scan().unwrap();
        let args = json!({"questions":[{"question":"选哪个方案？","options":[]}]}).to_string();
        append(
            &root,
            &[json!({"type":"tool/call","seq":52,"time":1_700_000_001_000i64,
                     "data":{"turn":1,"callId":"call-q","name":"ask_user_question",
                             "arguments":args}})],
        );
        w.scan().unwrap();
        // 定时模式的「暂未答复」回执：问题仍然可以回答，等待不能清掉。
        append(
            &root,
            &[json!({"type":"tool/result","seq":53,"time":1_700_000_002_000i64,
                     "data":{"turn":1,"toolCallId":"call-q",
                             "message":{"role":"tool","toolCallId":"call-q",
                                        "content":[{"type":"text","text":"{\"pending\":true}"}]}}})],
        );
        // 状态没变时不会重复上报，所以用一个新回放器看最终推导结果。
        let mut replay = Watcher::new(root.clone());
        let evs = replay.scan().unwrap();
        assert_eq!(evs.last().unwrap().0.status, Status::Waiting);

        // 用户真的答了：等待解除。
        append(
            &root,
            &[json!({"type":"user/message","seq":54,"time":1_700_000_003_000i64,
                     "data":{"role":"user","source":{"kind":"user-question-reply"},
                             "content":[{"type":"text","text":"选 A"}]}})],
        );
        assert_eq!(w.scan().unwrap().last().unwrap().0.status, Status::Working);
    }

    #[test]
    fn aborted_and_failed_turns_do_not_look_like_success() {
        let root = temp_root("aborted");
        let mut w = Watcher::new(root.clone());
        append(&root, &[session_header(), turn_start(1, 1_700_000_000_000)]);
        w.scan().unwrap();
        append(&root, &[turn_end(1, "aborted", 1_700_000_001_000)]);
        assert_eq!(w.scan().unwrap().last().unwrap().0.status, Status::Stopped);

        let root = temp_root("failed");
        let mut w = Watcher::new(root.clone());
        append(&root, &[session_header(), turn_start(1, 1_700_000_000_000)]);
        w.scan().unwrap();
        append(&root, &[turn_end(1, "failed", 1_700_000_001_000)]);
        assert_eq!(w.scan().unwrap().last().unwrap().0.status, Status::Error);
    }

    #[test]
    fn pending_question_waits_until_it_is_answered() {
        let root = temp_root("question");
        let mut w = Watcher::new(root.clone());
        append(&root, &[session_header(), turn_start(1, 1_700_000_000_000)]);
        w.scan().unwrap();
        let args = json!({"questions":[{"question":"要部署到哪台机器？","options":[]}]}).to_string();
        append(
            &root,
            &[json!({"type":"tool/call","seq":52,"time":1_700_000_001_000i64,
                     "data":{"turn":1,"callId":"call-q","name":"ask_user_question",
                             "arguments":args}})],
        );
        let waiting = w.scan().unwrap();
        let ev = &waiting.last().unwrap().0;
        assert_eq!(ev.status, Status::Waiting);
        assert_eq!(ev.waiting_kind, Some(WaitingKind::Question));
        // 默认不把提问正文写到屏幕上。
        assert_eq!(ev.detail, "有 1 个问题待确认");

        append(&root, &[tool_result("call-q", 1_700_000_010_000)]);
        assert_eq!(w.scan().unwrap().last().unwrap().0.status, Status::Working);
    }

    fn user_message(text: &str, ts: i64) -> Value {
        json!({"type":"user/message","seq":40,"time":ts,
               "data":{"role":"user","source":{"kind":"user"},
                       "content":[{"type":"text","text":text}]}})
    }

    #[test]
    fn user_message_never_becomes_title_by_default() {
        let root = temp_root("no-prompt-title");
        append(
            &root,
            &[
                session_header(),
                turn_start(1, 1_700_000_000_200),
                user_message("机密需求：把客户数据库导出到 U 盘", 1_700_000_000_300),
            ],
        );
        let mut w = Watcher::new(root.clone());
        let events = w.scan().unwrap();
        let ev = &events.last().unwrap().0;
        assert_eq!(ev.title, "demo 第1轮", "无标题时只能用项目名 + 轮次");
        assert!(!ev.title.contains("机密"));
        assert!(!ev.title.contains("数据库"));
    }

    #[test]
    fn prompt_mode_opt_in_only() {
        let root = temp_root("prompt-title");
        append(
            &root,
            &[
                session_header(),
                turn_start(1, 1_700_000_000_200),
                user_message("机密需求：把客户数据库导出到 U 盘", 1_700_000_000_300),
            ],
        );
        // 默认 Watcher 不读正文。
        let mut tenant = Watcher::new(root.clone());
        let default_title = tenant.scan().unwrap().last().unwrap().0.title.clone();
        assert_eq!(default_title, "demo 第1轮");

        // 显式 opt-in 后才退回首行。
        let mut opted_in = Watcher::new(root.clone()).prompt_titles(true);
        let ev = opted_in.scan().unwrap();
        assert_eq!(
            ev.last().unwrap().0.title,
            "机密需求：把客户数据库导出到 U 盘"
        );
    }

    #[test]
    fn waiting_detail_has_no_question_text() {
        let root = temp_root("question-detail");
        let args = json!({"questions":[{"question":"密钥放在哪个目录？","options":[]}]}).to_string();
        append(
            &root,
            &[
                session_header(),
                turn_start(1, 1_700_000_000_000),
                json!({"type":"tool/call","seq":52,"time":1_700_000_001_000i64,
                       "data":{"turn":1,"callId":"call-q","name":"ask_user_question",
                               "arguments":args}}),
            ],
        );
        let mut w = Watcher::new(root.clone());
        let events = w.scan().unwrap();
        let detail = &events.last().unwrap().0.detail;
        assert_eq!(detail, "有 1 个问题待确认");
        assert!(!detail.contains("密钥"));

        // prompt 模式才保留题干（文档已标注会写到设备）。
        let mut opted_in = Watcher::new(root).prompt_titles(true);
        assert_eq!(
            opted_in.scan().unwrap().last().unwrap().0.detail,
            "密钥放在哪个目录？"
        );
    }

    #[test]
    fn pending_approval_waits_and_mentions_permission_only() {
        let root = temp_root("approval");
        let mut w = Watcher::new(root.clone());
        append(&root, &[session_header(), turn_start(1, 1_700_000_000_000)]);
        w.scan().unwrap();
        append(
            &root,
            &[json!({"type":"approval/asked","seq":60,"time":1_700_000_002_000i64,
                     "data":{"id":"ap-1","toolName":"plugin_manager","callId":"call-1",
                             "reason":"escalate sandbox to danger-full-access: …"}})],
        );
        let waiting = w.scan().unwrap();
        let ev = &waiting.last().unwrap().0;
        assert_eq!(ev.status, Status::Waiting);
        assert_eq!(ev.waiting_kind, Some(WaitingKind::Permission));
        assert!(ev.detail.contains("等你确认"));

        append(
            &root,
            &[json!({"type":"approval/decided","seq":61,"time":1_700_000_003_000i64,
                     "data":{"id":"ap-1","outcome":"allowed-once"}})],
        );
        assert_eq!(w.scan().unwrap().last().unwrap().0.status, Status::Working);
    }

    #[test]
    fn todo_list_drives_progress_and_resets_per_turn() {
        let root = temp_root("todos");
        let mut w = Watcher::new(root.clone());
        append(&root, &[session_header(), turn_start(1, 1_700_000_000_000)]);
        let first = w.scan().unwrap();
        assert_eq!(first[0].0.progress, None, "没有清单时不得编造百分比");

        let todos = json!({"todos":[
            {"content":"一","status":"completed"},
            {"content":"二","status":"in_progress"},
            {"content":"三","status":"cancelled"},
            {"content":"四","status":"pending"}]});
        append(
            &root,
            &[json!({"type":"todo/write","seq":70,"time":1_700_000_001_000i64,"data":todos})],
        );
        let ev = w.scan().unwrap();
        let ev = &ev.last().unwrap().0;
        assert_eq!(ev.progress_steps, Some((1, 3)), "取消项不占分母");
        assert_eq!(ev.progress, Some(33));

        append(&root, &[turn_start(2, 1_700_000_010_000)]);
        let ev = w.scan().unwrap();
        let ev = ev.last().unwrap().0.clone();
        assert_eq!(ev.progress, None, "新轮次不得继承上一轮的完成项");
    }

    #[test]
    fn context_usage_comes_from_usage_and_window_records() {
        let root = temp_root("context");
        let mut w = Watcher::new(root.clone());
        append(
            &root,
            &[
                session_header(),
                json!({"type":"request/context","seq":3,"time":1_700_000_000_100i64,
                       "data":{"provider":"deepseek-official","model":"deepseek-flash",
                               "contextWindow":1048576}}),
                turn_start(1, 1_700_000_000_200),
                json!({"type":"assistant/message","seq":20,"time":1_700_000_000_300i64,
                       "data":{"turn":1,"step":1,"message":{"role":"assistant","content":[]},
                               "usage":{"inputTokens":24210,"outputTokens":891,
                                        "cacheReadTokens":4224,"totalTokens":29325}}}),
            ],
        );
        w.scan().unwrap();
        assert_eq!(w.context_of("session-aaaa"), Some((28434, 1048576)));
    }

    #[test]
    fn subagent_child_sessions_are_not_followed() {
        let root = temp_root("child");
        std::fs::create_dir_all(root.join("--C-demo-proj--").join("bbbb-1111")).unwrap();
        let path = root
            .join("--C-demo-proj--")
            .join("bbbb-1111")
            .join("session.v4.jsonl.zstd");
        let text = [
            json!({"type":"session","id":"bbbb-1111","cwd":"C:/work/demo","delegationDepth":2})
                .to_string(),
            turn_start(1, 1_700_000_000_000).to_string(),
        ]
        .join("\n");
        std::fs::write(&path, zstd::encode_all(text.as_bytes(), 3).unwrap()).unwrap();
        let mut w = Watcher::new(root.clone());
        append(&root, &[session_header(), turn_start(1, 1_700_000_000_000)]);
        let evs = w.scan().unwrap();
        assert!(
            evs.iter().all(|(e, _)| e.task_id.starts_with("session-aaaa")),
            "子代理会话不能上屏：{:?}",
            evs.iter().map(|(e, _)| &e.task_id).collect::<Vec<_>>()
        );
    }

    #[test]
    fn partial_frame_is_kept_until_it_completes() {
        // 真机上是「写进程刚写了一半」。这里手工把一帧切成两段追加。
        let root = temp_root("partial");
        let text = format!(
            "{}\n{}\n",
            session_header(),
            turn_start(1, 1_700_000_000_000)
        );
        let frame = zstd::encode_all(text.as_bytes(), 3).unwrap();
        let (a, b) = frame.split_at(frame.len() / 2);
        let path = log_path(&root);
        std::fs::write(&path, a).unwrap();
        let mut w = Watcher::new(root.clone());
        assert!(w.scan().unwrap().is_empty(), "半帧不能猜状态");
        let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(b).unwrap();
        let evs = w.scan().unwrap();
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].0.status, Status::Working);
    }

    #[test]
    fn decode_capped_returns_decoded_prefix_without_panicking() {
        // 小输入正常解出全部内容。
        let text = "hello 世界 ".repeat(200);
        let frame = zstd::encode_all(text.as_bytes(), 3).unwrap();
        let decoded = decode_capped(&frame, MAX_DECODED_BYTES).unwrap();
        assert_eq!(decoded, text.as_bytes());

        // 超限只返回已解出的前缀，不 panic（上限用 1KB 模拟 64MB 的截断）。
        let big = vec![b'a'; 8192];
        let frame = zstd::encode_all(&big[..], 3).unwrap();
        let decoded = decode_capped(&frame, 1024).unwrap();
        assert_eq!(decoded.len(), 1024, "超限只返回已解部分");
        assert!(decoded.iter().all(|b| *b == b'a'));

        // 非 zstd 字节返回 Err，交给 decode_frames 继续退候选。
        assert!(decode_capped(b"not a zstd frame", 1024).is_err());
    }

    #[test]
    fn decode_all_frames_roundtrips_many_small_frames() {
        let mut buf = Vec::new();
        let mut expected = Vec::new();
        for i in 0..8 {
            let text = format!("{{\"type\":\"turn/start\",\"seq\":{i}}}\n");
            buf.extend(zstd::encode_all(text.as_bytes(), 3).unwrap());
            expected.extend_from_slice(text.as_bytes());
        }
        assert_eq!(decode_all_frames(&buf), expected);
        assert!(decode_all_frames(&[]).is_empty());
        assert!(decode_all_frames(b"garbage").is_empty());
    }

    #[test]
    fn multi_frame_append_decodes_every_record() {
        let root = temp_root("frames");
        let mut w = Watcher::new(root.clone());
        append(&root, &[session_header(), turn_start(1, 1_700_000_000_000)]);
        w.scan().unwrap();
        append(&root, &[tool_call("pwsh", "c-1", 1_700_000_001_000)]);
        append(&root, &[tool_result("c-1", 1_700_000_002_000)]);
        append(&root, &[turn_end(1, "completed", 1_700_000_003_000)]);
        let evs = w.scan().unwrap();
        assert_eq!(evs.last().unwrap().0.status, Status::Done);
    }

    #[test]
    fn activity_description_only_uses_the_tool_name() {
        let root = temp_root("activity");
        let mut w = Watcher::new(root.clone());
        append(&root, &[session_header(), turn_start(1, 1_700_000_000_000)]);
        w.scan().unwrap();
        append(&root, &[tool_call("pwsh", "c-1", 1_700_000_001_000)]);
        let ev = w.scan().unwrap();
        assert_eq!(ev.last().unwrap().0.detail, "正在执行命令");
        assert!(
            !serde_json::to_string(&ev.iter().map(|(e, _)| e).collect::<Vec<_>>())
                .unwrap()
                .contains("\"command\""),
            "不得读取/保存工具参数"
        );
    }

    #[test]
    fn stale_sessions_are_not_imported_on_first_scan() {
        let root = temp_root("stale");
        let old = now_secs() - IMPORT_WINDOW - 3600;
        append(&root, &[session_header(), turn_start(1, old * 1000)]);
        // 文件修改时间也做旧：首扫判断「历史会话」只看 mtime，不看记录时间。
        set_modified_ago(&log_path(&root), IMPORT_WINDOW + 3600);
        let mut w = Watcher::new(root.clone());
        assert!(w.scan().unwrap().is_empty(), "历史会话不该上屏");
    }

    /// 把文件修改时间改到 `ago` 秒之前。
    fn set_modified_ago(path: &Path, ago: i64) {
        let when = SystemTime::now() - std::time::Duration::from_secs(ago as u64);
        let f = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        f.set_times(std::fs::FileTimes::new().set_modified(when))
            .unwrap();
    }

    #[test]
    fn session_file_version_parser() {
        assert_eq!(session_version("session.v4.jsonl.zstd"), Some(4));
        assert_eq!(session_version("session.v1.jsonl.zstd"), Some(1));
        assert_eq!(session_version("session.jsonl.zstd"), Some(0));
        assert_eq!(session_version("session.v4.jsonl"), Some(4));
        assert_eq!(session_version("other.jsonl.zstd"), None);
    }

    #[test]
    fn finished_turn_keeps_the_model_consistent() {
        let root = temp_root("model");
        let mut w = Watcher::new(root.clone());
        append(&root, &[session_header(), turn_start(1, 1_700_000_000_000)]);
        let mut m = Model::default();
        for (ev, at) in w.scan().unwrap() {
            m.apply(ev, "dsh", at).unwrap();
        }
        append(&root, &[turn_end(1, "completed", 1_700_000_004_000)]);
        for (ev, at) in w.scan().unwrap() {
            m.apply(ev, "dsh", at).unwrap();
        }
        assert_eq!(m.current(now_secs()).map(|t| t.status()), Some(Status::Done));
    }
}
