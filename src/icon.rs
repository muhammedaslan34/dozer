//! The app icon, drawn in code. Also used by build.rs to make the .exe icon.

use tiny_skia::{FillRule, Paint, PathBuilder, Pixmap, Stroke, Transform};

/// A hub: one center node joined to four outer nodes, on a teal rounded square.
pub fn pixmap(size: u32) -> Pixmap {
    let mut pixmap = Pixmap::new(size, size).expect("icon size must be non-zero");
    let s = size as f32;

    let mut bg = Paint::default();
    bg.set_color_rgba8(13, 148, 136, 255);
    bg.anti_alias = true;
    if let Some(path) = rounded_rect(0.0, 0.0, s, s, s * 0.22) {
        pixmap.fill_path(&path, &bg, FillRule::Winding, Transform::identity(), None);
    }

    let mut white = Paint::default();
    white.set_color_rgba8(255, 255, 255, 255);
    white.anti_alias = true;
    let c = s / 2.0;
    let d = s * 0.27;
    let outer = [(c - d, c - d), (c + d, c - d), (c - d, c + d), (c + d, c + d)];

    let stroke = Stroke { width: (s * 0.07).max(1.0), line_cap: tiny_skia::LineCap::Round, ..Default::default() };
    let mut pb = PathBuilder::new();
    for (x, y) in outer {
        pb.move_to(c, c);
        pb.line_to(x, y);
    }
    if let Some(path) = pb.finish() {
        pixmap.stroke_path(&path, &white, &stroke, Transform::identity(), None);
    }
    for (x, y) in outer {
        if let Some(path) = PathBuilder::from_circle(x, y, s * 0.095) {
            pixmap.fill_path(&path, &white, FillRule::Winding, Transform::identity(), None);
        }
    }
    let mut amber = Paint::default();
    amber.set_color_rgba8(251, 191, 36, 255);
    amber.anti_alias = true;
    if let Some(path) = PathBuilder::from_circle(c, c, s * 0.14) {
        pixmap.fill_path(&path, &amber, FillRule::Winding, Transform::identity(), None);
    }
    pixmap
}

/// Straight (non-premultiplied) RGBA pixels.
#[allow(dead_code)]
pub fn rgba(size: u32) -> Vec<u8> {
    pixmap(size)
        .pixels()
        .iter()
        .flat_map(|p| {
            let c = p.demultiply();
            [c.red(), c.green(), c.blue(), c.alpha()]
        })
        .collect()
}

fn rounded_rect(x: f32, y: f32, w: f32, h: f32, r: f32) -> Option<tiny_skia::Path> {
    let k = 0.552_284_8 * r;
    let mut pb = PathBuilder::new();
    pb.move_to(x + r, y);
    pb.line_to(x + w - r, y);
    pb.cubic_to(x + w - r + k, y, x + w, y + r - k, x + w, y + r);
    pb.line_to(x + w, y + h - r);
    pb.cubic_to(x + w, y + h - r + k, x + w - r + k, y + h, x + w - r, y + h);
    pb.line_to(x + r, y + h);
    pb.cubic_to(x + r - k, y + h, x, y + h - r + k, x, y + h - r);
    pb.line_to(x, y + r);
    pb.cubic_to(x, y + r - k, x + r - k, y, x + r, y);
    pb.close();
    pb.finish()
}
