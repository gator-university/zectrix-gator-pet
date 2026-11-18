//! 鳄鱼大宠物的形象层：把 `assets/croc/<outfit>/<action>.png`（90×120 单色）
//! 以 1:1 画到 400×300 画布上。
//!
//! 素材契约（见 SPEC FR-CROC-03 / FR-CROC-05）：90×120、L 模式、阈值 <128 为墨，
//! 画在右下角 (300,152)（品牌行下移 2px 后的最终基线）。v1 只有 `default` 一套；
//! 以后加装扮只需加目录。
//!
//! 画法：把位图压成「贪心矩形」再交给 raster——每个矩形是场景里的一个
//! `rect` 元素。这样一帧只有两三百个元素，比逐像素画上万像素轻得多。

use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// 素材尺寸。v2 起素材直接按 1:1 绘制，不再做 3× 最近邻放大。
const SPRITE_W: usize = 90;
const SPRITE_H: usize = 120;

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

/// 位图缓存：动作名 → 90×120 的黑白掩码（true = 墨）。
fn mask(action: &str) -> &'static Vec<Vec<bool>> {
    static CACHE: OnceLock<Mutex<HashMap<String, &'static Vec<Vec<bool>>>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(m) = cache.lock().unwrap().get(action) {
        return m;
    }
    let mut out = vec![vec![false; SPRITE_H]; SPRITE_W];
    if let Some(bytes) = embedded(action) {
        if let Ok(img) = image::load_from_memory(bytes) {
            let gray = img.to_luma8();
            if gray.width() as usize != SPRITE_W || gray.height() as usize != SPRITE_H {
                // mask() 每个动作只解码一次（之后走缓存），所以这里只会报一次；
                // 坏素材不至于静默裁剪成半只鳄鱼，能在 pet.log / 测试输出里看到。
                eprintln!(
                    "[素材] 动作 {action} 的 PNG 是 {}×{}，契约要求 {}×{}；已按左上角裁剪",
                    gray.width(),
                    gray.height(),
                    SPRITE_W,
                    SPRITE_H
                );
            }
            for y in 0..SPRITE_H.min(gray.height() as usize) {
                for x in 0..SPRITE_W.min(gray.width() as usize) {
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

/// 把 90×120 掩码压成贪心矩形，写进场景。返回画了多少个矩形。
pub fn draw_action(e: &mut Vec<Value>, x: i32, y: i32, action: &str, scale: i32) -> usize {
    let m = mask(action);
    let mut used = vec![[false; SPRITE_H]; SPRITE_W];
    let mut count = 0;
    for py in 0..SPRITE_H {
        for px in 0..SPRITE_W {
            if !m[px][py] || used[px][py] {
                continue;
            }
            // 先向右扩到最大宽度
            let mut w = 0usize;
            while px + w < SPRITE_W && m[px + w][py] && !used[px + w][py] {
                w += 1;
            }
            // 再向下扩：整行都墨且未被占用才继续
            let mut h = 1usize;
            while py + h < SPRITE_H {
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
            let bytes = embedded(a).expect("动作缺少 PNG");
            // 尺寸必须直接量 PNG 本身：mask() 会预分配 90×120 再 min() 拷贝，
            // 所以掩码的边长永远等于常量，量它抓不到坏素材。
            let img = image::load_from_memory(bytes).expect("PNG 必须可解码");
            assert_eq!(img.width(), SPRITE_W as u32, "动作 {a} 宽度不是 90");
            assert_eq!(img.height(), SPRITE_H as u32, "动作 {a} 高度不是 120");

            let m = mask(a);

            let ink = m.iter().flatten().filter(|px| **px).count();
            let pixels = SPRITE_W * SPRITE_H;
            assert!(
                ink * 100 >= 6 * pixels,
                "动作 {a} 墨量只有 {:.1}%，图可能空白",
                ink as f32 / pixels as f32 * 100.0
            );
            assert!(
                ink * 100 <= 50 * pixels,
                "动作 {a} 墨量 {:.1}% 过高，墨水瓶会糊成一块",
                ink as f32 / pixels as f32 * 100.0
            );

            let mut min_x = SPRITE_W;
            let mut min_y = SPRITE_H;
            let mut max_x = 0usize;
            let mut max_y = 0usize;
            for x in 0..SPRITE_W {
                for y in 0..SPRITE_H {
                    if m[x][y] {
                        min_x = min_x.min(x);
                        min_y = min_y.min(y);
                        max_x = max_x.max(x + 1);
                        max_y = max_y.max(y + 1);
                    }
                }
            }
            assert!(
                max_x - min_x >= 40 && max_y - min_y >= 60,
                "动作 {a} 包围盒只有 {}×{}，太小",
                max_x - min_x,
                max_y - min_y
            );

            let mut e = vec![];
            // 按出厂基线 (300,152) 画：品牌行下移 2px 后的实际位置。
            let n = draw_action(&mut e, 300, 152, a, 1);
            assert!(n > 20, "动作 {a} 只有 {n} 个矩形，图可能空白");
            // 90×120 线稿贪婪矩形化后实测 208–274 个；上限留给未来微调。
            assert!(n <= 400, "动作 {a} 矩形过多（{n}），场景会太重");
            for v in &e {
                let x = v["x"].as_i64().unwrap();
                let y = v["y"].as_i64().unwrap();
                let w = v["w"].as_i64().unwrap();
                let h = v["h"].as_i64().unwrap();
                assert!(x >= 300 && y >= 152, "动作 {a} 有元素画到了 (300,152) 之外");
                assert!(x + w <= 390, "动作 {a} 有元素越过右边界");
                assert!(y + h <= 272, "动作 {a} 有元素压到底部信息线");
            }
        }
    }

    #[test]
    fn actions_are_pairwise_distinct() {
        for (i, a) in ACTIONS.iter().enumerate() {
            for b in &ACTIONS[i + 1..] {
                assert_ne!(mask(a), mask(b), "动作 {a} 与 {b} 的位图完全相同");
            }
        }
    }

    #[test]
    fn unknown_action_draws_nothing() {
        let mut e = vec![];
        assert_eq!(draw_action(&mut e, 0, 0, "nope", 1), 0);
        assert!(e.is_empty());
    }

}
