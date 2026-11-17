//! 鳄鱼大宠物的形象层：把 `assets/croc/<outfit>/<action>.png`（30×40 单色）
//! 以 3× 最近邻放大画到 400×300 画布上。
//!
//! 素材契约（见 SPEC FR-CROC-03）：30×40、黑/透明，3× 放大到 90×120，
//! 画在右下角 (300,150)。v1 只有 `default` 一套；以后加装扮只需加目录。
//!
//! 画法：把位图压成「贪心矩形」再交给 raster——每个矩形是场景里的一个
//! `rect` 元素。这样一帧只有几十个元素，比逐像素画上千个矩形轻得多。

use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// 六套旧造型的 id/显示名仍保留，用于 `skin` 诊断命令兼容；
/// 版式已经不再按造型换装，而是按动作选图（动作由当前状态决定）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Skin {
    Pixel,
    Abacus,
    Fortune,
    Astro,
    Lens,
    Latte,
}

impl Default for Skin {
    fn default() -> Self {
        Skin::Astro
    }
}

impl Skin {
    pub const ALL: [Skin; 6] = [
        Skin::Pixel,
        Skin::Abacus,
        Skin::Fortune,
        Skin::Astro,
        Skin::Lens,
        Skin::Latte,
    ];

    pub fn id(self) -> &'static str {
        match self {
            Skin::Pixel => "pixel",
            Skin::Abacus => "abacus",
            Skin::Fortune => "fortune",
            Skin::Astro => "astro",
            Skin::Lens => "lens",
            Skin::Latte => "latte",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Skin::Pixel => "像素喵",
            Skin::Abacus => "金算喵",
            Skin::Fortune => "暴富喵",
            Skin::Astro => "星际喵",
            Skin::Lens => "镜头喵",
            Skin::Latte => "拿铁喵",
        }
    }

    pub fn desc(self) -> &'static str {
        match self {
            Skin::Pixel => "最原始的像素猫",
            Skin::Abacus => "算盘一响，黄金万两",
            Skin::Fortune => "暴富猫，见者有份",
            Skin::Astro => "头上带天线，信号满格",
            Skin::Lens => "镜头对准，记录工作",
            Skin::Latte => "一杯拿铁，慢慢干活",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Some(match value.trim().to_ascii_lowercase().as_str() {
            "pixel" => Skin::Pixel,
            "abacus" => Skin::Abacus,
            "fortune" => Skin::Fortune,
            "astro" => Skin::Astro,
            "lens" => Skin::Lens,
            "latte" => Skin::Latte,
            _ => return None,
        })
    }
}

/// 六个动作（SPEC FR-CROC-01）。单帧静态，只有动作变了才重绘。
pub const ACTIONS: [&str; 6] = ["idle", "working", "waiting", "done", "error", "sleep"];

fn embedded(action: &str) -> Option<&'static [u8]> {
    Some(match action {
        "idle" => include_bytes!("../assets/croc/default/idle.png").as_slice(),
        "working" => include_bytes!("../assets/croc/default/working.png").as_slice(),
        "waiting" => include_bytes!("../assets/croc/default/waiting.png").as_slice(),
        "done" => include_bytes!("../assets/croc/default/done.png").as_slice(),
        "error" => include_bytes!("../assets/croc/default/error.png").as_slice(),
        "sleep" => include_bytes!("../assets/croc/default/sleep.png").as_slice(),
        _ => return None,
    })
}

/// 位图缓存：动作名 → 30×40 的黑白掩码（true = 黑）。
fn mask(action: &str) -> &'static Vec<Vec<bool>> {
    static CACHE: OnceLock<Mutex<HashMap<String, &'static Vec<Vec<bool>>>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(m) = cache.lock().unwrap().get(action) {
        return m;
    }
    let mut out = vec![vec![false; 40]; 30];
    if let Some(bytes) = embedded(action) {
        if let Ok(img) = image::load_from_memory(bytes) {
            let gray = img.to_luma8();
            for y in 0..40.min(gray.height() as usize) {
                for x in 0..30.min(gray.width() as usize) {
                    if gray.get_pixel(x as u32, y as u32).0[0] < 128 {
                        out[x][y] = true;
                    }
                }
            }
        }
    }
    // 只加载一次：掩码故意泄漏成 'static，随进程存活。
    let leaked: &'static Vec<Vec<bool>> = Box::leak(Box::new(out));
    *cache
        .lock()
        .unwrap()
        .entry(action.to_string())
        .or_insert(leaked)
}

/// 把 30×40 掩码压成贪心矩形，写进场景。返回画了多少个矩形。
pub fn draw_action(e: &mut Vec<Value>, x: i32, y: i32, action: &str, scale: i32) -> usize {
    let m = mask(action);
    let mut used = vec![[false; 40]; 30];
    let mut count = 0;
    for py in 0..40usize {
        for px in 0..30usize {
            if !m[px][py] || used[px][py] {
                continue;
            }
            // 先向右扩到最大宽度
            let mut w = 0usize;
            while px + w < 30 && m[px + w][py] && !used[px + w][py] {
                w += 1;
            }
            // 再向下扩：整行都黑且未被占用才继续
            let mut h = 1usize;
            while py + h < 40 {
                let full = (0..w).all(|i| m[px + i][py + h] && !used[px + i][py + h]);
                if !full {
                    break;
                }
                h += 1;
            }
            for yy in py..py + h {
                for xx in px..px + w {
                    used[xx][yy] = true;
                }
            }
            e.push(json!({
                "type": "rect",
                "x": x + (px as i32) * scale,
                "y": y + (py as i32) * scale,
                "w": (w as i32) * scale,
                "h": (h as i32) * scale,
                "fill": 0,
            }));
            count += 1;
        }
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_actions_have_art() {
        for a in ACTIONS {
            assert!(embedded(a).is_some(), "动作 {a} 缺少 PNG");
            let mut e = vec![];
            let n = draw_action(&mut e, 300, 150, a, 3);
            assert!(n > 20, "动作 {a} 只有 {n} 个矩形，图可能空白");
            assert!(n < 200, "动作 {a} 矩形过多（{n}），场景会太重");
        }
    }

    #[test]
    fn unknown_action_draws_nothing() {
        let mut e = vec![];
        assert_eq!(draw_action(&mut e, 0, 0, "nope", 3), 0);
        assert!(e.is_empty());
    }

    #[test]
    fn skin_ids_roundtrip() {
        for s in Skin::ALL {
            assert_eq!(Skin::parse(s.id()), Some(s));
        }
    }
}
