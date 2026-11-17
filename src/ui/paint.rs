//! Paints the whole window into a [`Scene`] and records hit regions, mirroring Windows 11 File
//! Explorer: tab strip, address bar, command bar, navigation pane, item views, status bar.

use crate::fs::{Column, Entry, Thumb};
use crate::platform::{format_bytes, format_date, format_kb};
use crate::scene::{rgba, Align, Color, Font, ImageKey, Measure, Rect, Scene};

use super::layout::*;
use super::tab::{folder_name, Location};
use super::{App, Cmd, Drag, Focus, Hit, ViewMode};

pub mod glyph {
    pub const BACK: char = '\u{E72B}';
    pub const FORWARD: char = '\u{E72A}';
    pub const UP: char = '\u{E74A}';
    pub const REFRESH: char = '\u{E72C}';
    pub const CHEVRON_R: char = '\u{E974}';
    pub const CHEVRON_D: char = '\u{E96E}';
    pub const CHEVRON_U: char = '\u{E96D}';
    pub const CHEVRON_DOWN: char = '\u{E70D}';
    pub const SEARCH: char = '\u{E721}';
    pub const CANCEL: char = '\u{E711}';
    pub const ADD: char = '\u{E710}';
    pub const MORE: char = '\u{E712}';
    pub const CUT: char = '\u{E8C6}';
    pub const COPY: char = '\u{E8C8}';
    pub const PASTE: char = '\u{E77F}';
    pub const RENAME: char = '\u{E8AC}';
    pub const DELETE: char = '\u{E74D}';
    pub const SORT: char = '\u{E8CB}';
    pub const VIEW: char = '\u{E8A9}';
    pub const DETAILS: char = '\u{E8FD}';
    pub const TILES: char = '\u{E8A9}';
    pub const MIN: char = '\u{E921}';
    pub const MAX: char = '\u{E922}';
    pub const RESTORE: char = '\u{E923}';
    pub const CLOSE: char = '\u{E8BB}';
    pub const OVERFLOW: char = '\u{E8A0}';
    pub const PROPERTIES: char = '\u{E946}';
    pub const PIN: char = '\u{E718}';
    pub const SEARCH_OPTIONS: char = '\u{E71C}';
    pub const PREVIEW: char = '\u{E8A1}';
}

#[derive(Default, Clone)]
pub struct Geometry {
    pub items: Rect,
    pub files: Rect,
    pub nav: Rect,
    pub address: Rect,
    pub address_text: Rect,
    pub search: Rect,
    pub search_text: Rect,
    pub rename_text: Rect,
    pub crumbs: Vec<Location>,
    pub crumb_rects: Vec<Rect>,
    pub crumb_hidden: Vec<Location>,
    /// (left, right) of each details column.
    pub cols: Vec<(f32, f32)>,
    pub scroll_track: Rect,
    pub scroll_thumb: Rect,
    pub nav_track: Rect,
    pub nav_thumb: Rect,
    pub cmds: Vec<(Cmd, Rect)>,
    pub tab_rects: Vec<Rect>,
    pub preview: Rect,
}

const HQ: f32 = 4.0; // corner radius used throughout (Fluent "control" radius)

impl App {
    fn hit(&mut self, r: Rect, h: Hit) {
        self.hits.push((r, h));
    }

    pub fn hit_test(&self, x: f32, y: f32) -> Hit {
        self.hits
            .iter()
            .rev()
            .find(|(r, _)| r.contains(x, y))
            .map(|(_, h)| *h)
            .unwrap_or(Hit::None)
    }

    fn button_bg(&self, s: &mut Scene, r: Rect, h: Hit) {
        if self.hover == h {
            let pressed = matches!(self.drag, Drag::Button(p) if p == h);
            s.round(r, HQ, if pressed { self.theme.pressed } else { self.theme.hover });
        }
    }

    pub fn paint(&mut self, s: &mut Scene, m: &dyn Measure) {
        s.clear();
        self.hits.clear();
        let (w, h) = self.size;
        let th = self.theme.clone();
        // Chrome (transparent over Mica), toolbar surface, content surface.
        s.fill(Rect::new(0.0, 0.0, w, TAB_H), th.chrome);
        s.fill(Rect::new(0.0, TAB_H, w, TOOLBAR_B), th.toolbar);
        s.fill(Rect::new(0.0, TOOLBAR_B, w, h), th.content);
        self.paint_tabs(s, m, w);
        self.paint_address(s, m, w);
        self.paint_commands(s, m, w);
        s.line(0.0, TOOLBAR_B - 0.5, w, TOOLBAR_B - 0.5, 1.0, th.divider);
        let body = Rect::new(0.0, TOOLBAR_B, w, h - STATUS_H);
        let nav_w = self.cfg.nav_width.clamp(NAV_MIN, (w * 0.6).max(NAV_MIN));
        self.paint_nav(s, m, Rect::new(0.0, body.t, nav_w, body.b));
        s.line(nav_w + 0.5, body.t, nav_w + 0.5, body.b, 1.0, th.divider);
        self.hit(Rect::new(nav_w - 3.0, body.t, nav_w + 4.0, body.b), Hit::NavSplitter);
        let files = Rect::new(nav_w + 1.0, body.t, w, body.b);
        if self.cfg.show_preview {
            let pw = self
                .cfg
                .preview_width
                .clamp(super::preview_pane::MIN_W, (files.w() - 240.0).max(super::preview_pane::MIN_W));
            self.paint_files(s, m, Rect::new(files.l, files.t, files.r - pw, files.b));
            self.paint_preview(s, Rect::new(files.r - pw, files.t, files.r, files.b));
        } else {
            self.geo.preview = Rect::default();
            self.paint_files(s, m, files);
        }
        self.sync_preview_closed();
        self.paint_status(s, m, Rect::new(0.0, h - STATUS_H, w, h));
        self.paint_caption(s, w);
        if let Some((text, x, y)) = self.tooltip.clone() {
            let tw = m.text_width(&text, Font::Small) + 16.0;
            let r = Rect::xywh(x.min(w - tw - 4.0), y, tw, 26.0);
            s.round(r.offset(0.0, 1.0), HQ, rgba(0, 0, 0, 0.12));
            s.round(r, HQ, if th.dark { rgba(43, 43, 43, 1.0) } else { rgba(249, 249, 249, 1.0) });
            s.stroke(r, HQ, 1.0, th.divider);
            s.text(r, text, Font::Small, th.text, Align::Center);
        }
    }

    // ---- tab strip ----------------------------------------------------------------------------

    fn paint_tabs(&mut self, s: &mut Scene, m: &dyn Measure, w: f32) {
        let th = self.theme.clone();
        let fg = if self.window_active { th.text } else { th.text_dim };
        let n = self.tabs.len();
        let avail = w - CAPTION_W * 3.0 - 8.0 - 40.0 - 40.0;
        let tw = (avail / n as f32).clamp(TAB_MIN_W, TAB_MAX_W);
        let mut x = 8.0;
        self.geo.tab_rects.clear();
        for i in 0..n {
            let r = Rect::new(x, TAB_TOP, x + tw, TAB_H);
            self.geo.tab_rects.push(r);
            let active = i == self.active;
            if active {
                // Selected tab merges into the toolbar below it.
                s.round(Rect::new(r.l, r.t, r.r, r.b + 8.0), 8.0, th.toolbar);
                s.fill(Rect::new(r.l, r.b - 1.0, r.r, r.b + 1.0), th.toolbar);
            } else {
                if self.hover == Hit::Tab(i) || self.hover == Hit::TabClose(i) {
                    s.round(r.inset(2.0, 4.0).offset(0.0, -2.0), HQ, th.tab_inactive_hover);
                } else if i + 1 != self.active && i + 1 < n {
                    s.line(r.r - 0.5, r.t + 10.0, r.r - 0.5, r.b - 10.0, 1.0, th.divider);
                }
            }
            self.hit(r, Hit::Tab(i));
            let icon = self.tabs[i].icon;
            let key = self.icon_key(icon, 16.0);
            if icon >= 0 {
                s.image(Rect::xywh(r.l + 12.0, r.cy() - 8.0, 16.0, 16.0), key, 1.0);
            } else {
                s.glyph(Rect::xywh(r.l + 12.0, r.cy() - 8.0, 16.0, 16.0), glyph::SEARCH, Font::Icon12, fg);
            }
            let close = Rect::xywh(r.r - 34.0, r.cy() - 13.0, 28.0, 26.0);
            let title = self.tabs[i].title();
            s.text(
                Rect::new(r.l + 36.0, r.t, close.l - 2.0, r.b),
                title,
                Font::Ui,
                fg,
                Align::Left,
            );
            if active || self.hover == Hit::Tab(i) || self.hover == Hit::TabClose(i) {
                self.button_bg(s, close, Hit::TabClose(i));
                s.glyph(close, glyph::CANCEL, Font::Icon10, fg);
                self.hit(close, Hit::TabClose(i));
            }
            x += tw;
        }
        let plus = Rect::xywh(x + 4.0, TAB_TOP + 2.0, 36.0, 28.0);
        self.button_bg(s, plus, Hit::NewTab);
        s.glyph(plus, glyph::ADD, Font::Icon12, fg);
        self.hit(plus, Hit::NewTab);
        let _ = m;
    }

    fn paint_caption(&mut self, s: &mut Scene, w: f32) {
        let th = &self.theme;
        let fg = if self.window_active { th.text } else { th.text_dim };
        // HTMINBUTTON = 8, HTMAXBUTTON = 9, HTCLOSE = 20
        let buttons = [
            (8u32, glyph::MIN, 3.0),
            (9u32, if self.maximized { glyph::RESTORE } else { glyph::MAX }, 2.0),
            (20u32, glyph::CLOSE, 1.0),
        ];
        let mut hits = Vec::new();
        for (code, g, slot) in buttons {
            let r = Rect::new(w - CAPTION_W * slot, 0.0, w - CAPTION_W * (slot - 1.0), TAB_H - 8.0);
            let hot = self.caption_hover == code;
            let pressed = hot && self.caption_pressed == code;
            let close = code == 20 && !th.high_contrast;
            if hot {
                let fill = if close {
                    rgba(196, 43, 28, if pressed { 0.9 } else { 1.0 })
                } else if pressed {
                    th.pressed
                } else {
                    th.hover
                };
                s.fill(r, fill);
            }
            let color = if hot && close { rgba(255, 255, 255, 1.0) } else { fg };
            s.glyph(r, g, Font::Icon10, color);
            hits.push((r, Hit::Caption(code)));
        }
        self.hits.extend(hits);
    }

    // ---- address row --------------------------------------------------------------------------

    fn paint_address(&mut self, s: &mut Scene, m: &dyn Measure, w: f32) {
        let th = self.theme.clone();
        let y = TAB_H + (ADDR_H - BUTTON) / 2.0;
        let t = self.tab();
        let nav = [
            (Hit::Back, glyph::BACK, !t.back.is_empty()),
            (Hit::Forward, glyph::FORWARD, !t.fwd.is_empty()),
            (Hit::Up, glyph::UP, t.loc.parent().is_some()),
            (Hit::Refresh, glyph::REFRESH, true),
        ];
        let mut x = 8.0;
        for (hit, g, enabled) in nav {
            let r = Rect::xywh(x, y, 40.0, BUTTON);
            if enabled {
                self.button_bg(s, r, hit);
                self.hit(r, hit);
            }
            s.glyph(r, g, Font::Icon14, if enabled { th.text } else { th.text_disabled });
            x += 40.0;
        }
        let search_w = (w * 0.25).clamp(160.0, 340.0);
        let addr = Rect::new(x + 8.0, y, w - search_w - 20.0, y + BUTTON);
        let search = Rect::new(addr.r + 8.0, y, w - 12.0, y + BUTTON);
        self.geo.address = addr;
        self.geo.search = search;
        let editing = self.address.is_some();
        self.field_bg(s, addr, editing && self.focus == Focus::Address);
        self.hit(addr, Hit::Address);
        if editing {
            let tr = Rect::new(addr.l + 10.0, addr.t, addr.r - 8.0, addr.b);
            self.geo.address_text = tr;
            let caret = self.caret_on && self.focus == Focus::Address;
            let focused = self.focus == Focus::Address;
            if let Some(f) = self.address.as_mut() {
                f.paint(s, tr, m, Font::Ui, &th, focused, caret);
            }
        } else {
            self.paint_crumbs(s, m, addr);
        }
        // Search box.
        let focused = self.focus == Focus::Search;
        self.field_bg(s, search, focused);
        self.hit(search, Hit::Search);
        let tr = Rect::new(search.l + 10.0, search.t, search.r - 34.0, search.b);
        self.geo.search_text = tr;
        if self.search.text.is_empty() && !focused {
            let base = self.tab().loc.base().map(folder_name);
            let hint = match (&self.tab().loc, base) {
                (Location::ThisPc, _) => "Search This PC".to_string(),
                (_, Some(n)) => format!("Search {n}"),
                _ => "Search".to_string(),
            };
            s.text(tr, hint, Font::Ui, th.text_dim, Align::Left);
        } else {
            let caret = self.caret_on && focused;
            let mut f = std::mem::take(&mut self.search);
            f.paint(s, tr, m, Font::Ui, &th, focused, caret);
            self.search = f;
        }
        let icon = Rect::xywh(search.r - 32.0, search.t + 2.0, 28.0, BUTTON - 4.0);
        if self.search.text.is_empty() {
            s.glyph(icon, glyph::SEARCH, Font::Icon12, th.text_dim);
        } else {
            self.button_bg(s, icon, Hit::SearchClear);
            s.glyph(icon, glyph::CANCEL, Font::Icon10, th.text_dim);
            self.hit(icon, Hit::SearchClear);
        }
    }

    fn field_bg(&self, s: &mut Scene, r: Rect, focused: bool) {
        let th = &self.theme;
        s.round(r, HQ, th.field);
        s.stroke(r, HQ, 1.0, th.field_border);
        if focused {
            // Fluent text box: accent underline when focused.
            s.clip(Rect::new(r.l, r.b - 2.0, r.r, r.b));
            s.round(r, HQ, th.field_focus);
            s.unclip();
        }
    }

    fn crumbs(&self) -> Vec<(Location, String)> {
        let mut out = vec![(Location::ThisPc, "This PC".to_string())];
        let path = match &self.tab().loc {
            Location::ThisPc => return out,
            Location::Folder(p) => p.clone(),
            Location::Search { root, .. } => root.clone(),
        };
        let mut chain: Vec<std::path::PathBuf> = path.ancestors().map(|p| p.to_path_buf()).collect();
        chain.reverse();
        if path.to_string_lossy().starts_with(r"\\") {
            out.clear();
        }
        for p in chain {
            if p.as_os_str().is_empty() {
                continue;
            }
            // UNC: skip the bare "\\server" ancestor; "\\server\share" is the root.
            let s = p.to_string_lossy();
            if s.starts_with(r"\\") && s.trim_end_matches('\\').matches('\\').count() < 3 {
                continue;
            }
            out.push((Location::Folder(p.clone()), folder_name(&p)));
        }
        if let Location::Search { root, .. } = &self.tab().loc {
            out.push((
                self.tab().loc.clone(),
                format!("Search Results in {}", folder_name(root)),
            ));
        }
        out
    }

    fn paint_crumbs(&mut self, s: &mut Scene, m: &dyn Measure, addr: Rect) {
        let th = self.theme.clone();
        let crumbs = self.crumbs();
        let icon = self.tab().icon;
        let ir = Rect::xywh(addr.l + 8.0, addr.cy() - 8.0, 16.0, 16.0);
        if icon >= 0 {
            let key = self.icon_key(icon, 16.0);
            s.image(ir, key, 1.0);
        } else {
            s.glyph(ir, glyph::SEARCH, Font::Icon12, th.text);
        }
        let chev_w = 20.0;
        let widths: Vec<f32> = crumbs
            .iter()
            .map(|(_, l)| m.text_width(l, Font::Ui) + 12.0)
            .collect();
        let start = ir.r + 4.0;
        let avail = addr.r - 8.0 - start;
        // Drop leading crumbs until the rest fit (with room for the overflow button).
        let mut first = 0;
        let total = |from: usize| -> f32 {
            widths[from..].iter().sum::<f32>() + chev_w * (crumbs.len() - from) as f32
        };
        while first + 1 < crumbs.len() && total(first) + if first > 0 { 28.0 } else { 0.0 } > avail {
            first += 1;
        }
        self.geo.crumbs = crumbs.iter().map(|(l, _)| l.clone()).collect();
        self.geo.crumb_hidden = crumbs[..first].iter().map(|(l, _)| l.clone()).collect();
        self.geo.crumb_rects = vec![Rect::default(); crumbs.len()];
        let mut x = start;
        let h = 24.0;
        let y = addr.cy() - h / 2.0;
        if first > 0 {
            let r = Rect::xywh(x, y, 26.0, h);
            self.button_bg(s, r, Hit::CrumbOverflow);
            s.text(r, "\u{00AB}", Font::Ui, th.text, Align::Center);
            self.hit(r, Hit::CrumbOverflow);
            x += 28.0;
        }
        let last = crumbs.len() - 1;
        for i in first..crumbs.len() {
            let cr = Rect::xywh(x, y, widths[i].min(addr.r - 8.0 - x).max(0.0), h);
            self.geo.crumb_rects[i] = cr;
            self.button_bg(s, cr, Hit::Crumb(i));
            s.text(cr.inset(6.0, 0.0), crumbs[i].1.clone(), Font::Ui, th.text, Align::Left);
            self.hit(cr, Hit::Crumb(i));
            x = cr.r;
            if i < last || matches!(crumbs[i].0, Location::Folder(_) | Location::ThisPc) {
                let chev = Rect::xywh(x, y, chev_w, h);
                if chev.r <= addr.r - 4.0 {
                    self.button_bg(s, chev, Hit::CrumbChevron(i));
                    s.glyph(chev, glyph::CHEVRON_R, Font::Icon8, th.text_dim);
                    self.hit(chev, Hit::CrumbChevron(i));
                }
                x += chev_w;
            }
        }
    }

    // ---- command bar --------------------------------------------------------------------------

    fn paint_commands(&mut self, s: &mut Scene, m: &dyn Measure, w: f32) {
        let th = self.theme.clone();
        let y = TAB_H + ADDR_H + (CMD_H - BUTTON) / 2.0;
        let t = self.tab();
        let has_sel = t.selection_count() > 0;
        let can_write = t.loc.base().is_some();
        let one = t.selection_count() == 1;
        let searching = matches!(t.loc, Location::Search { .. });
        let mut items: Vec<(Option<(Cmd, char, Option<&str>, bool, bool)>, ())> = vec![
            (Some((Cmd::New, glyph::ADD, Some("New"), true, can_write)), ()),
            (None, ()),
            (Some((Cmd::Cut, glyph::CUT, None, false, has_sel && can_write)), ()),
            (Some((Cmd::Copy, glyph::COPY, None, false, has_sel)), ()),
            (Some((Cmd::Paste, glyph::PASTE, None, false, can_write && crate::shell::clipboard_has_files())), ()),
            (Some((Cmd::Rename, glyph::RENAME, None, false, one && can_write)), ()),
            (Some((Cmd::Delete, glyph::DELETE, None, false, has_sel && can_write)), ()),
            (None, ()),
            (Some((Cmd::Sort, glyph::SORT, Some("Sort"), true, true)), ()),
            (Some((Cmd::View, glyph::VIEW, Some("View"), true, true)), ()),
        ];
        if searching {
            items.push((None, ()));
            items.push((Some((Cmd::SearchOptions, glyph::SEARCH_OPTIONS, Some("Search options"), true, true)), ()));
            items.push((Some((Cmd::CloseSearch, glyph::CANCEL, Some("Close search"), false, true)), ()));
            items.push((None, ()));
        }
        items.push((Some((Cmd::More, glyph::MORE, None, false, true)), ()));
        let mut x = 12.0;
        self.geo.cmds.clear();
        for (item, _) in items {
            let Some((cmd, g, label, dropdown, enabled)) = item else {
                s.line(x + 4.5, y + 6.0, x + 4.5, y + BUTTON - 6.0, 1.0, th.divider);
                x += 10.0;
                continue;
            };
            let mut bw = 40.0;
            if let Some(l) = label {
                bw = 12.0 + 16.0 + 8.0 + m.text_width(l, Font::Ui) + 10.0;
                if dropdown {
                    bw += 16.0;
                }
            }
            if x + bw > w - 12.0 {
                break;
            }
            let r = Rect::xywh(x, y, bw, BUTTON);
            let hit = Hit::Command(cmd);
            let fg = if enabled { th.text } else { th.text_disabled };
            if enabled {
                self.button_bg(s, r, hit);
                self.hit(r, hit);
            }
            if let Some(l) = label {
                let icon_color = if cmd == Cmd::New && enabled { th.accent } else { fg };
                s.glyph(Rect::xywh(r.l + 10.0, r.t, 16.0, BUTTON), g, Font::Icon16, icon_color);
                s.text(Rect::new(r.l + 34.0, r.t, r.r, r.b), l, Font::Ui, fg, Align::Left);
                if dropdown {
                    s.glyph(Rect::xywh(r.r - 22.0, r.t, 16.0, BUTTON), glyph::CHEVRON_D, Font::Icon8, fg);
                }
            } else {
                s.glyph(r, g, Font::Icon16, fg);
            }
            self.geo.cmds.push((cmd, r));
            x += bw + 2.0;
        }
        // Right side: the Preview pane toggle (checked look when on), like Explorer's "Details".
        let label = "Preview";
        let bw = 12.0 + 16.0 + 8.0 + m.text_width(label, Font::Ui) + 12.0;
        let r = Rect::xywh(w - 12.0 - bw, y, bw, BUTTON);
        if r.l > x + 8.0 {
            let hit = Hit::Command(Cmd::Preview);
            if self.cfg.show_preview {
                s.round(r, HQ, th.item_sel);
            }
            self.button_bg(s, r, hit);
            s.glyph(Rect::xywh(r.l + 10.0, r.t, 16.0, BUTTON), glyph::PREVIEW, Font::Icon16, th.text);
            s.text(Rect::new(r.l + 34.0, r.t, r.r, r.b), label, Font::Ui, th.text, Align::Left);
            self.hit(r, hit);
            self.geo.cmds.push((Cmd::Preview, r));
        }
    }

    // ---- navigation pane ----------------------------------------------------------------------

    fn paint_nav(&mut self, s: &mut Scene, m: &dyn Measure, area: Rect) {
        let th = self.theme.clone();
        self.geo.nav = area;
        self.hit(area, Hit::NavArea);
        let rh = nav_row_h(self.cfg.compact);
        let gap = 12.0;
        let content: f32 = 8.0
            + self.nav.rows.iter().map(|r| rh + if r.gap { gap } else { 0.0 }).sum::<f32>()
            + 8.0;
        let max = (content - area.h()).max(0.0);
        self.nav.scroll = self.nav.scroll.clamp(0.0, max);
        let cur = self.tab().loc.clone();
        s.clip(area);
        let mut y = area.t + 8.0 - self.nav.scroll;
        let drop = self.drop_target.clone();
        for i in 0..self.nav.rows.len() {
            let row = self.nav.rows[i].clone();
            if row.gap {
                let ly = y + gap / 2.0;
                s.line(area.l + 12.0, ly, area.r - 12.0, ly, 1.0, th.divider);
                y += gap;
            }
            let r = Rect::new(area.l + 4.0, y, area.r - 4.0, y + rh);
            y += rh;
            if r.b < area.t || r.t > area.b {
                continue;
            }
            let selected = row.loc == cur;
            let dropping = matches!((&row.loc, &drop), (Location::Folder(p), Some(d)) if p == d);
            let hovered = self.hover == Hit::Nav(i) || self.hover == Hit::NavExpand(i);
            if selected || dropping {
                s.round(r, HQ, if hovered { th.item_sel_hover } else { th.item_sel });
            } else if hovered {
                s.round(r, HQ, th.item_hover);
            }
            let visible = r.intersect(&area);
            if !visible.is_empty() {
                self.hit(visible, Hit::Nav(i));
            }
            let x = r.l + 4.0 + row.depth as f32 * 16.0;
            let chev = Rect::xywh(x, r.t, 16.0, rh);
            if row.expandable && (hovered || self.hover == Hit::NavArea || row.expanded || true) {
                let g = if row.expanded { glyph::CHEVRON_D } else { glyph::CHEVRON_R };
                s.glyph(chev, g, Font::Icon8, th.text_dim);
                let visible = chev.intersect(&area);
                if !visible.is_empty() {
                    self.hit(visible, Hit::NavExpand(i));
                }
            }
            let ir = Rect::xywh(chev.r + 4.0, r.cy() - 8.0, 16.0, 16.0);
            let key = self.icon_key(row.icon, 16.0);
            s.image(ir, key, 1.0);
            let fg = if selected { th.item_sel_text } else { th.text };
            let text_r = if row.pinned { r.r - 28.0 } else { r.r - 4.0 };
            s.text(Rect::new(ir.r + 8.0, r.t, text_r, r.b), row.label.clone(), Font::Ui, fg, Align::Left);
            if row.pinned {
                s.glyph(Rect::xywh(r.r - 26.0, r.t, 20.0, rh), glyph::PIN, Font::Icon10, th.text_dim);
            }
        }
        s.unclip();
        // Overlay scrollbar.
        if max > 0.0 {
            let track = Rect::new(area.r - SCROLL_W, area.t + 2.0, area.r, area.b - 2.0);
            let thumb_h = (area.h() / content * track.h()).max(24.0);
            let ty = track.t + (track.h() - thumb_h) * (self.nav.scroll / max);
            let thumb = Rect::new(track.l, ty, track.r, ty + thumb_h);
            self.geo.nav_track = track;
            self.geo.nav_thumb = thumb;
            let hot = track.contains(self.mouse.0, self.mouse.1)
                || matches!(self.drag, Drag::Thumb { nav: true, .. });
            self.paint_scrollbar(s, track, thumb, hot, false);
            self.hit(thumb, Hit::NavScrollThumb);
        } else {
            self.geo.nav_track = Rect::default();
        }
        let _ = m;
    }

    fn paint_scrollbar(&self, s: &mut Scene, track: Rect, thumb: Rect, hot: bool, horizontal: bool) {
        let th = &self.theme;
        if hot {
            s.round(track, HQ, th.content.with_alpha(0.9));
            let t = if horizontal { thumb.inset(0.0, 3.0) } else { thumb.inset(3.0, 0.0) };
            s.round(t, 3.0, th.scroll);
        } else {
            let t = if horizontal {
                Rect::new(thumb.l, thumb.b - 4.0, thumb.r, thumb.b - 2.0)
            } else {
                Rect::new(thumb.r - 4.0, thumb.t, thumb.r - 2.0, thumb.b)
            };
            s.round(t, 1.0, th.scroll.with_alpha(0.8));
        }
    }

    // ---- items --------------------------------------------------------------------------------

    fn paint_files(&mut self, s: &mut Scene, m: &dyn Measure, area: Rect) {
        let th = self.theme.clone();
        self.geo.files = area;
        let mode = self.tab().view.mode;
        let details = mode == ViewMode::Details;
        let items_area = if details {
            Rect::new(area.l, area.t + HEADER_H, area.r, area.b)
        } else {
            area
        };
        self.geo.items = Rect::new(items_area.l, items_area.t, items_area.r, items_area.b);
        self.hit(items_area, Hit::Items);
        self.clamp_scroll();
        let g = self.grid();
        let scroll = self.tab().scroll;

        if details {
            self.paint_header(s, Rect::new(area.l, area.t, area.r, area.t + HEADER_H));
        }
        s.clip(items_area);
        let t = self.tab();
        if t.order.is_empty() {
            let msg = if t.loading {
                (t.load_started.elapsed().as_millis() > 200).then_some("Working on it...")
            } else if let Some(e) = &t.error {
                Some(e.as_str())
            } else if matches!(t.loc, Location::Search { .. }) {
                Some("No items match your search.")
            } else {
                Some("This folder is empty.")
            };
            if let Some(msg) = msg {
                s.text(
                    Rect::new(items_area.l + 16.0, items_area.t + 12.0, items_area.r - 16.0, items_area.t + 40.0),
                    msg,
                    Font::Ui,
                    th.text_dim,
                    Align::Center,
                );
            }
        }
        let range = g.visible(scroll);
        let rename_pos = self.rename.as_ref().map(|(p, _)| *p);
        for pos in range {
            let r = g.screen(pos, scroll);
            let hr = if details {
                Rect::new(r.l + 4.0, r.t, r.r.min(items_area.r - 4.0), r.b)
            } else {
                r.inset(2.0, 1.0)
            };
            self.paint_item(s, m, &g, pos, r, hr, rename_pos == Some(pos));
            // Rows scrolled partly out of view must not catch clicks meant for the header or
            // toolbar above them.
            let visible = hr.intersect(&items_area);
            if !visible.is_empty() {
                self.hits.push((visible, Hit::Item(pos)));
            }
        }
        // Rubber band.
        if let Drag::Band { x0, y0, x1, y1, .. } = &self.drag {
            let (sx0, sy0) = self.content_to_screen(*x0, *y0);
            let r = Rect::span(sx0, sy0, *x1, *y1);
            s.fill(r, th.band);
            s.stroke(r, 0.0, 1.0, th.band_border);
        }
        s.unclip();
        // Scrollbar.
        let max = g.max_scroll();
        if max > 0.0 {
            let horizontal = g.horizontal;
            let (track, thumb) = if horizontal {
                let track = Rect::new(items_area.l + 2.0, items_area.b - SCROLL_W, items_area.r - 2.0, items_area.b);
                let len = (g.viewport() / g.extent() * track.w()).max(24.0);
                let x = track.l + (track.w() - len) * (scroll / max);
                (track, Rect::new(x, track.t, x + len, track.b))
            } else {
                let track = Rect::new(items_area.r - SCROLL_W, items_area.t + 2.0, items_area.r, items_area.b - 2.0);
                let len = (g.viewport() / g.extent() * track.h()).max(24.0);
                let y = track.t + (track.h() - len) * (scroll / max);
                (track, Rect::new(track.l, y, track.r, y + len))
            };
            self.geo.scroll_track = track;
            self.geo.scroll_thumb = thumb;
            let hot = track.contains(self.mouse.0, self.mouse.1)
                || matches!(self.drag, Drag::Thumb { nav: false, .. });
            self.paint_scrollbar(s, track, thumb, hot, horizontal);
            self.hit(track, Hit::ScrollTrack);
            self.hit(thumb, Hit::ScrollThumb);
        } else {
            self.geo.scroll_track = Rect::default();
        }
    }

    pub fn content_to_screen(&self, x: f32, y: f32) -> (f32, f32) {
        let g = self.grid();
        let scroll = self.tab().scroll;
        if g.horizontal {
            (x + g.area.l - scroll, y + g.area.t)
        } else {
            (x + g.area.l, y + g.area.t - scroll)
        }
    }

    pub fn screen_to_content(&self, x: f32, y: f32) -> (f32, f32) {
        let g = self.grid();
        let scroll = self.tab().scroll;
        if g.horizontal {
            (x - g.area.l + scroll, y - g.area.t)
        } else {
            (x - g.area.l, y - g.area.t + scroll)
        }
    }

    fn paint_header(&mut self, s: &mut Scene, r: Rect) {
        let th = self.theme.clone();
        let view = self.tab().view.clone();
        let mut x = r.l + 4.0;
        self.geo.cols.clear();
        self.hit(r, Hit::HeaderArea);
        s.clip(r);
        for (i, (col, w)) in view.cols.iter().enumerate() {
            let cr = Rect::new(x, r.t, x + w, r.b);
            self.geo.cols.push((cr.l, cr.r));
            let hot = self.hover == Hit::Header(i);
            if hot {
                s.fill(cr, th.hover);
            }
            // Always-visible separators show where columns can be resized.
            s.line(cr.r - 0.5, r.t + 8.0, cr.r - 0.5, r.b - 8.0, 1.0, th.divider);
            let align = if col.right_aligned() { Align::Right } else { Align::Left };
            let label_r = if col.right_aligned() {
                Rect::new(cr.l + 8.0, cr.t, cr.r - 10.0, cr.b)
            } else {
                Rect::new(cr.l + 10.0, cr.t, cr.r - 8.0, cr.b)
            };
            s.text(label_r, col.title(), Font::Ui, th.text_dim, align);
            if view.sort == *col {
                let g = if view.asc { glyph::CHEVRON_U } else { glyph::CHEVRON_D };
                s.glyph(Rect::xywh(cr.cx() - 6.0, r.t + 2.0, 12.0, 10.0), g, Font::Icon8, th.text_dim);
            }
            self.hits.push((cr, Hit::Header(i)));
            self.hits.push((Rect::new(cr.r - 4.0, r.t, cr.r + 4.0, r.b), Hit::HeaderEdge(i)));
            x += w;
        }
        s.unclip();
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_item(
        &mut self,
        s: &mut Scene,
        m: &dyn Measure,
        g: &Grid,
        pos: usize,
        cell: Rect,
        hr: Rect,
        renaming: bool,
    ) {
        let th = self.theme.clone();
        let t = self.tab();
        let idx = t.order[pos] as usize;
        let e: Entry = t.entries[idx].clone();
        let selected = t.sel[idx];
        let focused = t.focus == Some(pos);
        let hovered = self.hover == Hit::Item(pos);
        let dropping = self.drop_target.as_deref() == Some(e.path.as_path());
        let cut = self.cut.contains(&e.path);
        let show_ext = self.cfg.show_extensions;
        if selected || dropping {
            s.round(hr, HQ, if hovered { th.item_sel_hover } else { th.item_sel });
            if th.item_sel_border.a > 0.0 && focused && self.window_active {
                s.stroke(hr, HQ, 1.0, th.item_sel_border);
            }
        } else if hovered {
            s.round(hr, HQ, th.item_hover);
        } else if focused && self.focus == Focus::Items && self.window_active && t.selection_count() == 0 {
            s.stroke(hr, HQ, 1.0, th.divider);
        }
        let fg = if selected { th.item_sel_text } else { th.text };
        let opacity = if cut || e.hidden() { 0.5 } else { 1.0 };
        let name = e.display_name(show_ext).to_string();
        match g.mode {
            ViewMode::Details => {
                let cols = self.tab().view.cols.clone();
                let mut x = cell.l + 4.0;
                for (i, (col, w)) in cols.iter().enumerate() {
                    let cr = Rect::new(x, cell.t, x + w, cell.b);
                    x += w;
                    if i == 0 {
                        let ir = Rect::xywh(cr.l + 10.0, cr.cy() - 8.0, 16.0, 16.0);
                        self.paint_icon(s, &e, ir, 16.0, opacity);
                        let tr = Rect::new(ir.r + 8.0, cr.t, cr.r - 6.0, cr.b);
                        if renaming {
                            self.paint_rename(s, m, Rect::new(tr.l - 3.0, cr.t + 3.0, (tr.l + 300.0).max(cr.r), cr.b - 3.0));
                        } else {
                            s.text(tr, name.clone(), Font::Ui, fg, Align::Left);
                        }
                        continue;
                    }
                    let text = column_text(&e, *col);
                    if text.is_empty() {
                        continue;
                    }
                    let (align, tr) = if col.right_aligned() {
                        (Align::Right, Rect::new(cr.l + 6.0, cr.t, cr.r - 10.0, cr.b))
                    } else {
                        (Align::Left, Rect::new(cr.l + 10.0, cr.t, cr.r - 6.0, cr.b))
                    };
                    s.text(tr, text, Font::Ui, if selected { fg } else { th.text_dim }, align);
                }
            }
            ViewMode::List | ViewMode::Small => {
                let ir = Rect::xywh(hr.l + 6.0, hr.cy() - 8.0, 16.0, 16.0);
                self.paint_icon(s, &e, ir, 16.0, opacity);
                let tr = Rect::new(ir.r + 6.0, hr.t, hr.r - 4.0, hr.b);
                if renaming {
                    self.paint_rename(s, m, Rect::new(tr.l - 3.0, hr.t + 2.0, tr.r + 40.0, hr.b - 2.0));
                } else {
                    s.text(tr, name, Font::Ui, fg, Align::Left);
                }
            }
            ViewMode::Tiles => {
                let ir = Rect::xywh(hr.l + 8.0, hr.cy() - 24.0, 48.0, 48.0);
                self.paint_icon(s, &e, ir, 48.0, opacity);
                let x = ir.r + 10.0;
                let tx = Rect::new(x, hr.t, hr.r - 8.0, hr.b);
                if let Some((total, free)) = e.drive {
                    s.text(Rect::new(tx.l, hr.t + 8.0, tx.r, hr.t + 26.0), name, Font::Ui, fg, Align::Left);
                    if total > 0 {
                        let bar = Rect::new(tx.l, hr.t + 30.0, tx.r.min(tx.l + 190.0), hr.t + 42.0);
                        let used = 1.0 - free as f32 / total as f32;
                        s.fill(bar, th.usage_track);
                        let color = if (free as f32) < total as f32 * 0.1 { th.usage_full } else { th.usage };
                        s.fill(Rect::new(bar.l, bar.t, bar.l + bar.w() * used, bar.b), color);
                        s.stroke(bar, 0.0, 1.0, th.usage_border);
                        let info = format!("{} free of {}", format_bytes(free), format_bytes(total));
                        s.text(Rect::new(tx.l, hr.t + 44.0, tx.r, hr.t + 62.0), info, Font::Small, th.text_dim, Align::Left);
                    }
                } else if renaming {
                    self.paint_rename(s, m, Rect::new(tx.l - 3.0, hr.t + 6.0, tx.r, hr.t + 30.0));
                } else {
                    s.text(Rect::new(tx.l, hr.t + 8.0, tx.r, hr.t + 26.0), name, Font::Ui, fg, Align::Left);
                    s.text(Rect::new(tx.l, hr.t + 26.0, tx.r, hr.t + 44.0), e.type_name.to_string(), Font::Small, th.text_dim, Align::Left);
                    if !e.is_dir {
                        s.text(Rect::new(tx.l, hr.t + 43.0, tx.r, hr.t + 61.0), format_bytes(e.size), Font::Small, th.text_dim, Align::Left);
                    }
                }
            }
            _ => {
                let isz = g.mode.icon();
                let ir = Rect::xywh(cell.cx() - isz / 2.0, cell.t + 6.0, isz, isz);
                self.paint_icon(s, &e, ir, isz, opacity);
                let tr = Rect::new(hr.l + 4.0, ir.b + 4.0, hr.r - 4.0, hr.b - 2.0);
                if renaming {
                    self.paint_rename(s, m, Rect::new(hr.l - 6.0, tr.t, hr.r + 6.0, tr.t + 24.0));
                } else {
                    s.text_wrap(tr, name, Font::Ui, fg);
                }
            }
        }
    }

    fn paint_icon(&self, s: &mut Scene, e: &Entry, r: Rect, dip: f32, opacity: f32) {
        if let Thumb::Ready(id, _) = e.thumb {
            if self.tab().view.mode.shows_thumbnails() {
                s.image(r, ImageKey::Thumb(id), opacity);
                return;
            }
        }
        if e.icon >= 0 {
            s.image(r, self.icon_key(e.icon, dip), opacity);
        }
    }

    fn paint_rename(&mut self, s: &mut Scene, m: &dyn Measure, r: Rect) {
        let th = self.theme.clone();
        s.fill(r, th.field.with_alpha(1.0).max_alpha(if th.dark { rgba(32, 32, 32, 1.0) } else { rgba(255, 255, 255, 1.0) }));
        s.stroke(r, 2.0, 1.0, th.accent);
        let tr = Rect::new(r.l + 4.0, r.t, r.r - 4.0, r.b);
        self.geo.rename_text = tr;
        self.hits.push((r, Hit::Rename));
        let caret = self.caret_on;
        if let Some((_, f)) = self.rename.as_mut() {
            f.paint(s, tr, m, Font::Ui, &th, true, caret);
        }
    }

    // ---- status bar ---------------------------------------------------------------------------

    fn paint_status(&mut self, s: &mut Scene, m: &dyn Measure, r: Rect) {
        let th = self.theme.clone();
        let t = self.tab();
        let n = t.order.len();
        let mut text = format!("{} {}", n, if n == 1 { "item" } else { "items" });
        let k = t.selection_count();
        if k > 0 {
            text.push_str(&format!("    {} {} selected", k, if k == 1 { "item" } else { "items" }));
            let files: Vec<&Entry> = t
                .order
                .iter()
                .filter(|&&i| t.sel[i as usize])
                .map(|&i| &t.entries[i as usize])
                .collect();
            if files.iter().all(|e| !e.is_dir) {
                let total: u64 = files.iter().map(|e| e.size).sum();
                text.push_str(&format!("  {}", format_bytes(total)));
            }
        }
        if matches!(t.loc, Location::Search { .. }) {
            if let Some(st) = crate::index::status() {
                text.push_str(&format!("    {st}…"));
            } else if t.loading {
                text.push_str("    Searching...");
            }
            if n >= crate::index::MAX_RESULTS {
                text.push_str("    (showing the first 200,000)");
            }
        }
        s.text(Rect::new(r.l + 12.0, r.t, r.r - 90.0, r.b), text, Font::Small, th.text, Align::Left);
        let mode = t.view.mode;
        let b1 = Rect::xywh(r.r - 66.0, r.t + 1.0, 28.0, r.h() - 2.0);
        let b2 = Rect::xywh(r.r - 36.0, r.t + 1.0, 28.0, r.h() - 2.0);
        for (b, hit, g, on) in [
            (b1, Hit::StatusDetails, glyph::DETAILS, mode == ViewMode::Details),
            (b2, Hit::StatusTiles, glyph::TILES, mode == ViewMode::Large),
        ] {
            if on {
                s.round(b, HQ, th.item_sel);
            }
            self.button_bg(s, b, hit);
            s.glyph(b, g, Font::Icon12, th.text);
            self.hit(b, hit);
        }
        let _ = m;
    }
}

trait MaxAlpha {
    fn max_alpha(self, opaque: Color) -> Color;
}

impl MaxAlpha for Color {
    /// Composites a translucent fill over an opaque base (rename box must hide what's under it).
    fn max_alpha(self, base: Color) -> Color {
        let a = self.a;
        Color {
            r: self.r * a + base.r * (1.0 - a),
            g: self.g * a + base.g * (1.0 - a),
            b: self.b * a + base.b * (1.0 - a),
            a: 1.0,
        }
    }
}

pub fn column_text(e: &Entry, col: Column) -> String {
    match col {
        Column::Name => e.name.clone(),
        Column::Modified => format_date(e.modified),
        Column::Type => e.type_name.to_string(),
        Column::Size => {
            if e.is_dir {
                String::new()
            } else {
                format_kb(e.size)
            }
        }
        Column::Folder => e
            .path
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default(),
        Column::Total => e.drive.map(|d| format_bytes(d.0)).unwrap_or_default(),
        Column::Free => e.drive.map(|d| format_bytes(d.1)).unwrap_or_default(),
    }
}
