//! The preview pane (Alt+P): layout, painting and loading. Content decisions live in
//! `crate::preview`; native preview handlers are hosted by the window.

use crate::msg::Msg;
use crate::platform::{format_bytes, format_date};
use crate::preview::Content;
use crate::scene::{Align, Font, ImageKey, Rect, Scene};

use super::{App, Effect, Hit, PreviewView, TIMER_PREVIEW};

const LINE_H: f32 = 17.0;
pub const MIN_W: f32 = 200.0;

impl App {
    /// The single selected (or focused) item, if exactly one.
    fn preview_candidate(&self) -> Option<(std::path::PathBuf, String, bool)> {
        let t = self.tab();
        let sel = t.selected();
        let pos = match sel.as_slice() {
            [one] => *one,
            _ => return None,
        };
        let e = t.entry_at(pos)?;
        if e.drive.is_some() {
            return None;
        }
        Some((e.path.clone(), e.ext.clone(), e.is_dir))
    }

    /// Notices selection changes and schedules a (debounced) load.
    fn sync_preview_target(&mut self) {
        let cand = self.preview_candidate();
        if cand == self.preview.target {
            return;
        }
        self.preview.gen += 1;
        self.preview.scroll = 0.0;
        if let Some(ImageKey::Thumb(id)) = self.preview.image.take() {
            self.images.remove(&ImageKey::Thumb(id));
        }
        self.preview.view = if cand.is_some() {
            PreviewView::Loading
        } else {
            let many = self.tab().selection_count() > 1;
            PreviewView::Message(if many {
                "Select a single file to preview."
            } else {
                "Select a file to preview."
            })
        };
        self.preview.target = cand;
        if self.preview.target.is_some() {
            // Arrowing through files shouldn't load every one of them.
            self.effects.push(Effect::Timer { id: TIMER_PREVIEW, ms: 60 });
        }
    }

    /// Debounce timer fired: load the current target off the UI thread.
    pub fn load_preview(&mut self) {
        self.effects.push(Effect::KillTimer(TIMER_PREVIEW));
        let Some((path, ext, is_dir)) = self.preview.target.clone() else {
            return;
        };
        let gen = self.preview.gen;
        let r = self.geo.preview;
        let (w, h) = (((r.w() - 32.0) * self.scale) as u32, ((r.h() - 32.0) * self.scale) as u32);
        let notifier = self.notifier.clone();
        std::thread::spawn(move || {
            unsafe {
                let _ = windows::Win32::System::Com::CoInitializeEx(
                    None,
                    windows::Win32::System::Com::COINIT_MULTITHREADED,
                );
            }
            let content = crate::preview::load(&path, &ext, is_dir, w, h);
            notifier.send(Msg::Preview { gen, content });
        });
    }

    pub fn on_preview(&mut self, gen: u64, content: Content) {
        if gen != self.preview.gen {
            return;
        }
        self.preview.view = match content {
            Content::Image(b) => {
                let key = ImageKey::Thumb(crate::thumbs::next_image_id());
                self.images.put(key, b);
                self.preview.image = Some(key);
                PreviewView::Image
            }
            Content::Text(lines) => PreviewView::Text(lines),
            Content::Native(clsid) => PreviewView::Native(clsid),
            Content::Info => PreviewView::Info,
            Content::Message(m) => PreviewView::Message(m),
            Content::Loading => PreviewView::Loading,
        };
        self.invalidate();
    }

    /// The host couldn't start the native handler: fall back to details.
    pub fn native_preview_failed(&mut self) {
        self.preview.native_shown = None;
        self.preview.view = PreviewView::Info;
        self.invalidate();
    }

    pub fn toggle_preview(&mut self) {
        self.cfg.show_preview = !self.cfg.show_preview;
        self.effects.push(Effect::SaveConfig);
        self.invalidate();
    }

    /// Asks the host to show/move/remove the native handler when that changes.
    fn want_native(&mut self, want: Option<(std::path::PathBuf, windows::core::GUID, Rect)>) {
        if want != self.preview.native_shown {
            self.preview.native_shown = want.clone();
            self.effects.push(Effect::NativePreview(want));
        }
    }

    /// Called every frame; hides the native handler while the pane is closed.
    pub(super) fn sync_preview_closed(&mut self) {
        if !self.cfg.show_preview {
            self.want_native(None);
        }
    }

    pub(super) fn paint_preview(&mut self, s: &mut Scene, area: Rect) {
        let th = self.theme.clone();
        self.geo.preview = area;
        self.sync_preview_target();
        s.line(area.l + 0.5, area.t, area.l + 0.5, area.b, 1.0, th.divider);
        self.hits.push((area, Hit::Preview));
        self.hits.push((Rect::new(area.l - 3.0, area.t, area.l + 4.0, area.b), Hit::PreviewSplitter));
        let inner = Rect::new(area.l + 1.0, area.t, area.r, area.b);
        let mut native = None;
        match &self.preview.view {
            PreviewView::Message(m) => {
                s.text(inner.inset(16.0, 0.0), *m, Font::Ui, th.text_dim, Align::Center);
            }
            PreviewView::Loading => {}
            PreviewView::Image => {
                if let Some(key) = self.preview.image {
                    s.image(inner.inset(16.0, 16.0), key, 1.0);
                }
            }
            PreviewView::Text(lines) => {
                let content_h = lines.len() as f32 * LINE_H + 24.0;
                let max = (content_h - inner.h()).max(0.0);
                self.preview.scroll = self.preview.scroll.clamp(0.0, max);
                let first = (self.preview.scroll / LINE_H) as usize;
                let visible = (inner.h() / LINE_H) as usize + 2;
                s.clip(inner);
                for (i, line) in lines.iter().enumerate().skip(first).take(visible) {
                    let y = inner.t + 12.0 + i as f32 * LINE_H - self.preview.scroll;
                    s.text(
                        Rect::new(inner.l + 16.0, y, inner.r + 2000.0, y + LINE_H),
                        line.clone(),
                        Font::Mono,
                        th.text,
                        Align::Left,
                    );
                }
                s.unclip();
            }
            PreviewView::Native(clsid) => {
                if let Some((path, _, _)) = &self.preview.target {
                    native = Some((path.clone(), *clsid, inner));
                }
            }
            PreviewView::Info => self.paint_preview_info(s, inner),
        }
        self.want_native(native);
    }

    fn paint_preview_info(&mut self, s: &mut Scene, r: Rect) {
        let th = self.theme.clone();
        let Some(pos) = self.tab().selected().first().copied() else {
            return;
        };
        let Some(e) = self.tab().entry_at(pos).cloned() else {
            return;
        };
        let size = 96.0;
        let ir = Rect::xywh(r.cx() - size / 2.0, r.t + 48.0, size, size);
        if e.icon >= 0 {
            s.image(ir, self.icon_key(e.icon, size), 1.0);
        }
        let mut y = ir.b + 16.0;
        let name = e.display_name(self.cfg.show_extensions).to_string();
        s.text_wrap(Rect::new(r.l + 16.0, y, r.r - 16.0, y + 36.0), name, Font::UiSemibold, th.text);
        y += 40.0;
        let mut lines = vec![e.type_name.to_string()];
        if !e.is_dir {
            lines.push(format_bytes(e.size));
        }
        if e.modified != 0 {
            lines.push(format!("Modified {}", format_date(e.modified)));
        }
        if !e.is_dir {
            lines.push("No preview available.".into());
        }
        for l in lines {
            s.text(Rect::new(r.l + 16.0, y, r.r - 16.0, y + 18.0), l, Font::Small, th.text_dim, Align::Center);
            y += 20.0;
        }
    }

    pub fn scroll_preview(&mut self, delta: f32) {
        if let PreviewView::Text(_) = self.preview.view {
            self.preview.scroll = (self.preview.scroll - delta * 3.0 * LINE_H).max(0.0);
        }
    }
}
