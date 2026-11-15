//! Renderer-agnostic display list.
//!
//! The UI never talks to Direct2D directly: every frame it fills a [`Scene`] with simple drawing
//! commands in DIPs. The native backend (`render.rs`) replays the scene with Direct2D; a future web
//! backend can serialize the same commands to a `<canvas>` client (see docs/ARCHITECTURE.md).
//! Images are referenced by [`ImageKey`] so a remote client only has to receive each bitmap once.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct Color {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}

pub const fn rgba(r: u8, g: u8, b: u8, a: f32) -> Color {
    Color {
        r: r as f32 / 255.0,
        g: g as f32 / 255.0,
        b: b as f32 / 255.0,
        a,
    }
}

impl Color {
    pub fn with_alpha(self, a: f32) -> Color {
        Color { a: self.a * a, ..self }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct Rect {
    pub l: f32,
    pub t: f32,
    pub r: f32,
    pub b: f32,
}

impl Rect {
    pub const fn new(l: f32, t: f32, r: f32, b: f32) -> Rect {
        Rect { l, t, r, b }
    }
    pub fn xywh(x: f32, y: f32, w: f32, h: f32) -> Rect {
        Rect::new(x, y, x + w, y + h)
    }
    pub fn w(&self) -> f32 {
        self.r - self.l
    }
    pub fn h(&self) -> f32 {
        self.b - self.t
    }
    pub fn cx(&self) -> f32 {
        (self.l + self.r) * 0.5
    }
    pub fn cy(&self) -> f32 {
        (self.t + self.b) * 0.5
    }
    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.l && x < self.r && y >= self.t && y < self.b
    }
    pub fn inset(&self, dx: f32, dy: f32) -> Rect {
        Rect::new(self.l + dx, self.t + dy, self.r - dx, self.b - dy)
    }
    pub fn offset(&self, dx: f32, dy: f32) -> Rect {
        Rect::new(self.l + dx, self.t + dy, self.r + dx, self.b + dy)
    }
    /// The overlapping part of two rects (may be empty: check with `is_empty`).
    pub fn intersect(&self, o: &Rect) -> Rect {
        Rect::new(self.l.max(o.l), self.t.max(o.t), self.r.min(o.r), self.b.min(o.b))
    }
    pub fn is_empty(&self) -> bool {
        self.r <= self.l || self.b <= self.t
    }
    pub fn intersects(&self, o: &Rect) -> bool {
        self.l < o.r && o.l < self.r && self.t < o.b && o.t < self.b
    }
    /// Normalized rectangle spanning two points (for rubber-band selection).
    pub fn span(x0: f32, y0: f32, x1: f32, y1: f32) -> Rect {
        Rect::new(x0.min(x1), y0.min(y1), x0.max(x1), y0.max(y1))
    }
}

/// Logical fonts. Backends map these to real faces (Segoe UI / Segoe Fluent Icons).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Font {
    /// Body text, 12 DIP (Explorer's 9pt).
    Ui,
    UiSemibold,
    /// Secondary text in tiles and the status bar.
    Small,
    /// Icon font glyphs.
    Icon16,
    Icon14,
    Icon12,
    Icon10,
    Icon8,
    /// Monospace text (preview pane).
    Mono,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Align {
    Left,
    Center,
    Right,
}

/// Bitmaps a scene can draw. Icons come from the shell's system image list; thumbnails are
/// produced by the thumbnail workers and live in the [`ImageStore`] under a unique id.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ImageKey {
    /// System image list index (overlay in bits 24..28) and list (`SHIL_*`).
    SysIcon { index: i32, list: u8 },
    Thumb(u64),
}

#[derive(Clone, Debug)]
pub enum Cmd {
    Fill(Rect, Color),
    Round(Rect, f32, Color),
    Stroke(Rect, f32, f32, Color),
    Line(f32, f32, f32, f32, f32, Color),
    Text {
        r: Rect,
        s: String,
        font: Font,
        color: Color,
        align: Align,
        /// Wrap onto several lines (icon-view labels); otherwise single line with ellipsis.
        wrap: bool,
    },
    /// Image fitted into `r` preserving aspect ratio, centered.
    Image { r: Rect, key: ImageKey, opacity: f32 },
    PushClip(Rect),
    PopClip,
}

#[derive(Default)]
pub struct Scene {
    pub cmds: Vec<Cmd>,
}

impl Scene {
    pub fn clear(&mut self) {
        self.cmds.clear();
    }
    pub fn fill(&mut self, r: Rect, c: Color) {
        if c.a > 0.0 {
            self.cmds.push(Cmd::Fill(r, c));
        }
    }
    pub fn round(&mut self, r: Rect, radius: f32, c: Color) {
        if c.a > 0.0 {
            self.cmds.push(Cmd::Round(r, radius, c));
        }
    }
    pub fn stroke(&mut self, r: Rect, radius: f32, width: f32, c: Color) {
        if c.a > 0.0 {
            self.cmds.push(Cmd::Stroke(r, radius, width, c));
        }
    }
    pub fn line(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, w: f32, c: Color) {
        self.cmds.push(Cmd::Line(x0, y0, x1, y1, w, c));
    }
    pub fn text(&mut self, r: Rect, s: impl Into<String>, font: Font, color: Color, align: Align) {
        self.cmds.push(Cmd::Text {
            r,
            s: s.into(),
            font,
            color,
            align,
            wrap: false,
        });
    }
    pub fn text_wrap(&mut self, r: Rect, s: impl Into<String>, font: Font, color: Color) {
        self.cmds.push(Cmd::Text {
            r,
            s: s.into(),
            font,
            color,
            align: Align::Center,
            wrap: true,
        });
    }
    pub fn glyph(&mut self, r: Rect, g: char, font: Font, color: Color) {
        self.text(r, g.to_string(), font, color, Align::Center);
    }
    pub fn image(&mut self, r: Rect, key: ImageKey, opacity: f32) {
        self.cmds.push(Cmd::Image { r, key, opacity });
    }
    pub fn clip(&mut self, r: Rect) {
        self.cmds.push(Cmd::PushClip(r));
    }
    pub fn unclip(&mut self) {
        self.cmds.push(Cmd::PopClip);
    }
}

/// Text measurement, implemented by each backend (DirectWrite natively).
pub trait Measure {
    fn text_width(&self, s: &str, font: Font) -> f32;
}

/// Premultiplied BGRA pixels.
pub struct Bitmap {
    pub w: u32,
    pub h: u32,
    pub data: Vec<u8>,
}

/// Decoded bitmaps shared between workers (producers) and renderers (consumers).
#[derive(Default)]
pub struct ImageStore {
    map: Mutex<HashMap<ImageKey, Arc<Bitmap>>>,
}

impl ImageStore {
    pub fn get(&self, key: &ImageKey) -> Option<Arc<Bitmap>> {
        self.map.lock().unwrap().get(key).cloned()
    }
    pub fn put(&self, key: ImageKey, bmp: Bitmap) {
        self.map.lock().unwrap().insert(key, Arc::new(bmp));
    }
    pub fn remove(&self, key: &ImageKey) {
        self.map.lock().unwrap().remove(key);
    }
    /// Drops all thumbnails whose ids are not in `keep`.
    pub fn retain_thumbs(&self, keep: impl Fn(u64) -> bool) {
        self.map.lock().unwrap().retain(|k, _| match k {
            ImageKey::Thumb(id) => keep(*id),
            _ => true,
        });
    }
}
