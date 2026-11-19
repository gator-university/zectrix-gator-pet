//! 场景光栅化：把 layout 输出的元素列表（rect / text）画成 400×300 灰度 PNG。
//!
//! 上游项目把这步交给 ZECTRIX Hub 的公共渲染器；本机程序没有 Hub，
//! 因此这里用 fontdue + 系统字体实现同样的元素语义：
//! - `rect`：`fill` 有值则填充灰度，无值则描 1px 边（w/h 为 0 时画直线）
//! - `text`：`y` 是基线，`width_limit` 是可用宽度，超宽按像素裁剪
//!
//! 输出是连续灰阶 PNG，不做任何量化或抖动——灰阶压缩由 SDK 按设备色板完成。

use anyhow::{Context, Result};
use fontdue::{Font, FontSettings};
use image::{codecs::png::PngEncoder, ExtendedColorType, ImageEncoder};
use serde_json::Value;
use std::path::PathBuf;

pub const W: i32 = 400;
pub const H: i32 = 300;

pub struct Fonts {
    regular: Font,
    bold: Font,
}

fn font_dir() -> PathBuf {
    let root = std::env::var("WINDIR").unwrap_or_else(|_| r"C:\Windows".to_string());
    PathBuf::from(root).join("Fonts")
}

fn load_first(names: &[&str]) -> Result<Font> {
    let dir = font_dir();
    let mut tried = vec![];
    for name in names {
        let path = dir.join(name);
        if !path.is_file() {
            continue;
        }
        let bytes =
            std::fs::read(&path).with_context(|| format!("无法读取字体 {}", path.display()))?;
        match Font::from_bytes(
            bytes,
            FontSettings {
                collection_index: 0,
                ..Default::default()
            },
        ) {
            Ok(f) => return Ok(f),
            Err(e) => tried.push(format!("{}: {e}", path.display())),
        }
    }
    anyhow::bail!(
        "没有可用的中文字体（尝试过 {names:?}）：{}",
        tried.join("; ")
    )
}

impl Fonts {
    pub fn load() -> Result<Self> {
        Ok(Self {
            regular: load_first(&["msyh.ttc", "simhei.ttf", "Deng.ttf", "simsun.ttc"])?,
            bold: load_first(&["msyhbd.ttc", "simhei.ttf", "Dengb.ttf", "msyh.ttc"])?,
        })
    }
}

struct Canvas {
    pixels: Vec<u8>,
}

impl Canvas {
    fn new() -> Self {
        Self {
            pixels: vec![255; (W * H) as usize],
        }
    }

    fn set(&mut self, x: i32, y: i32, gray: u8) {
        if x < 0 || y < 0 || x >= W || y >= H {
            return;
        }
        self.pixels[(y * W + x) as usize] = gray;
    }

    /// 用覆盖率把前景色混合到已有像素上，保留文字边缘的灰阶过渡。
    fn blend(&mut self, x: i32, y: i32, coverage: u8, ink: u8) {
        if x < 0 || y < 0 || x >= W || y >= H || coverage == 0 {
            return;
        }
        let idx = (y * W + x) as usize;
        let bg = self.pixels[idx] as u32;
        let cov = coverage as u32;
        self.pixels[idx] = ((bg * (255 - cov) + ink as u32 * cov) / 255) as u8;
    }

    fn fill_rect(&mut self, x: i32, y: i32, w: i32, h: i32, gray: u8) {
        for py in y..y + h {
            for px in x..x + w {
                self.set(px, py, gray);
            }
        }
    }

    /// 圆角矩形填充：圆角处按带符号距离算覆盖率，带抗锯齿。
    fn fill_round_rect(&mut self, x: i32, y: i32, w: i32, h: i32, r: i32, gray: u8) {
        if r <= 0 {
            return self.fill_rect(x, y, w, h, gray);
        }
        let r = r.min(w / 2).min(h / 2) as f32;
        let cx = x as f32 + w as f32 / 2.0;
        let cy = y as f32 + h as f32 / 2.0;
        let hx = w as f32 / 2.0 - r;
        let hy = h as f32 / 2.0 - r;
        for py in y..y + h {
            for px in x..x + w {
                let dx = (px as f32 + 0.5 - cx).abs() - hx;
                let dy = (py as f32 + 0.5 - cy).abs() - hy;
                // 圆角矩形 SDF（标准公式，末尾必须减 r）：内部为负，外部为正；
                // 0.5px 过渡带做抗锯齿
                let dist = dx.max(dy).min(0.0) + dx.max(0.0).hypot(dy.max(0.0)) - r;
                let cov = ((0.5 - dist) * 255.0).clamp(0.0, 255.0) as u8;
                if cov > 0 {
                    self.blend(px, py, cov, gray);
                }
            }
        }
    }

    fn stroke_rect(&mut self, x: i32, y: i32, w: i32, h: i32, gray: u8) {
        if h <= 0 {
            for px in x..x + w.max(1) {
                self.set(px, y, gray);
            }
            return;
        }
        if w <= 0 {
            for py in y..y + h.max(1) {
                self.set(x, py, gray);
            }
            return;
        }
        for px in x..x + w {
            self.set(px, y, gray);
            self.set(px, y + h - 1, gray);
        }
        for py in y..y + h {
            self.set(x, py, gray);
            self.set(x + w - 1, py, gray);
        }
    }
}

fn int(el: &Value, key: &str, default: i32) -> i32 {
    el[key].as_i64().map(|v| v as i32).unwrap_or(default)
}

fn draw_rect(c: &mut Canvas, el: &Value) {
    let (x, y, w, h) = (
        int(el, "x", 0),
        int(el, "y", 0),
        int(el, "w", 0),
        int(el, "h", 0),
    );
    let radius = int(el, "radius", 0);
    match el["fill"].as_u64() {
        Some(gray) => c.fill_round_rect(x, y, w, h, radius, gray.min(255) as u8),
        None => c.stroke_rect(x, y, w, h, 0),
    }
}

/// 线段到点的距离（用于画带圆头、抗锯齿的粗线）。
fn seg_dist(px: f32, py: f32, x1: f32, y1: f32, x2: f32, y2: f32) -> f32 {
    let dx = x2 - x1;
    let dy = y2 - y1;
    let len2 = dx * dx + dy * dy;
    if len2 <= f32::EPSILON {
        return ((px - x1).powi(2) + (py - y1).powi(2)).sqrt();
    }
    let t = (((px - x1) * dx + (py - y1) * dy) / len2).clamp(0.0, 1.0);
    let cx = x1 + t * dx;
    let cy = y1 + t * dy;
    ((px - cx).powi(2) + (py - cy).powi(2)).sqrt()
}

/// `line`：带圆头的粗线段（用来画矢量图标，比如任务完成的勾）。字体里不一定有
/// ✓（微软雅黑就没有），与其找符号字形，不如直接画出来，形状也更可控。
fn draw_line(c: &mut Canvas, el: &Value) {
    let x1 = int(el, "x1", 0) as f32;
    let y1 = int(el, "y1", 0) as f32;
    let x2 = int(el, "x2", 0) as f32;
    let y2 = int(el, "y2", 0) as f32;
    let width = int(el, "width", 2).max(1) as f32;
    let ink = el["fill"].as_u64().map(|v| v.min(255) as u8).unwrap_or(0);
    let half = width / 2.0;
    let minx = (x1.min(x2) - half - 1.0).floor() as i32;
    let maxx = (x1.max(x2) + half + 1.0).ceil() as i32;
    let miny = (y1.min(y2) - half - 1.0).floor() as i32;
    let maxy = (y1.max(y2) + half + 1.0).ceil() as i32;
    for py in miny..=maxy {
        for px in minx..=maxx {
            let dist = seg_dist(
                px as f32 + 0.5,
                py as f32 + 0.5,
                x1,
                y1,
                x2,
                y2,
            );
            let cov = ((half + 0.5 - dist) * 255.0).clamp(0.0, 255.0) as u8;
            if cov > 0 {
                c.blend(px, py, cov, ink);
            }
        }
    }
}

fn draw_text(c: &mut Canvas, el: &Value, fonts: &Fonts) {
    let source = el["content"].as_str().unwrap_or("");
    if source.is_empty() {
        return;
    }
    let x = int(el, "x", 0);
    let baseline = int(el, "y", 0);
    let size = int(el, "size", 12).clamp(6, 96) as f32;
    let limit = int(el, "width_limit", W - x);
    let right = (x + limit).min(W);
    let bold = el["weight"].as_str() == Some("bold");
    let font = if bold { &fonts.bold } else { &fonts.regular };
    let fitted;
    let content = if el["truncate"].as_bool() == Some(true) {
        fitted = fit_text(source, limit.max(0) as f32, font, size);
        fitted.as_str()
    } else {
        source
    };
    if content.is_empty() {
        return;
    }
    // 墨水色默认黑；黑底白字徽章传 "color":255。
    let ink = el["color"].as_u64().map(|v| v.min(255) as u8).unwrap_or(0);

    // 右对齐：先量总宽，再从右边界往左排；量不出宽度时退化为左对齐。
    let mut pen = x as f32;
    if el["align"].as_str() == Some("right") {
        let total: f32 = content
            .chars()
            .map(|ch| font.rasterize(ch, size).0.advance_width)
            .sum();
        pen = (right as f32 - total).max(x as f32);
    }
    for ch in content.chars() {
        let (metrics, bitmap) = font.rasterize(ch, size);
        if pen >= right as f32 {
            break;
        }
        let left = pen.round() as i32 + metrics.xmin;
        let top = baseline - (metrics.ymin + metrics.height as i32);
        for row in 0..metrics.height {
            for col in 0..metrics.width {
                let coverage = bitmap[row * metrics.width + col];
                if coverage == 0 {
                    continue;
                }
                let px = left + col as i32;
                if px >= right {
                    continue;
                }
                c.blend(px, top + row as i32, coverage, ink);
            }
        }
        pen += metrics.advance_width;
    }
}

fn fit_text(content: &str, max_width: f32, font: &Font, size: f32) -> String {
    let width = |s: &str| {
        s.chars()
            .map(|ch| font.rasterize(ch, size).0.advance_width)
            .sum::<f32>()
    };
    if width(content) <= max_width {
        return content.to_string();
    }
    let ellipsis_width = font.rasterize('…', size).0.advance_width;
    let mut fitted = String::new();
    let mut used = 0.0;
    for ch in content.chars() {
        let advance = font.rasterize(ch, size).0.advance_width;
        if used + advance + ellipsis_width > max_width {
            break;
        }
        fitted.push(ch);
        used += advance;
    }
    if ellipsis_width <= max_width {
        fitted.push('…');
    }
    fitted
}

pub fn render(scene: &Value, fonts: &Fonts) -> Vec<u8> {
    let mut canvas = Canvas::new();
    if let Some(elements) = scene["elements"].as_array() {
        for el in elements {
            match el["type"].as_str() {
                Some("rect") => draw_rect(&mut canvas, el),
                Some("line") => draw_line(&mut canvas, el),
                Some("text") => draw_text(&mut canvas, el, fonts),
                _ => {}
            }
        }
    }
    canvas.pixels
}

pub fn encode_png(pixels: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    PngEncoder::new(&mut out)
        .write_image(pixels, W as u32, H as u32, ExtendedColorType::L8)
        .context("PNG 编码失败")?;
    Ok(out)
}

pub fn to_png(scene: &Value, fonts: &Fonts) -> Result<Vec<u8>> {
    encode_png(&render(scene, fonts))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn canvas_is_400x300_white_by_default() {
        let px = render(
            &json!({"canvas":{"w":400,"h":300},"elements":[]}),
            &Fonts::load().unwrap(),
        );
        assert_eq!(px.len(), 400 * 300);
        assert!(px.iter().all(|p| *p == 255));
    }

    fn dark(pixels: &[u8]) -> usize {
        pixels.iter().filter(|p| **p < 128).count()
    }

    #[test]
    fn rect_fill_and_stroke() {
        let f = Fonts::load().unwrap();
        let filled = render(
            &json!({"elements":[{"type":"rect","x":10,"y":10,"w":20,"h":20,"fill":0}]}),
            &f,
        );
        assert_eq!(dark(&filled), 400);
        // w=0/h=0 的描边退化为 1px 直线
        let line = render(
            &json!({"elements":[{"type":"rect","x":16,"y":82,"w":100,"h":0}]}),
            &f,
        );
        assert_eq!(dark(&line), 100);
        let vline = render(
            &json!({"elements":[{"type":"rect","x":16,"y":80,"w":0,"h":10}]}),
            &f,
        );
        assert_eq!(dark(&vline), 10);
    }

    #[test]
    fn line_draws_a_thick_segment() {
        let f = Fonts::load().unwrap();
        let px = render(
            &json!({"elements":[{"type":"line","x1":20,"y1":20,"x2":60,"y2":60,"width":4,"fill":0}]}),
            &f,
        );
        let d = dark(&px);
        assert!(d > 120 && d < 600, "粗线应画出一条实线段，实际 {d}");
        // 端点须有圆头：起点附近也应有墨迹
        assert!(px[(20 * 400 + 20) as usize] < 128, "起点应有圆头墨迹");
    }

    #[test]
    fn text_draws_ink_and_respects_width_limit() {
        let f = Fonts::load().unwrap();
        let wide = render(
            &json!({"elements":[{"type":"text","content":"工作搭子","x":20,"y":60,"size":28,"width_limit":300}]}),
            &f,
        );
        assert!(dark(&wide) > 100, "文字应产生黑色像素");
        let clipped = render(
            &json!({"elements":[{"type":"text","content":"工作搭子","x":20,"y":60,"size":28,"width_limit":40}]}),
            &f,
        );
        assert!(dark(&clipped) < dark(&wide), "超宽内容必须裁剪");
        // 裁剪只作用于 x 之后：起点左侧与宽度上限右侧都必须保持白色
        let narrow = render(
            &json!({"elements":[{"type":"text","content":"工作搭子","x":300,"y":60,"size":28,"width_limit":20}]}),
            &f,
        );
        for y in 0..300 {
            for x in 0..300 {
                assert_eq!(
                    narrow[(y * 400 + x) as usize],
                    255,
                    "文字起点左侧必须保持白色"
                );
            }
            for x in 320..400 {
                assert_eq!(
                    narrow[(y * 400 + x) as usize],
                    255,
                    "宽度限制外必须保持白色"
                );
            }
        }
    }

    #[test]
    fn fitted_text_keeps_an_ellipsis_inside_its_pixel_limit() {
        let fonts = Fonts::load().unwrap();
        let scene = json!({"elements":[{
            "type":"text","content":"MMMMMMMMMMMMMMMM","x":20,"y":60,
            "size":16,"width_limit":64,"truncate":true
        }]});
        let pixels = render(&scene, &fonts);
        let ink_columns: Vec<i32> = (20..400)
            .filter(|x| (0..300).any(|y| pixels[(y * 400 + x) as usize] < 220))
            .collect();
        assert!(!ink_columns.is_empty());
        let rightmost = *ink_columns.last().unwrap();
        assert!(rightmost < 84, "文字不得越过 x + width_limit，实际 {rightmost}");
        assert!(rightmost >= 68, "省略号应留在可用区域末端，实际 {rightmost}");
    }

    #[test]
    fn long_chinese_mixed_and_windows_path_titles_keep_a_fitted_ellipsis() {
        let fonts = Fonts::load().unwrap();
        let samples = [
            "检查多任务标题截断是否适配中文字符宽度和省略号",
            "Update task_cache.rs after normal heartbeats preserve ordering",
            r"C:\Users\liujj\AppData\Local\Temp\very-long-session-title-for-preview.log",
        ];

        for sample in samples {
            let fitted = fit_text(sample, 112.0, &fonts.regular, 16.0);
            assert!(fitted.ends_with('…'), "标题应保留省略号：{sample}");
            let width = fitted
                .chars()
                .map(|ch| fonts.regular.rasterize(ch, 16.0).0.advance_width)
                .sum::<f32>();
            assert!(width <= 112.0, "{sample} 截断后仍超出像素宽度：{width}");
        }
    }

    #[test]
    fn right_aligned_text_hugs_the_right_edge() {
        let f = Fonts::load().unwrap();
        let px = render(
            &json!({"elements":[{"type":"text","content":"跟随","x":0,"y":60,"size":20,"width_limit":400,"align":"right"}]}),
            &f,
        );
        let cols: Vec<i32> = (0..400)
            .filter(|x| (0..300).any(|y| px[(y * 400 + x) as usize] < 200))
            .collect();
        assert!(!cols.is_empty());
        let rightmost = *cols.last().unwrap();
        let leftmost = *cols.first().unwrap();
        assert!(rightmost >= 380, "文字应贴近右边界，实际右端 {rightmost}");
        assert!(leftmost > 300, "右对齐后左侧应留白，实际左端 {leftmost}");
    }

    #[test]
    fn text_y_is_baseline_not_top() {
        let f = Fonts::load().unwrap();
        // 上游布局把 y 当基线：size=28、y=59 的字块必须整体落在基线之上
        let px = render(
            &json!({"elements":[{"type":"text","content":"工作搭子","x":16,"y":59,"size":28,"width_limit":340}]}),
            &f,
        );
        let rows: Vec<i32> = (0..300)
            .filter(|y| (0..400).any(|x| px[(y * 400 + x) as usize] < 200))
            .collect();
        let top = *rows.first().unwrap();
        let bottom = *rows.last().unwrap();
        // 允许少量下伸笔画（如「子」的竖钩）与抗锯齿，但绝不能是「以 y 为顶」的 59..90
        assert!(bottom <= 65, "字块主体必须落在基线附近，实际底边 {bottom}");
        assert!(
            (30..=40).contains(&top),
            "字块顶部应在基线上方约一个字号处，实际 {top}"
        );
    }
}
