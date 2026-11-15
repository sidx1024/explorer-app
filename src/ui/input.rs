//! Input handling and the actions behind buttons, menus and shortcuts.

use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::fs::Column;
use crate::scene::{Font, Measure};
use crate::shell::{self, Op};

use super::layout::*;
use super::tab::Location;
use super::textfield::{FieldAction, TextField};
use super::{
    App, Button, Cmd, Cursor, Drag, Effect, Focus, Hit, Input, MenuKind, Mods, ViewMode,
    TIMER_CARET, TIMER_LOADING, TIMER_REFRESH, TIMER_SEARCH,
};

// Menu command ids.
const M_VIEW: u32 = 100;
const M_COMPACT: u32 = 120;
const M_HIDDEN: u32 = 121;
const M_EXT: u32 = 122;
const M_PREVIEW: u32 = 123;
const M_SORT: u32 = 200;
const M_ASC: u32 = 220;
const M_DESC: u32 = 221;
const M_NEW_FOLDER: u32 = 300;
const M_NEW_TEXT: u32 = 301;
const M_SELECT_ALL: u32 = 400;
const M_SELECT_NONE: u32 = 401;
const M_INVERT: u32 = 402;
const M_PROPERTIES: u32 = 403;
const M_TERMINAL: u32 = 404;
const M_PIN: u32 = 405;
const M_COPY_PATH: u32 = 406;
const M_PROTECTED: u32 = 407;
const M_THUMB_STORE: u32 = 408;
const M_THEME: u32 = 410; // +0 system, +1 light, +2 dark
const M_MICA: u32 = 414;
const M_NEW_WINDOW: u32 = 415;
const M_CRUMB: u32 = 500;
const M_SUB_ALL: u32 = 800;
const M_SUB_HERE: u32 = 801;
const M_DATE: u32 = 810;
const M_KIND: u32 = 830;
const M_SIZE: u32 = 850;
const M_SYSTEM: u32 = 870;
const M_INDEX: u32 = 872;
const M_TAB_CLOSE: u32 = 600;
const M_TAB_OTHERS: u32 = 601;
const M_TAB_DUP: u32 = 602;
pub const M_OPEN_TAB: u32 = 700;
pub const M_OPEN_WINDOW: u32 = 701;
pub const M_ITEM_COPY_PATH: u32 = 702;
pub const M_ITEM_PIN: u32 = 703;
pub const M_REFRESH: u32 = 704;
pub const M_PASTE: u32 = 705;

const VK_BACK: u32 = 0x08;
const VK_TAB: u32 = 0x09;
const VK_RETURN: u32 = 0x0D;
const VK_ESCAPE: u32 = 0x1B;
const VK_SPACE: u32 = 0x20;
const VK_PRIOR: u32 = 0x21;
const VK_HOME: u32 = 0x24;
const VK_LEFT: u32 = 0x25;
const VK_UP: u32 = 0x26;
const VK_RIGHT: u32 = 0x27;
const VK_DOWN: u32 = 0x28;
const VK_DELETE: u32 = 0x2E;
const VK_APPS: u32 = 0x5D;
const VK_F2: u32 = 0x71;
const VK_F3: u32 = 0x72;
const VK_F4: u32 = 0x73;
const VK_F5: u32 = 0x74;
const VK_ADD: u32 = 0x6B;

fn item(id: u32, label: impl Into<String>, checked: bool, enabled: bool) -> (u32, String, bool, bool) {
    (id, label.into(), checked, enabled)
}

fn sep() -> (u32, String, bool, bool) {
    (0, "-".into(), false, true)
}

impl App {
    pub fn cursor(&self) -> Cursor {
        match (&self.drag, self.hover) {
            (Drag::Column { .. } | Drag::Splitter { .. } | Drag::PreviewSplitter { .. }, _) => Cursor::SizeWE,
            (_, Hit::HeaderEdge(_) | Hit::NavSplitter | Hit::PreviewSplitter) => Cursor::SizeWE,
            (_, Hit::Search | Hit::Rename) => Cursor::IBeam,
            (_, Hit::Address) if self.address.is_some() => Cursor::IBeam,
            _ => Cursor::Arrow,
        }
    }

    pub fn input(&mut self, ev: Input, m: &dyn Measure) {
        match ev {
            Input::Move { x, y } => self.on_move(x, y, m),
            Input::Down { x, y, button, double, mods } => self.on_down(x, y, button, double, mods, m),
            Input::Up { x, y, button, mods } => self.on_up(x, y, button, mods),
            Input::Wheel { x, y, delta, horizontal, mods } => self.on_wheel(x, y, delta, horizontal, mods),
            Input::Leave => {
                if matches!(self.drag, Drag::None) {
                    self.hover = Hit::None;
                    self.mouse = (-1.0, -1.0);
                }
            }
            Input::Key { vk, mods } => self.on_key(vk, mods),
            Input::Char(c) => self.on_char(c),
            Input::CaptureLost => {
                self.drag = Drag::None;
            }
        }
        self.invalidate();
    }

    fn capture(&mut self, on: bool) {
        self.effects.push(Effect::Capture(on));
    }

    // ---- mouse --------------------------------------------------------------------------------

    fn on_move(&mut self, x: f32, y: f32, m: &dyn Measure) {
        self.mouse = (x, y);
        self.hover = self.hit_test(x, y);
        match &mut self.drag {
            Drag::Pending { x: x0, y: y0, .. } => {
                if (x - *x0).abs() > 4.0 || (y - *y0).abs() > 4.0 {
                    let paths = self.tab().selected_paths();
                    self.drag = Drag::None;
                    self.capture(false);
                    if !paths.is_empty() && self.tab().loc != Location::ThisPc {
                        self.effects.push(Effect::Drag(paths));
                    }
                }
            }
            Drag::Band { .. } => self.update_band(x, y),
            Drag::Column { col, x: x0, w } => {
                let (col, nw) = (*col, (*w + x - *x0).max(40.0));
                if let Some(c) = self.tab_mut().view.cols.get_mut(col) {
                    c.1 = nw;
                }
            }
            Drag::Splitter { x: x0, w } => {
                self.cfg.nav_width = (*w + x - *x0).clamp(NAV_MIN, NAV_MAX);
            }
            Drag::PreviewSplitter { x: x0, w } => {
                self.cfg.preview_width = (*w - (x - *x0)).max(super::preview_pane::MIN_W);
            }
            Drag::Thumb { y: y0, scroll, nav } => {
                let (y0, scroll, nav) = (*y0, *scroll, *nav);
                if nav {
                    let track = self.geo.nav_track;
                    let thumb = self.geo.nav_thumb;
                    let rh = nav_row_h(self.cfg.compact);
                    let content: f32 = 16.0 + self.nav.rows.iter().map(|r| rh + if r.gap { 12.0 } else { 0.0 }).sum::<f32>();
                    let max = (content - self.geo.nav.h()).max(0.0);
                    let range = (track.h() - thumb.h()).max(1.0);
                    self.nav.scroll = (scroll + (y - y0) / range * max).clamp(0.0, max);
                } else {
                    let g = self.grid();
                    let (track, thumb) = (self.geo.scroll_track, self.geo.scroll_thumb);
                    let (delta, range) = if g.horizontal {
                        (x - y0, (track.w() - thumb.w()).max(1.0))
                    } else {
                        (y - y0, (track.h() - thumb.h()).max(1.0))
                    };
                    let max = g.max_scroll();
                    self.tab_mut().scroll = (scroll + delta / range * max).clamp(0.0, max);
                    self.request_thumbnails();
                }
            }
            Drag::Text { field } => {
                let field = *field;
                match field {
                    Focus::Address => {
                        let l = self.geo.address_text.l;
                        if let Some(f) = self.address.as_mut() {
                            f.drag_to(x - l, m, Font::Ui);
                        }
                    }
                    Focus::Search => {
                        let l = self.geo.search_text.l;
                        self.search.drag_to(x - l, m, Font::Ui);
                    }
                    Focus::Rename => {
                        let l = self.geo.rename_text.l;
                        if let Some((_, f)) = self.rename.as_mut() {
                            f.drag_to(x - l, m, Font::Ui);
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    fn update_band(&mut self, x: f32, y: f32) {
        let Drag::Band { x0, y0, base, x1, y1 } = &mut self.drag else {
            return;
        };
        *x1 = x;
        *y1 = y;
        let (bx0, by0) = (*x0, *y0);
        let base = base.clone();
        // Auto-scroll when dragging past the edge.
        let g = self.grid();
        let area = g.area;
        let step = if g.horizontal {
            if x > area.r { x - area.r } else if x < area.l { x - area.l } else { 0.0 }
        } else if y > area.b {
            y - area.b
        } else if y < area.t {
            y - area.t
        } else {
            0.0
        };
        if step != 0.0 {
            let t = self.tab_mut();
            t.scroll = (t.scroll + step).clamp(0.0, g.max_scroll());
        }
        let (cx, cy) = self.screen_to_content(x, y);
        let r = crate::scene::Rect::span(bx0, by0, cx, cy);
        let hits = self.grid().intersecting(r);
        let t = self.tab_mut();
        t.sel.clone_from(&base);
        for pos in hits {
            let i = t.order[pos] as usize;
            t.sel[i] = !base[i];
            t.focus = Some(pos);
        }
    }

    fn on_down(&mut self, x: f32, y: f32, button: Button, double: bool, mods: Mods, m: &dyn Measure) {
        self.mouse = (x, y);
        self.tooltip = None;
        let hit = self.hit_test(x, y);
        self.hover = hit;
        // Clicking elsewhere ends text editing (rename commits, like Explorer).
        if self.rename.is_some() && hit != Hit::Rename {
            self.commit_rename();
        }
        if self.address.is_some() && hit != Hit::Address {
            self.address = None;
            if self.focus == Focus::Address {
                self.focus = Focus::Items;
            }
        }
        match button {
            Button::Back => return self.go_back(),
            Button::Forward => return self.go_forward(),
            Button::Middle => {
                match hit {
                    Hit::Tab(i) => self.close_tab(i),
                    Hit::Item(pos) => {
                        if let Some(e) = self.tab().entry_at(pos) {
                            if e.is_dir {
                                let p = e.path.clone();
                                self.new_tab(Location::Folder(p));
                            }
                        }
                    }
                    Hit::Nav(i) => {
                        let loc = self.nav.rows[i].loc.clone();
                        self.new_tab(loc);
                    }
                    _ => {}
                }
                return;
            }
            Button::Right => {
                match hit {
                    Hit::Item(pos) => {
                        if !self.tab().is_selected(pos) {
                            self.tab_mut().select_only(pos);
                        }
                        self.tab_mut().focus = Some(pos);
                    }
                    Hit::Items => self.tab_mut().clear_selection(),
                    _ => {}
                }
                self.focus = if matches!(hit, Hit::Search) { Focus::Search } else { self.focus };
                return;
            }
            Button::Left => {}
        }
        match hit {
            Hit::Tab(i) => self.activate_tab(i),
            Hit::Address => {
                self.focus = Focus::Address;
                if self.address.is_none() {
                    let mut f = TextField::new(&self.address_text());
                    f.select_all();
                    self.address = Some(f);
                    self.start_caret();
                } else {
                    let l = self.geo.address_text.l;
                    if let Some(f) = self.address.as_mut() {
                        if double {
                            f.select_word_at(x - l, m, Font::Ui);
                        } else {
                            f.click(x - l, m, Font::Ui, mods.shift);
                        }
                    }
                    self.drag = Drag::Text { field: Focus::Address };
                    self.capture(true);
                }
            }
            Hit::Search => {
                let l = self.geo.search_text.l;
                if self.focus != Focus::Search {
                    self.focus = Focus::Search;
                    self.search.select_all();
                    self.start_caret();
                } else {
                    if double {
                        self.search.select_word_at(x - l, m, Font::Ui);
                    } else {
                        self.search.click(x - l, m, Font::Ui, mods.shift);
                    }
                    self.drag = Drag::Text { field: Focus::Search };
                    self.capture(true);
                }
            }
            Hit::Rename => {
                let l = self.geo.rename_text.l;
                if let Some((_, f)) = self.rename.as_mut() {
                    if double {
                        f.select_word_at(x - l, m, Font::Ui);
                    } else {
                        f.click(x - l, m, Font::Ui, mods.shift);
                    }
                }
                self.drag = Drag::Text { field: Focus::Rename };
                self.capture(true);
            }
            Hit::HeaderEdge(i) => {
                if double {
                    self.autofit_column(i, m);
                } else {
                    let w = self.tab().view.cols[i].1;
                    self.drag = Drag::Column { col: i, x, w };
                    self.capture(true);
                }
            }
            Hit::NavSplitter => {
                self.drag = Drag::Splitter { x, w: self.cfg.nav_width };
                self.capture(true);
            }
            Hit::PreviewSplitter => {
                let w = self.geo.preview.w();
                self.drag = Drag::PreviewSplitter { x, w };
                self.capture(true);
            }
            Hit::ScrollThumb => {
                let g = self.grid();
                self.drag = Drag::Thumb {
                    y: if g.horizontal { x } else { y },
                    scroll: self.tab().scroll,
                    nav: false,
                };
                self.capture(true);
            }
            Hit::NavScrollThumb => {
                self.drag = Drag::Thumb { y, scroll: self.nav.scroll, nav: true };
                self.capture(true);
            }
            Hit::ScrollTrack => {
                let g = self.grid();
                let thumb = self.geo.scroll_thumb;
                let before = if g.horizontal { x < thumb.l } else { y < thumb.t };
                let page = g.viewport() * 0.9;
                let t = self.tab_mut();
                t.scroll = (t.scroll + if before { -page } else { page }).clamp(0.0, g.max_scroll());
                self.request_thumbnails();
            }
            Hit::Item(pos) => {
                self.focus = Focus::Items;
                if double && !mods.ctrl && !mods.shift {
                    self.tab_mut().select_only(pos);
                    self.open_selection(mods);
                    return;
                }
                let t = self.tab_mut();
                let mut select_on_up = false;
                if mods.ctrl && !mods.shift {
                    let on = !t.is_selected(pos);
                    t.set_selected(pos, on);
                    t.focus = Some(pos);
                    t.anchor = Some(pos);
                } else if mods.shift {
                    let a = t.anchor.unwrap_or(0);
                    t.select_range(a, pos, mods.ctrl);
                    t.focus = Some(pos);
                } else if t.is_selected(pos) {
                    select_on_up = true;
                    t.focus = Some(pos);
                } else {
                    t.select_only(pos);
                }
                self.drag = Drag::Pending { x, y, pos, select_on_up };
                self.capture(true);
            }
            Hit::Items => {
                self.focus = Focus::Items;
                if !mods.ctrl && !mods.shift {
                    self.tab_mut().clear_selection();
                }
                let (cx, cy) = self.screen_to_content(x, y);
                let base = self.tab().sel.clone();
                self.drag = Drag::Band { x0: cx, y0: cy, base, x1: x, y1: y };
                self.capture(true);
            }
            Hit::Nav(_) | Hit::NavExpand(_) | Hit::NavArea => {
                self.focus = Focus::Nav;
                if hit.is_button() {
                    self.drag = Drag::Button(hit);
                    self.capture(true);
                }
            }
            h if h.is_button() => {
                if double && matches!(h, Hit::Command(_)) {
                    return;
                }
                self.drag = Drag::Button(h);
                self.capture(true);
            }
            _ => {}
        }
    }

    fn on_up(&mut self, x: f32, y: f32, button: Button, mods: Mods) {
        let hit = self.hit_test(x, y);
        if button == Button::Right {
            match hit {
                Hit::Item(_) => {
                    let paths = self.tab().selected_paths();
                    if !paths.is_empty() {
                        self.effects.push(Effect::ItemMenu { paths, x, y, extended: mods.shift });
                    }
                }
                Hit::Items | Hit::HeaderArea | Hit::Header(_) => {
                    let folder = self.tab().loc.folder().map(Path::to_path_buf);
                    self.effects.push(Effect::BackgroundMenu { folder, x, y });
                }
                Hit::Nav(i) => {
                    if let Location::Folder(p) = &self.nav.rows[i].loc {
                        self.effects.push(Effect::ItemMenu { paths: vec![p.clone()], x, y, extended: mods.shift });
                    }
                }
                Hit::Tab(i) => {
                    let items = vec![
                        item(M_TAB_DUP, "Duplicate tab", false, true),
                        sep(),
                        item(M_TAB_CLOSE, "Close tab", false, true),
                        item(M_TAB_OTHERS, "Close other tabs", false, self.tabs.len() > 1),
                    ];
                    self.effects.push(Effect::Menu { kind: MenuKind::TabMenu(i), x, y, items });
                }
                _ => {}
            }
            return;
        }
        if button != Button::Left {
            return;
        }
        let drag = std::mem::replace(&mut self.drag, Drag::None);
        if !matches!(drag, Drag::None) {
            self.capture(false);
        }
        match drag {
            Drag::Pending { pos, select_on_up: true, .. } => {
                self.tab_mut().select_only(pos);
            }
            Drag::Button(h) if h == hit => self.click(h, x, y, mods),
            Drag::Column { .. } => self.tab_mut().save_view(),
            Drag::Splitter { .. } | Drag::PreviewSplitter { .. } => self.effects.push(Effect::SaveConfig),
            _ => {}
        }
    }

    fn on_wheel(&mut self, x: f32, y: f32, delta: f32, horizontal: bool, mods: Mods) {
        self.tooltip = None;
        if self.geo.preview.contains(x, y) {
            self.scroll_preview(delta);
            return;
        }
        if self.geo.nav.contains(x, y) {
            self.nav.scroll = (self.nav.scroll - delta * 3.0 * nav_row_h(self.cfg.compact)).max(0.0);
            return;
        }
        if !self.geo.files.contains(x, y) {
            return;
        }
        if mods.ctrl {
            // Ctrl+wheel cycles icon sizes, as in Explorer.
            const ORDER: [ViewMode; 6] = [
                ViewMode::Details,
                ViewMode::List,
                ViewMode::Small,
                ViewMode::Medium,
                ViewMode::Large,
                ViewMode::ExtraLarge,
            ];
            let cur = self.tab().view.mode;
            let i = ORDER.iter().position(|&m| m == cur).unwrap_or(0) as i32;
            let j = (i + if delta > 0.0 { 1 } else { -1 }).clamp(0, ORDER.len() as i32 - 1);
            self.set_view_mode(ORDER[j as usize]);
            return;
        }
        let g = self.grid();
        let line = if g.horizontal { g.cell_w } else if g.mode == ViewMode::Details || g.cell_h < 40.0 { g.cell_h * 3.0 } else { g.cell_h };
        let d = if horizontal { -delta } else { delta };
        let t = self.tab_mut();
        t.scroll = (t.scroll - d * line).clamp(0.0, g.max_scroll());
        self.request_thumbnails();
    }

    fn click(&mut self, h: Hit, x: f32, y: f32, mods: Mods) {
        match h {
            Hit::TabClose(i) => self.close_tab(i),
            Hit::NewTab => self.new_tab(Location::ThisPc),
            Hit::Back => self.go_back(),
            Hit::Forward => self.go_forward(),
            Hit::Up => self.go_up(),
            Hit::Refresh => {
                self.nav.refresh_drives();
                self.refresh();
            }
            Hit::Crumb(i) => {
                if let Some(loc) = self.geo.crumbs.get(i).cloned() {
                    if mods.ctrl {
                        self.new_tab(loc);
                    } else {
                        self.navigate(loc);
                    }
                }
            }
            Hit::CrumbChevron(i) => {
                let r = self.geo.crumb_rects.get(i).copied().unwrap_or_default();
                if let Some(loc) = self.geo.crumbs.get(i).cloned() {
                    self.crumb_menu(&loc, r.r, r.b + 4.0);
                }
            }
            Hit::CrumbOverflow => {
                let hidden = self.geo.crumb_hidden.clone();
                let paths: Vec<PathBuf> = hidden.iter().filter_map(|l| l.folder().map(Path::to_path_buf)).collect();
                let mut items: Vec<_> = hidden
                    .iter()
                    .enumerate()
                    .map(|(i, l)| item(M_CRUMB + i as u32, l.name(), false, true))
                    .collect();
                items.reverse();
                let has_pc = hidden.first() == Some(&Location::ThisPc);
                let paths = if has_pc {
                    // Index 0 is This PC (no path); keep indices aligned.
                    std::iter::once(PathBuf::from("::thispc")).chain(paths).collect()
                } else {
                    paths
                };
                self.effects.push(Effect::Menu { kind: MenuKind::Crumb(paths), x, y: y + 12.0, items });
            }
            Hit::SearchClear => {
                self.search.set("");
                if let Location::Search { root, .. } = self.tab().loc.clone() {
                    if root.as_os_str().is_empty() {
                        self.navigate(Location::ThisPc);
                    } else {
                        self.navigate(Location::Folder(root));
                    }
                }
            }
            Hit::Command(c) => self.command(c, x, y),
            Hit::Nav(i) => {
                let loc = self.nav.rows[i].loc.clone();
                if mods.ctrl {
                    self.new_tab(loc);
                } else {
                    self.navigate(loc);
                }
            }
            Hit::NavExpand(i) => self.nav.toggle(i),
            Hit::Header(i) => {
                let col = self.tab().view.cols[i].0;
                self.set_sort(col, None);
            }
            Hit::StatusDetails => self.set_view_mode(ViewMode::Details),
            Hit::StatusTiles => self.set_view_mode(ViewMode::Large),
            _ => {}
        }
    }

    fn crumb_menu(&mut self, loc: &Location, x: f32, y: f32) {
        // Subfolders of this crumb (drives for This PC).
        let dirs: Vec<(PathBuf, String)> = match loc {
            Location::ThisPc => crate::fs::list_this_pc().into_iter().map(|e| (e.path, e.name)).collect(),
            Location::Folder(p) => {
                let cancel = std::sync::atomic::AtomicBool::new(false);
                let mut v: Vec<_> = crate::fs::list_dir(p, &cancel)
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|e| e.is_dir && (!e.hidden() || self.cfg.show_hidden))
                    .map(|e| (e.path, e.name))
                    .collect();
                v.sort_by(|a, b| crate::fs::cmp_names(&a.1, &b.1));
                v.truncate(300);
                v
            }
            _ => Vec::new(),
        };
        if dirs.is_empty() {
            return;
        }
        let cur = self.tab().loc.folder().map(Path::to_path_buf);
        let items = dirs
            .iter()
            .enumerate()
            .map(|(i, (p, n))| item(M_CRUMB + i as u32, n.clone(), cur.as_ref().is_some_and(|c| c.starts_with(p)), true))
            .collect();
        let paths = dirs.into_iter().map(|(p, _)| p).collect();
        self.effects.push(Effect::Menu { kind: MenuKind::Crumb(paths), x, y, items });
    }

    fn command(&mut self, c: Cmd, x: f32, y: f32) {
        let anchor = self.geo.cmds.iter().find(|(k, _)| *k == c).map(|(_, r)| *r);
        let (mx, my) = anchor.map(|r| (r.l, r.b + 2.0)).unwrap_or((x, y));
        match c {
            Cmd::New => {
                let items = vec![item(M_NEW_FOLDER, "Folder", false, true), item(M_NEW_TEXT, "Text Document", false, true)];
                self.effects.push(Effect::Menu { kind: MenuKind::New, x: mx, y: my, items });
            }
            Cmd::Cut => self.copy_selection(true),
            Cmd::Copy => self.copy_selection(false),
            Cmd::Paste => self.paste(),
            Cmd::Rename => self.begin_rename(),
            Cmd::Delete => self.delete_selection(false),
            Cmd::Sort => {
                let items = self.sort_items(0);
                self.effects.push(Effect::Menu { kind: MenuKind::Sort, x: mx, y: my, items });
            }
            Cmd::View => {
                let items = self.view_items(0);
                self.effects.push(Effect::Menu { kind: MenuKind::View, x: mx, y: my, items });
            }
            Cmd::More => {
                let t = self.tab();
                let has_sel = t.selection_count() > 0;
                let folder = t.loc.folder().is_some();
                let th = self.cfg.theme;
                let items = vec![
                    item(
                        M_PIN,
                        if t.loc.folder().is_some_and(|p| self.nav.is_pinned(p)) {
                            "Unpin from Quick access"
                        } else {
                            "Pin to Quick access"
                        },
                        false,
                        folder,
                    ),
                    item(M_COPY_PATH, "Copy path", false, t.loc.base().is_some()),
                    item(M_TERMINAL, "Open in Terminal", false, folder),
                    item(M_NEW_WINDOW, "New window", false, true),
                    sep(),
                    item(M_SELECT_ALL, "Select all", false, true),
                    item(M_SELECT_NONE, "Select none", false, has_sel),
                    item(M_INVERT, "Invert selection", false, true),
                    sep(),
                    item(M_HIDDEN, "Show hidden items", self.cfg.show_hidden, true),
                    item(M_PROTECTED, "Show protected operating system files", self.cfg.show_protected, true),
                    item(M_EXT, "Show file name extensions", self.cfg.show_extensions, true),
                    item(M_COMPACT, "Compact view", self.cfg.compact, true),
                    item(M_THUMB_STORE, "Store thumbnails inside folders (thumbs26.db)", self.cfg.thumbs == crate::config::ThumbLocation::InFolder, true),
                    sep(),
                    item(M_THEME, "Theme: Windows default", th == crate::theme::PREF_SYSTEM, true),
                    item(M_THEME + 1, "Theme: Light", th == crate::theme::PREF_LIGHT, true),
                    item(M_THEME + 2, "Theme: Dark", th == crate::theme::PREF_DARK, true),
                    item(M_MICA, "Mica backdrop", self.cfg.mica, crate::theme::mica_supported()),
                    sep(),
                    item(M_PROPERTIES, "Properties", false, true),
                ];
                let mx = anchor.map(|r| r.l).unwrap_or(x);
                self.effects.push(Effect::Menu { kind: MenuKind::More, x: mx, y: my, items });
            }
            Cmd::Properties => self.properties(),
            Cmd::Preview => self.toggle_preview(),
            Cmd::SearchOptions => {
                let items = self.search_option_items();
                self.effects.push(Effect::Menu { kind: MenuKind::SearchOptions, x: mx, y: my, items });
            }
            Cmd::CloseSearch => self.close_search(),
        }
    }

    fn search_opts(&self) -> crate::fs::SearchOpts {
        match &self.tab().loc {
            Location::Search { opts, .. } => *opts,
            _ => crate::fs::SearchOpts { system: self.cfg.search_system_files, ..Default::default() },
        }
    }

    fn search_option_items(&self) -> Vec<(u32, String, bool, bool)> {
        use crate::fs::{DateFilter, KindFilter, SizeFilter};
        let o = self.search_opts();
        let has_folder = matches!(&self.tab().loc, Location::Search { root, .. } if !root.as_os_str().is_empty());
        let mut v = vec![
            item(M_SUB_ALL, "All subfolders", o.subfolders, true),
            item(M_SUB_HERE, "Current folder", !o.subfolders, has_folder),
            sep(),
            item(0, ">Date modified", false, true),
        ];
        for (i, d) in DateFilter::ALL.iter().enumerate() {
            v.push(item(M_DATE + i as u32, d.label(), o.date == Some(*d), true));
        }
        v.push(item(0, "<", false, true));
        v.push(item(0, ">Type", false, true));
        for (i, k) in KindFilter::ALL.iter().enumerate() {
            v.push(item(M_KIND + i as u32, k.label(), o.kind == Some(*k), true));
        }
        v.push(item(0, "<", false, true));
        v.push(item(0, ">Size", false, true));
        for (i, s) in SizeFilter::ALL.iter().enumerate() {
            v.push(item(M_SIZE + i as u32, s.label(), o.size == Some(*s), true));
        }
        v.push(item(0, "<", false, true));
        v.push(sep());
        v.push(item(M_SYSTEM, "System files", o.system, true));
        v.push(sep());
        v.push(item(M_INDEX, "Instant search index (all drives)", self.cfg.index_drives, true));
        v
    }

    /// Applies a Search options choice by re-running the search with new options.
    fn search_option_chosen(&mut self, id: u32) {
        use crate::fs::{DateFilter, KindFilter, SizeFilter};
        let Location::Search { root, query, mut opts } = self.tab().loc.clone() else {
            return;
        };
        let toggle = |cur: Option<usize>, i: usize| if cur == Some(i) { None } else { Some(i) };
        match id {
            M_SUB_ALL => opts.subfolders = true,
            M_SUB_HERE => opts.subfolders = false,
            i if (M_DATE..M_DATE + 8).contains(&i) => {
                let cur = opts.date.and_then(|d| DateFilter::ALL.iter().position(|x| *x == d));
                opts.date = toggle(cur, (i - M_DATE) as usize).map(|k| DateFilter::ALL[k]);
            }
            i if (M_KIND..M_KIND + 7).contains(&i) => {
                let cur = opts.kind.and_then(|d| KindFilter::ALL.iter().position(|x| *x == d));
                opts.kind = toggle(cur, (i - M_KIND) as usize).map(|k| KindFilter::ALL[k]);
            }
            i if (M_SIZE..M_SIZE + 7).contains(&i) => {
                let cur = opts.size.and_then(|d| SizeFilter::ALL.iter().position(|x| *x == d));
                opts.size = toggle(cur, (i - M_SIZE) as usize).map(|k| SizeFilter::ALL[k]);
            }
            M_SYSTEM => {
                opts.system = !opts.system;
                self.cfg.search_system_files = opts.system;
                self.effects.push(Effect::SaveConfig);
            }
            M_INDEX => {
                self.cfg.index_drives = !self.cfg.index_drives;
                self.effects.push(Effect::SaveConfig);
                if self.cfg.index_drives {
                    crate::index::start();
                }
                return;
            }
            _ => return,
        }
        self.navigate(Location::Search { root, query, opts });
    }

    pub fn close_search(&mut self) {
        self.search.set("");
        if let Location::Search { root, .. } = self.tab().loc.clone() {
            if root.as_os_str().is_empty() {
                self.navigate(Location::ThisPc);
            } else {
                self.navigate(Location::Folder(root));
            }
        }
    }

    pub fn view_items(&self, base: u32) -> Vec<(u32, String, bool, bool)> {
        let cur = self.tab().view.mode;
        let mut v: Vec<_> = ViewMode::ALL
            .iter()
            .enumerate()
            .map(|(i, m)| item(base + M_VIEW + i as u32, m.label(), *m == cur, true))
            .collect();
        v.push(sep());
        v.push(item(base + M_COMPACT, "Compact view", self.cfg.compact, true));
        v.push(item(base + M_HIDDEN, "Hidden items", self.cfg.show_hidden, true));
        v.push(item(base + M_EXT, "File name extensions", self.cfg.show_extensions, true));
        v.push(sep());
        v.push(item(base + M_PREVIEW, "Preview pane	Alt+P", self.cfg.show_preview, true));
        v
    }

    pub fn sort_items(&self, base: u32) -> Vec<(u32, String, bool, bool)> {
        let view = &self.tab().view;
        let mut v: Vec<_> = view
            .cols
            .iter()
            .enumerate()
            .map(|(i, (c, _))| item(base + M_SORT + i as u32, c.title(), view.sort == *c, true))
            .collect();
        v.push(sep());
        v.push(item(base + M_ASC, "Ascending", view.asc, true));
        v.push(item(base + M_DESC, "Descending", !view.asc, true));
        v
    }

    /// A menu choice (ids from our own menus; background/item menus pass ids minus OWN_BASE).
    pub fn menu_chosen(&mut self, kind: Option<MenuKind>, id: u32) {
        if let Some(MenuKind::Crumb(paths)) = &kind {
            if let Some(p) = paths.get((id - M_CRUMB) as usize) {
                let loc = if p.as_os_str() == "::thispc" { Location::ThisPc } else { Location::Folder(p.clone()) };
                self.navigate(loc);
            }
            return;
        }
        if let Some(MenuKind::SearchOptions) = kind {
            self.search_option_chosen(id);
            self.invalidate();
            return;
        }
        if let Some(MenuKind::TabMenu(i)) = kind {
            match id {
                M_TAB_CLOSE => self.close_tab(i),
                M_TAB_OTHERS => {
                    let keep = self.tabs.remove(i);
                    for t in self.tabs.drain(..) {
                        t.close();
                    }
                    self.tabs.push(keep);
                    self.active = 0;
                    self.release_thumbnails();
                    self.sync_title();
                }
                M_TAB_DUP => {
                    let loc = self.tabs[i].loc.clone();
                    self.active = i;
                    self.new_tab(loc);
                }
                _ => {}
            }
            self.invalidate();
            return;
        }
        match id {
            i if (M_VIEW..M_VIEW + 7).contains(&i) => self.set_view_mode(ViewMode::ALL[(i - M_VIEW) as usize]),
            M_COMPACT => {
                self.cfg.compact = !self.cfg.compact;
                self.effects.push(Effect::SaveConfig);
                self.clamp_scroll();
            }
            M_HIDDEN => {
                self.cfg.show_hidden = !self.cfg.show_hidden;
                self.effects.push(Effect::SaveConfig);
                self.reapply_filters();
            }
            M_PROTECTED => {
                self.cfg.show_protected = !self.cfg.show_protected;
                self.effects.push(Effect::SaveConfig);
                self.reapply_filters();
            }
            M_PREVIEW => self.toggle_preview(),
            M_EXT => {
                self.cfg.show_extensions = !self.cfg.show_extensions;
                self.effects.push(Effect::SaveConfig);
            }
            M_THUMB_STORE => {
                use crate::config::ThumbLocation;
                self.cfg.thumbs = if self.cfg.thumbs == ThumbLocation::InFolder { ThumbLocation::Central } else { ThumbLocation::InFolder };
                self.effects.push(Effect::SaveConfig);
            }
            i if (M_THEME..M_THEME + 3).contains(&i) => {
                self.cfg.theme = match i - M_THEME {
                    1 => crate::theme::PREF_LIGHT,
                    2 => crate::theme::PREF_DARK,
                    _ => crate::theme::PREF_SYSTEM,
                };
                self.effects.push(Effect::SaveConfig);
                self.effects.push(Effect::ApplyTheme);
            }
            M_MICA => {
                self.cfg.mica = !self.cfg.mica;
                self.effects.push(Effect::SaveConfig);
                self.effects.push(Effect::ApplyTheme);
            }
            i if (M_SORT..M_SORT + 10).contains(&i) => {
                if let Some(&(c, _)) = self.tab().view.cols.get((i - M_SORT) as usize) {
                    let asc = self.tab().view.asc;
                    self.set_sort(c, Some(asc));
                }
            }
            M_ASC | M_DESC => {
                let col = self.tab().view.sort;
                self.set_sort(col, Some(id == M_ASC));
            }
            M_NEW_FOLDER => self.new_folder(),
            M_NEW_TEXT => self.new_text_document(),
            M_SELECT_ALL => self.tab_mut().select_all(),
            M_SELECT_NONE => self.tab_mut().clear_selection(),
            M_INVERT => self.tab_mut().invert_selection(),
            M_PROPERTIES => self.properties(),
            M_TERMINAL => {
                if let Some(p) = self.tab().loc.folder() {
                    self.effects.push(Effect::OpenTerminal(p.to_path_buf()));
                }
            }
            M_PIN => {
                if let Some(p) = self.tab().loc.folder().map(Path::to_path_buf) {
                    let pin = !self.nav.is_pinned(&p);
                    self.effects.push(Effect::Pin { paths: vec![p], pin });
                }
            }
            M_ITEM_PIN => {
                let paths: Vec<PathBuf> =
                    self.tab().selected_paths().into_iter().filter(|p| p.is_dir()).collect();
                if !paths.is_empty() {
                    let pin = !paths.iter().all(|p| self.nav.is_pinned(p));
                    self.effects.push(Effect::Pin { paths, pin });
                }
            }
            M_COPY_PATH => {
                if let Some(p) = self.tab().loc.base() {
                    crate::platform::set_clipboard_text(&p.to_string_lossy());
                }
            }
            M_ITEM_COPY_PATH => {
                let paths: Vec<String> = self.tab().selected_paths().iter().map(|p| format!("\"{}\"", p.display())).collect();
                crate::platform::set_clipboard_text(&paths.join("\r\n"));
            }
            M_NEW_WINDOW => self.effects.push(Effect::NewWindow(self.tab().loc.folder().map(Path::to_path_buf))),
            M_OPEN_TAB => {
                for p in self.tab().selected_paths() {
                    self.new_tab(Location::Folder(p));
                }
            }
            M_OPEN_WINDOW => {
                for p in self.tab().selected_paths() {
                    self.effects.push(Effect::NewWindow(Some(p)));
                }
            }
            M_REFRESH => self.refresh(),
            M_PASTE => self.paste(),
            _ => {}
        }
        self.invalidate();
    }

    // ---- keyboard -----------------------------------------------------------------------------

    fn on_key(&mut self, vk: u32, mods: Mods) {
        self.tooltip = None;
        // Shortcuts that work everywhere.
        match (vk, mods.ctrl, mods.shift, mods.alt) {
            (0x54, true, false, false) => return self.new_tab(Location::ThisPc), // Ctrl+T
            (0x57, true, false, false) => return self.close_tab(self.active),    // Ctrl+W
            (VK_TAB, true, shift, false) => {
                let n = self.tabs.len();
                let i = if shift { (self.active + n - 1) % n } else { (self.active + 1) % n };
                return self.activate_tab(i);
            }
            (VK_LEFT, false, false, true) => return self.go_back(),
            (VK_RIGHT, false, false, true) => return self.go_forward(),
            (VK_UP, false, false, true) => return self.go_up(),
            (0x50, false, false, true) => return self.toggle_preview(), // Alt+P
            (0x44, false, false, true) | (0x4C, true, false, false) | (VK_F4, false, false, false) => {
                // Alt+D, Ctrl+L, F4: edit the address.
                let mut f = TextField::new(&self.address_text());
                f.select_all();
                self.address = Some(f);
                self.focus = Focus::Address;
                return self.start_caret();
            }
            (0x46, true, false, false) | (0x45, true, false, false) | (VK_F3, false, false, false) => {
                // Ctrl+F, Ctrl+E, F3: search.
                self.end_editing();
                self.focus = Focus::Search;
                self.search.select_all();
                return self.start_caret();
            }
            (0x4E, true, false, false) => {
                return self.effects.push(Effect::NewWindow(self.tab().loc.folder().map(Path::to_path_buf)));
            }
            (0x4E, true, true, false) => return self.new_folder(), // Ctrl+Shift+N
            (VK_F5, false, false, false) | (0x52, true, false, false) => {
                if self.focus != Focus::Rename {
                    self.nav.refresh_drives();
                    return self.refresh();
                }
            }
            _ => {}
        }
        match self.focus {
            Focus::Address => return self.address_key(vk, mods),
            Focus::Search => return self.search_key(vk, mods),
            Focus::Rename => return self.rename_key(vk, mods),
            _ => {}
        }
        // Ctrl+Shift+1..7: views (Explorer order: extra large ... tiles).
        if mods.ctrl && mods.shift && (0x31..=0x37).contains(&vk) {
            return self.set_view_mode(ViewMode::ALL[(vk - 0x31) as usize]);
        }
        let g = self.grid();
        let n = self.tab().order.len();
        match vk {
            VK_RETURN if mods.alt => self.properties(),
            VK_RETURN => self.open_selection(mods),
            VK_BACK => self.go_back(),
            VK_DELETE => self.delete_selection(mods.shift),
            VK_F2 => self.begin_rename(),
            VK_APPS => self.selection_menu(mods.shift),
            VK_ESCAPE => {
                self.cut.clear();
            }
            0x41 if mods.ctrl => self.tab_mut().select_all(),
            0x43 if mods.ctrl => self.copy_selection(false),
            0x58 if mods.ctrl => self.copy_selection(true),
            0x56 if mods.ctrl => self.paste(),
            VK_ADD if mods.ctrl => {
                // Ctrl+Numpad+ : size all Name column to fit.
            }
            VK_SPACE if mods.ctrl => {
                if let Some(f) = self.tab().focus {
                    let t = self.tab_mut();
                    let on = !t.is_selected(f);
                    t.set_selected(f, on);
                }
            }
            VK_PRIOR..=VK_DOWN if n > 0 => {
                if vk == VK_LEFT && g.mode == ViewMode::Details {
                    return;
                }
                let cur = self.tab().focus;
                let target = match cur {
                    Some(c) => g.step(c, vk, g.page_items()),
                    None => {
                        if vk == 0x23 {
                            n - 1
                        } else {
                            0
                        }
                    }
                };
                let t = self.tab_mut();
                if mods.shift {
                    let a = t.anchor.unwrap_or(cur.unwrap_or(0));
                    t.anchor = Some(a);
                    t.select_range(a, target, mods.ctrl);
                    t.focus = Some(target);
                } else if mods.ctrl {
                    t.focus = Some(target);
                } else {
                    t.select_only(target);
                }
                self.ensure_visible(target);
            }
            _ => {}
        }
        let _ = VK_HOME;
    }

    fn on_char(&mut self, c: char) {
        match self.focus {
            Focus::Address => {
                if let Some(f) = self.address.as_mut() {
                    f.char(c);
                }
            }
            Focus::Search => {
                if self.search.char(c) == FieldAction::Changed {
                    self.schedule_search();
                }
            }
            Focus::Rename => {
                if let Some((_, f)) = self.rename.as_mut() {
                    if r#"\/:*?"<>|"#.contains(c) {
                        self.effects.push(Effect::Beep);
                        self.tooltip = Some((
                            "A file name can't contain any of the following characters: \\ / : * ? \" < > |".into(),
                            self.geo.rename_text.l,
                            self.geo.rename_text.b + 6.0,
                        ));
                    } else {
                        f.char(c);
                    }
                }
            }
            Focus::Items | Focus::Nav => {
                if c.is_control() {
                    return;
                }
                // Type-ahead: jump to the first item starting with what was typed recently.
                let (buf, last) = &mut self.typeahead;
                if last.elapsed().as_millis() > 1000 {
                    buf.clear();
                }
                *last = Instant::now();
                buf.extend(c.to_lowercase());
                let prefix = buf.clone();
                let show_ext = self.cfg.show_extensions;
                let t = self.tab();
                let start = if prefix.chars().count() == 1 { t.focus.map(|f| f + 1).unwrap_or(0) } else { t.focus.unwrap_or(0) };
                let n = t.order.len();
                let found = (0..n).map(|k| (start + k) % n.max(1)).find(|&p| {
                    t.entry_at(p)
                        .map(|e| e.display_name(show_ext).to_lowercase().starts_with(&prefix))
                        .unwrap_or(false)
                });
                if let Some(p) = found {
                    self.tab_mut().select_only(p);
                    self.ensure_visible(p);
                }
            }
        }
    }

    fn address_text(&self) -> String {
        match &self.tab().loc {
            Location::ThisPc => "This PC".into(),
            Location::Folder(p) => p.to_string_lossy().into_owned(),
            Location::Search { root, .. } => root.to_string_lossy().into_owned(),
        }
    }

    fn address_key(&mut self, vk: u32, mods: Mods) {
        let Some(f) = self.address.as_mut() else {
            self.focus = Focus::Items;
            return;
        };
        match f.key(vk, mods) {
            FieldAction::Enter => {
                let text = f.text.trim().trim_matches('"').to_string();
                self.address = None;
                self.focus = Focus::Items;
                self.go_to_text(&text);
            }
            FieldAction::Escape | FieldAction::Tab => {
                self.address = None;
                self.focus = Focus::Items;
            }
            _ => {}
        }
        self.caret_on = true;
    }

    /// Address bar input: folders navigate, files open, "This PC" and %VARS% work.
    fn go_to_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        if text.eq_ignore_ascii_case("this pc") || text.eq_ignore_ascii_case("computer") {
            return self.navigate(Location::ThisPc);
        }
        let expanded = expand_env(text);
        let mut p = PathBuf::from(&expanded);
        if p.is_relative() {
            if let Some(base) = self.tab().loc.base() {
                p = base.join(&p);
            }
        }
        if expanded.len() == 2 && expanded.ends_with(':') {
            p = PathBuf::from(format!("{expanded}\\"));
        }
        if p.is_dir() {
            let p = std::path::absolute(&p).unwrap_or(p);
            self.navigate(Location::Folder(p));
        } else if p.exists() {
            self.effects.push(Effect::Open(p));
        } else {
            // URLs, shell: folders, apps on PATH... let the shell try.
            self.effects.push(Effect::Open(PathBuf::from(expanded)));
        }
    }

    fn schedule_search(&mut self) {
        self.effects.push(Effect::Timer { id: TIMER_SEARCH, ms: 150 });
    }

    pub fn run_search(&mut self) {
        let q = self.search.text.trim().to_string();
        let root = match &self.tab().loc {
            Location::Folder(p) => Some(p.clone()),
            Location::Search { root, .. } => Some(root.clone()),
            // From This PC, search every indexed drive.
            Location::ThisPc => Some(PathBuf::new()),
        };
        let Some(root) = root else {
            return;
        };
        if q.is_empty() {
            if matches!(self.tab().loc, Location::Search { .. }) {
                if root.as_os_str().is_empty() {
                    self.navigate(Location::ThisPc);
                } else {
                    self.navigate(Location::Folder(root));
                }
            }
        } else {
            let opts = self.search_opts();
            self.navigate(Location::Search { root, query: q, opts });
        }
    }

    fn search_key(&mut self, vk: u32, mods: Mods) {
        match self.search.key(vk, mods) {
            FieldAction::Enter => {
                self.effects.push(Effect::KillTimer(TIMER_SEARCH));
                self.run_search();
                self.focus = Focus::Items;
            }
            FieldAction::Escape => {
                self.search.set("");
                self.focus = Focus::Items;
                self.run_search();
            }
            FieldAction::Tab => self.focus = Focus::Items,
            FieldAction::Changed => self.schedule_search(),
            _ => {}
        }
        self.caret_on = true;
    }

    fn rename_key(&mut self, vk: u32, mods: Mods) {
        let Some((_, f)) = self.rename.as_mut() else {
            self.focus = Focus::Items;
            return;
        };
        match f.key(vk, mods) {
            FieldAction::Enter | FieldAction::Tab => self.commit_rename(),
            FieldAction::Escape => {
                self.rename = None;
                self.focus = Focus::Items;
            }
            _ => {}
        }
        self.tooltip = None;
        self.caret_on = true;
    }

    fn start_caret(&mut self) {
        self.caret_on = true;
        self.effects.push(Effect::Timer { id: TIMER_CARET, ms: 530 });
    }

    pub fn on_timer(&mut self, id: usize) {
        match id {
            TIMER_CARET => {
                if matches!(self.focus, Focus::Address | Focus::Search | Focus::Rename) {
                    self.caret_on = !self.caret_on;
                } else {
                    self.effects.push(Effect::KillTimer(TIMER_CARET));
                }
            }
            TIMER_REFRESH => {
                self.effects.push(Effect::KillTimer(TIMER_REFRESH));
                self.refresh_due_tabs();
            }
            TIMER_LOADING => {
                if !self.tabs.iter().any(|t| t.loading) {
                    self.effects.push(Effect::KillTimer(TIMER_LOADING));
                }
            }
            super::TIMER_PREVIEW => self.load_preview(),
            TIMER_SEARCH => {
                self.effects.push(Effect::KillTimer(TIMER_SEARCH));
                self.run_search();
            }
            _ => {}
        }
        self.invalidate();
    }

    // ---- actions ------------------------------------------------------------------------------

    pub fn open_selection(&mut self, mods: Mods) {
        let t = self.tab();
        let entries: Vec<(PathBuf, bool, String)> = t
            .order
            .iter()
            .filter(|&&i| t.sel[i as usize])
            .map(|&i| {
                let e = &t.entries[i as usize];
                (e.path.clone(), e.is_dir, e.ext.clone())
            })
            .collect();
        let mut navigated = false;
        for (path, is_dir, ext) in entries {
            let target = if is_dir {
                Some(path.clone())
            } else if ext == ".lnk" {
                shell::resolve_link(&path).filter(|p| p.is_dir())
            } else {
                None
            };
            match target {
                Some(dir) if !navigated && !mods.ctrl => {
                    navigated = true;
                    self.navigate(Location::Folder(dir));
                }
                Some(dir) => self.new_tab(Location::Folder(dir)),
                None => self.effects.push(Effect::Open(path)),
            }
        }
    }

    fn selection_menu(&mut self, extended: bool) {
        let paths = self.tab().selected_paths();
        let pos = self.tab().focus;
        let (x, y) = match pos {
            Some(p) => {
                let r = self.grid().screen(p, self.tab().scroll);
                (r.l + 24.0, r.cy())
            }
            None => (self.geo.items.l + 24.0, self.geo.items.t + 24.0),
        };
        if paths.is_empty() {
            let folder = self.tab().loc.folder().map(Path::to_path_buf);
            self.effects.push(Effect::BackgroundMenu { folder, x, y });
        } else {
            self.effects.push(Effect::ItemMenu { paths, x, y, extended });
        }
    }

    pub fn begin_rename(&mut self) {
        let t = self.tab();
        if t.loc.base().is_none() {
            return;
        }
        let sel = t.selected();
        let pos = match (t.focus, sel.as_slice()) {
            (_, [one]) => *one,
            (Some(f), _) if t.is_selected(f) => f,
            _ => return,
        };
        let Some(e) = t.entry_at(pos) else {
            return;
        };
        let name = e.display_name(self.cfg.show_extensions).to_string();
        let mut f = TextField::new(&name);
        // Select the name without its extension, like Explorer.
        let stem = if !e.is_dir && self.cfg.show_extensions {
            name.rfind('.').filter(|&i| i > 0).unwrap_or(name.len())
        } else {
            name.len()
        };
        f.select(0, stem);
        self.rename = Some((pos, f));
        self.focus = Focus::Rename;
        self.ensure_visible(pos);
        self.start_caret();
    }

    pub fn commit_rename(&mut self) {
        let Some((pos, f)) = self.rename.take() else {
            return;
        };
        self.focus = Focus::Items;
        let Some(e) = self.tab().entry_at(pos).cloned() else {
            return;
        };
        let mut new = f.text.trim().trim_end_matches('.').to_string();
        if !self.cfg.show_extensions && !e.is_dir && !e.ext.is_empty() {
            new.push_str(&e.name[e.name.len() - e.ext.len()..]);
        }
        if new.is_empty() || new == e.name || new.contains(|c: char| r#"\/:*?"<>|"#.contains(c)) {
            return;
        }
        self.tab_mut().pending_select = Some((new.clone(), false));
        self.effects.push(Effect::FileOp(Op::Rename(e.path.clone(), new)));
    }

    pub fn delete_selection(&mut self, permanent: bool) {
        if self.tab().loc.base().is_none() {
            return;
        }
        let paths = self.tab().selected_paths();
        if !paths.is_empty() {
            self.effects.push(Effect::FileOp(Op::Delete(paths, permanent)));
        }
    }

    pub fn copy_selection(&mut self, cut: bool) {
        let paths = self.tab().selected_paths();
        if paths.is_empty() || (cut && self.tab().loc.base().is_none()) {
            return;
        }
        self.cut = if cut { paths.iter().cloned().collect() } else { Default::default() };
        self.effects.push(Effect::SetClipboard { paths, cut });
    }

    pub fn paste(&mut self) {
        if let Some(dir) = self.tab().loc.base().map(Path::to_path_buf) {
            self.effects.push(Effect::Paste(dir));
        }
    }

    /// The host performed a paste of cut files: they're no longer "cut".
    pub fn pasted(&mut self) {
        self.cut.clear();
    }

    pub fn properties(&mut self) {
        let mut paths = self.tab().selected_paths();
        if paths.is_empty() {
            if let Some(p) = self.tab().loc.base() {
                paths.push(p.to_path_buf());
            }
        }
        if !paths.is_empty() {
            self.effects.push(Effect::Properties(paths));
        }
    }

    pub fn new_folder(&mut self) {
        let Some(dir) = self.tab().loc.folder().map(Path::to_path_buf) else {
            return;
        };
        match shell::new_folder(&dir) {
            Ok(name) => {
                self.tab_mut().pending_select = Some((name, true));
                self.refresh();
            }
            Err(_) => self.effects.push(Effect::Beep),
        }
    }

    fn new_text_document(&mut self) {
        let Some(dir) = self.tab().loc.folder().map(Path::to_path_buf) else {
            return;
        };
        let mut name = "New Text Document.txt".to_string();
        let mut n = 2;
        while dir.join(&name).exists() {
            name = format!("New Text Document ({n}).txt");
            n += 1;
        }
        if std::fs::write(dir.join(&name), b"").is_ok() {
            self.tab_mut().pending_select = Some((name, true));
            self.refresh();
        } else {
            self.effects.push(Effect::Beep);
        }
    }

    fn autofit_column(&mut self, i: usize, m: &dyn Measure) {
        let t = self.tab();
        let Some(&(col, _)) = t.view.cols.get(i) else {
            return;
        };
        let show_ext = self.cfg.show_extensions;
        let mut w: f32 = m.text_width(col.title(), Font::Ui) + 30.0;
        for &idx in t.order.iter().take(5000) {
            let e = &t.entries[idx as usize];
            let text = if col == Column::Name {
                e.display_name(show_ext).to_string()
            } else {
                super::paint::column_text(e, col)
            };
            let extra = if col == Column::Name { 48.0 } else { 24.0 };
            w = w.max(m.text_width(&text, Font::Ui) + extra);
        }
        self.tab_mut().view.cols[i].1 = w.min(1200.0);
        self.tab_mut().save_view();
    }

    /// The window's drop target asks which folder (or file) is under a client point.
    pub fn drop_target_at(&self, x: f32, y: f32) -> Option<PathBuf> {
        match self.hit_test(x, y) {
            Hit::Item(pos) => {
                let e = self.tab().entry_at(pos)?;
                // Folders, drives, executables and archives accept drops; other files mean "here".
                if e.is_dir || matches!(e.ext.as_str(), ".exe" | ".zip" | ".lnk" | ".bat" | ".cmd") {
                    Some(e.path.clone())
                } else {
                    self.tab().loc.base().map(Path::to_path_buf)
                }
            }
            Hit::Items | Hit::Header(_) | Hit::HeaderArea | Hit::ScrollTrack | Hit::ScrollThumb => {
                self.tab().loc.base().map(Path::to_path_buf)
            }
            Hit::Nav(i) => self.nav.rows.get(i)?.loc.folder().map(Path::to_path_buf),
            Hit::Crumb(i) => self.geo.crumbs.get(i)?.folder().map(Path::to_path_buf),
            Hit::Tab(i) => self.tabs.get(i)?.loc.folder().map(Path::to_path_buf),
            _ => None,
        }
    }

    pub fn loading_tick(&self) -> bool {
        self.tabs.iter().any(|t| t.loading)
    }

    pub fn tab_icon_refresh(&mut self) {
        let _ = TIMER_LOADING;
    }
}

/// Expands %VARIABLES% in typed paths.
fn expand_env(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find('%') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        match after.find('%') {
            Some(j) => {
                let name = &after[..j];
                match std::env::var(name) {
                    Ok(v) if !name.is_empty() => out.push_str(&v),
                    _ => {
                        out.push('%');
                        out.push_str(name);
                        out.push('%');
                    }
                }
                rest = &after[j + 1..];
            }
            None => {
                out.push('%');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}
