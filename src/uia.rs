//! 从 DeepSeek Harness 窗口读「此刻在干什么」。
//!
//! 为什么必须走界面：状态行（`深度求索中`、`正在运行命令 · <这一步在做什么>`）
//! 里那句活动描述是渲染进程根据工具调用现场生成的，**磁盘上的会话日志只有工具名**
//! （`tool/call` 的 `name`），没有这句人话。已经核查过 `~/.dsh` 全目录（会话日志、
//! storages/projcache、日志与缓存），没有这句的副本。所以想把它放上墨水屏，只能
//! 读界面。
//!
//! 界面上它出现在对话末尾：当前这一步的状态按钮名形如
//!
//! ```text
//! 正在运行命令 · Dump DSH accessible text nodes
//! 深度求索中
//! 深度求索中，用时 6分54秒 ···
//! ```
//!
//! 历史轮次的状态行会留在可访问性树里，所以取**树序最后一个**状态节点——它才是
//! 当前这一步。读不到就返回 None，调用方回退内置短语池，屏幕上不会出现空白。
//!
//! 采样时机：只在确定这一帧真要上屏时调一次（见 `main::push_if_changed`），
//! 不单独轮询界面——界面自己换了十几条而任务状态没变时，设备不该跟着闪屏。

use std::sync::mpsc::{self, Sender};
use std::sync::OnceLock;
use std::time::Duration;

/// 采样超时。UIA 是跨进程 COM 调用，窗口卡顿时不能把守护拖住。
const PROBE_TIMEOUT: Duration = Duration::from_millis(1500);

/// 读一次界面上当前的活动描述；读不到时返回 None（调用方回退内置短语池）。
///
/// 界面文字可能带命令、路径等敏感片段：上屏/落日志前统一净化并截到 40 字符，
/// 避免整句原文跟着 pet.log 或屏幕一起被转发出去。
pub fn current_quip() -> Option<String> {
    imp::current_quip().and_then(|q| sanitize_quip(&q))
}

/// 花絮净化：去掉控制字符与双向控制符，最多保留 40 个字符；空串视为没读到。
fn sanitize_quip(raw: &str) -> Option<String> {
    let cleaned = crate::model::clean(raw, 40);
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// 可访问性树里节点的类别。只需要区分「文本」「按钮」和「其它」。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Text,
    Button,
    Other,
}

/// 从（按出现顺序排好的）节点序列里挑出当前活动描述。纯函数，便于单测。
///
/// 规则：
/// - 状态按钮 `正在… · <描述>`：取 `·` 后面的描述，那是最具体的一句；
/// - 文本 `深度求索中` / `深度求索中，用时 …`：没有更具体的信息时用它；
/// - 历史轮次的状态行同样匹配，所以**取最后一个**；
/// - 太短或不像话的（纯时长、纯标点）不要。
pub fn pick_quip(items: &[(Kind, String)]) -> Option<String> {
    let mut desc: Option<String> = None;
    let mut label: Option<String> = None;
    let mut saw_status = false;
    for (kind, name) in items {
        if *kind == Kind::Other {
            continue;
        }
        let name = name.trim();
        if let Some(rest) = running_line(name) {
            saw_status = true;
            // `正在运行命令 · 描述`：描述是最具体的一句，优先用它。
            if (4..=40).contains(&rest.chars().count())
                && rest.chars().any(|c| c.is_alphanumeric() || is_cjk(c))
            {
                desc = Some(rest.to_string());
            }
            continue;
        }
        if let Some(text) = deepseek_line(name) {
            saw_status = true;
            label = Some(text);
        }
    }
    desc.or(label).or_else(|| saw_status.then(|| "深度求索中".to_string()))
}

/// `正在… · <描述>` / `正在…` 形态的状态按钮；返回 `·` 后面的描述（可能为空）。
fn running_line(name: &str) -> Option<&str> {
    if !name.starts_with("正在") {
        return None;
    }
    // 太长的是整段工具卡片正文，不是状态行。
    if name.chars().count() > 120 {
        return None;
    }
    Some(match name.split_once(" · ") {
        Some((_, rest)) => rest.trim(),
        None => "",
    })
}

/// `深度求索中` / `深度求索中，用时 …`。
fn deepseek_line(name: &str) -> Option<String> {
    const HEAD: &str = "深度求索中";
    if !name.starts_with(HEAD) {
        return None;
    }
    let rest = name[HEAD.len()..].trim();
    // 只有「，用时 …」这种尾巴时退回短标签，别把时长顶到屏幕上（它每几秒在变）。
    if rest.is_empty() || rest.starts_with('，') || rest.starts_with(',') {
        Some(HEAD.to_string())
    } else {
        None
    }
}

fn is_cjk(c: char) -> bool {
    ('\u{4e00}'..='\u{9fff}').contains(&c)
}

/// DeepSeek Harness 窗口里的一个可访问性节点。只用于诊断/探测输入框位置。
#[derive(Clone, Debug)]
pub struct Node {
    pub control: String,
    pub class: String,
    pub name: String,
    pub focusable: bool,
    /// 支持的 UIA 模式（value / text / invoke…），空表示只有基础属性。
    pub patterns: String,
}

/// 把 DeepSeek Harness 窗口整棵可访问性树摊平成节点列表（诊断用）。
pub fn dump_nodes() -> Vec<Node> {
    imp::dump_nodes()
}

/// 把诊断用的节点列表还原成 `pick_quip` 的输入（`uiadump` 排错时用）。
pub fn pick_from_nodes(nodes: &[Node]) -> Option<String> {
    let items: Vec<(Kind, String)> = nodes
        .iter()
        .map(|n| {
            let kind = match n.control.as_str() {
                "Text" => Kind::Text,
                "Button" => Kind::Button,
                _ => Kind::Other,
            };
            (kind, n.name.clone())
        })
        .collect();
    pick_quip(&items)
}

/// 窗口主进程名。Electron 多进程，只有主机窗口进程带标题。
pub const PROCESS_NAME: &str = "DeepSeek Harness.exe";

/// 窗口标题标记：`<会话标题> — DeepSeek Harness`（用「结尾匹配」，避免把标题里\n/// 恰好带 DeepSeek Harness 字样的浏览器窗口当成主窗口）。
pub const WINDOW_MARK: &str = "DeepSeek Harness";

#[cfg(windows)]
mod imp {
    use super::*;
    use windows::Win32::Foundation::{BOOL, HWND, LPARAM, RECT};
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
    };
    use windows::Win32::UI::Accessibility::{
        CUIAutomation, IUIAutomation, IUIAutomationElement, IUIAutomationInvokePattern,
        IUIAutomationTextPattern, IUIAutomationValuePattern, TreeScope_Descendants,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetClassNameW, GetWindowRect, GetWindowTextW, IsWindowVisible,
    };

    /// 待命的工作线程：UIA 必须在初始化过 COM 的线程上调用，且一旦常驻，
    /// 后续采样就只剩一次进程内消息投递的代价。
    static CLIENT: OnceLock<Sender<Sender<Option<String>>>> = OnceLock::new();

    pub fn current_quip() -> Option<String> {
        let tx = CLIENT.get_or_init(spawn).clone();
        let (reply_tx, reply_rx) = mpsc::channel();
        tx.send(reply_tx).ok()?;
        reply_rx.recv_timeout(PROBE_TIMEOUT).ok().flatten()
    }

    /// 诊断通道：同样是「独占 COM 的常驻线程」，避免每次探测都重开 COM。
    static DUMP: OnceLock<Sender<Sender<Vec<Node>>>> = OnceLock::new();

    pub fn dump_nodes() -> Vec<Node> {
        let tx = DUMP.get_or_init(spawn_dump).clone();
        let (reply_tx, reply_rx) = mpsc::channel();
        if tx.send(reply_tx).is_err() {
            return vec![];
        }
        reply_rx
            .recv_timeout(Duration::from_secs(5))
            .unwrap_or_default()
    }

    fn spawn_dump() -> Sender<Sender<Vec<Node>>> {
        let (tx, rx) = mpsc::channel::<Sender<Vec<Node>>>();
        std::thread::Builder::new()
            .name("uia-dump".into())
            .spawn(move || {
                let automation: Option<IUIAutomation> = unsafe {
                    let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
                    CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER).ok()
                };
                for reply in rx {
                    let value = match &automation {
                        Some(a) => read_nodes(a),
                        None => vec![],
                    };
                    let _ = reply.send(value);
                }
            })
            .ok();
        tx
    }

    fn spawn() -> mpsc::Sender<Sender<Option<String>>> {
        let (tx, rx) = mpsc::channel::<Sender<Option<String>>>();
        std::thread::Builder::new()
            .name("uia-probe".into())
            .spawn(move || {
                // SAFETY: 线程独占 COM 初始化；失败时下面会自然退化成「读不到」。
                let automation: Option<IUIAutomation> = unsafe {
                    let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
                    CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER).ok()
                };
                for reply in rx {
                    let value = match &automation {
                        Some(a) => read_quip(a),
                        None => None,
                    };
                    // 调用方可能已经超时走人，发送失败属正常。
                    let _ = reply.send(value);
                }
            })
            .ok();
        tx
    }

    /// 摊平整棵树（诊断用）。控件类型按 UIA 的 50000 段编号翻成短名。
    fn read_nodes(automation: &IUIAutomation) -> Vec<Node> {
        let Some(root) = element_of_dsh(automation) else {
            return vec![];
        };
        let Ok(condition) = (unsafe { automation.CreateTrueCondition() }) else {
            return vec![];
        };
        let Ok(found) = (unsafe { root.FindAll(TreeScope_Descendants, &condition) }) else {
            return vec![];
        };
        let Ok(count) = (unsafe { found.Length() }) else {
            return vec![];
        };
        let mut nodes = Vec::with_capacity(count as usize);
        for index in 0..count {
            let Ok(element) = (unsafe { found.GetElement(index) }) else {
                continue;
            };
            let control = match unsafe { element.CurrentControlType() } {
                Ok(t) => match t.0 {
                    50000 => "Button".into(),
                    50001 => "Calendar".into(),
                    50002 => "CheckBox".into(),
                    50003 => "ComboBox".into(),
                    50004 => "Edit".into(),
                    50005 => "Hyperlink".into(),
                    50006 => "Image".into(),
                    50007 => "ListItem".into(),
                    50008 => "List".into(),
                    50009 => "Menu".into(),
                    50010 => "MenuBar".into(),
                    50011 => "MenuItem".into(),
                    50012 => "ProgressBar".into(),
                    50013 => "RadioButton".into(),
                    50014 => "ScrollBar".into(),
                    50015 => "Slider".into(),
                    50016 => "Spinner".into(),
                    50017 => "StatusBar".into(),
                    50018 => "Tab".into(),
                    50019 => "TabItem".into(),
                    50020 => "Text".into(),
                    50021 => "ToolBar".into(),
                    50022 => "ToolTip".into(),
                    50023 => "Tree".into(),
                    50024 => "TreeItem".into(),
                    50025 => "Custom".into(),
                    50026 => "Group".into(),
                    50027 => "Thumb".into(),
                    50028 => "DataGrid".into(),
                    50029 => "DataItem".into(),
                    50030 => "Document".into(),
                    50031 => "SplitButton".into(),
                    50032 => "Window".into(),
                    50033 => "Pane".into(),
                    50034 => "Header".into(),
                    50035 => "HeaderItem".into(),
                    50036 => "Table".into(),
                    50037 => "TitleBar".into(),
                    50038 => "Separator".into(),
                    other => format!("Type{other}"),
                },
                Err(_) => "Unknown".into(),
            };
            let class = unsafe { element.CurrentClassName() }
                .ok()
                .map(|v| v.to_string())
                .unwrap_or_default();
            let name = unsafe { element.CurrentName() }
                .ok()
                .map(|v| v.to_string())
                .unwrap_or_default();
            let focusable = unsafe { element.CurrentIsKeyboardFocusable() }
                .map(|v| v.as_bool())
                .unwrap_or(false);
            let mut patterns = Vec::new();
            // 直接试着取模式接口：能取到就说明支持。
            use windows::Win32::UI::Accessibility::{
                UIA_InvokePatternId, UIA_SelectionItemPatternId, UIA_TextPatternId,
                UIA_TogglePatternId, UIA_ValuePatternId,
            };
            for (id, label) in [
                (UIA_ValuePatternId, "value"),
                (UIA_TextPatternId, "text"),
                (UIA_InvokePatternId, "invoke"),
                (UIA_SelectionItemPatternId, "select"),
                (UIA_TogglePatternId, "toggle"),
            ] {
                if unsafe { element.GetCurrentPatternAs::<IUIAutomationValuePattern>(id) }.is_ok()
                    || unsafe { element.GetCurrentPatternAs::<IUIAutomationTextPattern>(id) }.is_ok()
                    || unsafe { element.GetCurrentPatternAs::<IUIAutomationInvokePattern>(id) }.is_ok()
                {
                    if !patterns.contains(&label) {
                        patterns.push(label);
                    }
                }
            }
            nodes.push(Node {
                control,
                class,
                name,
                focusable,
                patterns: patterns.join(","),
            });
        }
        nodes
    }

    /// 读当前活动描述：按树序取最后一个状态节点。
    ///
    /// Chromium 的渲染进程只在「有 UIA 客户端」时才建无障碍树，刚连上时拿到的
    /// 还是退化树（实测只有标题栏那 9 个节点）。所以必须重试：单次查询会随机
    /// 失败，这也是「界面明明在跑、屏幕却读不到」的真正原因。
    fn read_quip(automation: &IUIAutomation) -> Option<String> {
        for attempt in 0..8 {
            if attempt > 0 {
                std::thread::sleep(Duration::from_millis(180));
            }
            if let Some(quip) = read_quip_once(automation) {
                return Some(quip);
            }
        }
        None
    }

    fn read_quip_once(automation: &IUIAutomation) -> Option<String> {
        // 直接复用诊断用的「摊平整棵树」：它走 TrueCondition，最稳。按控件类型
        // 建 Or 条件的 FindAll 在冷启动时容易只拿到退化的壳，实测读不到状态行。
        pick_from_nodes(&read_nodes(automation))
    }
    /// 找主窗口：多进程 Electron，按「可见 + 标题含 DeepSeek Harness +
    /// 窗口类 Chrome_WidgetWin_1」定位，再挑面积最大的那个（主窗口）。
    fn element_of_dsh(automation: &IUIAutomation) -> Option<IUIAutomationElement> {
        struct Search {
            hwnd: HWND,
            area: i64,
        }
        unsafe extern "system" fn callback(hwnd: HWND, lparam: LPARAM) -> BOOL {
            let search = unsafe { &mut *(lparam.0 as *mut Search) };
            if unsafe { IsWindowVisible(hwnd) }.as_bool() {
                let title = window_title(hwnd);
                if title.ends_with(WINDOW_MARK) && class_name(hwnd) == "Chrome_WidgetWin_1" {
                    let mut rect = RECT::default();
                    if unsafe { GetWindowRect(hwnd, &mut rect) }.is_ok() {
                        let area = i64::from(rect.right - rect.left)
                            * i64::from(rect.bottom - rect.top);
                        if area > search.area {
                            search.hwnd = hwnd;
                            search.area = area;
                        }
                    }
                }
            }
            BOOL(1)
        }
        let mut search = Search {
            hwnd: HWND(std::ptr::null_mut()),
            area: 0,
        };
        // SAFETY: 回调只在本调用期间使用 search；EnumWindows 同步执行。
        unsafe { EnumWindows(Some(callback), LPARAM(&mut search as *mut Search as isize)) }.ok()?;
        if search.hwnd.0.is_null() {
            return None;
        }
        unsafe { automation.ElementFromHandle(search.hwnd) }.ok()
    }

    fn window_title(hwnd: HWND) -> String {
        let mut buf = [0u16; 512];
        let len = unsafe { GetWindowTextW(hwnd, &mut buf) };
        String::from_utf16_lossy(&buf[..len.max(0) as usize])
    }

    fn class_name(hwnd: HWND) -> String {
        let mut buf = [0u16; 256];
        let len = unsafe { GetClassNameW(hwnd, &mut buf) };
        String::from_utf16_lossy(&buf[..len.max(0) as usize])
    }
}

#[cfg(not(windows))]
mod imp {
    use super::*;
    pub fn current_quip() -> Option<String> {
        None
    }
    pub fn dump_nodes() -> Vec<Node> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(s: &str) -> (Kind, String) {
        (Kind::Text, s.to_string())
    }
    fn button(s: &str) -> (Kind, String) {
        (Kind::Button, s.to_string())
    }

    #[test]
    fn picks_the_last_running_status_and_keeps_its_description() {
        let items = vec![
            // 历史轮次留下的状态行：不能取它，否则设备永远停在上一轮那句话上。
            button("正在运行命令 · 上一次的命令"),
            text("已处理"),
            // 当前这一步。
            button("正在运行命令 · Dump DSH accessible text nodes"),
            text("深度求索中，用时 6分54秒 ···"),
        ];
        assert_eq!(
            pick_quip(&items).as_deref(),
            Some("Dump DSH accessible text nodes")
        );
    }

    #[test]
    fn falls_back_to_the_short_label_and_drops_the_elapsed_time() {
        let items = vec![text("深度求索中"), text("深度求索中，用时 6分54秒 ···")];
        assert_eq!(pick_quip(&items).as_deref(), Some("深度求索中"));
    }

    #[test]
    fn quips_are_sanitized_and_capped_before_leaving_uia() {
        // 控制字符 / 双向控制符：不能让它们跟着花絮上屏或进日志。
        assert_eq!(
            sanitize_quip("正在运行\u{202e}命令\u{0007}").as_deref(),
            Some("正在运行命令")
        );
        // 超长的一句话只留前 40 个字符。
        let long = "命".repeat(100);
        assert_eq!(sanitize_quip(&long).unwrap().chars().count(), 40);
        // 纯空白/纯控制字符 = 没读到，交给内置短语池。
        assert!(sanitize_quip("  \u{0007} ").is_none());
        assert!(sanitize_quip("").is_none());
    }

    #[test]
    fn ignores_reading_ui_chrome_and_waiting_cards() {
        let items = vec![
            text("会话层级"),
            text("标准模式"),
            text("正在运行命令"),
            button("已运行命令 · 生成报告"),
            text(""),
        ];
        // `正在运行命令` 没有描述：退回短标签，但不把 UI 标签当花絮。
        assert_eq!(pick_quip(&items).as_deref(), Some("深度求索中"));
        assert!(pick_quip(&[]).is_none());
    }
}
