//! A tab: its location and history, the listing, selection, view settings, and the background work
//! that fills it (listing, per-file icons, search, watching, thumbnails).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};

use crate::fs::{self, Column, Entry, Thumb};
use crate::icons;
use crate::msg::{Msg, Notifier};
use crate::store::{self, ThumbDb};
use crate::thumbs;
use crate::watch::{self, Watcher};

use super::layout::Grid;
use super::{App, Effect, Focus, ViewMode, TIMER_LOADING, TIMER_REFRESH};

#[derive(Clone, Debug, PartialEq)]
pub enum Location {
    ThisPc,
    Folder(PathBuf),
    Search {
        root: PathBuf,
        query: String,
        opts: fs::SearchOpts,
    },
}

impl Location {
    pub fn folder(&self) -> Option<&Path> {
        match self {
            Location::Folder(p) => Some(p),
            _ => None,
        }
    }

    /// Folder for new items / paste (the search root for searches).
    pub fn base(&self) -> Option<&Path> {
        match self {
            Location::Folder(p) => Some(p),
            Location::Search { root, .. } if !root.as_os_str().is_empty() => Some(root),
            _ => None,
        }
    }

    pub fn parent(&self) -> Option<Location> {
        match self {
            Location::ThisPc => None,
            Location::Search { root, .. } if root.as_os_str().is_empty() => Some(Location::ThisPc),
            Location::Search { root, .. } => Some(Location::Folder(root.clone())),
            Location::Folder(p) => Some(match p.parent() {
                Some(par) if !par.as_os_str().is_empty() => Location::Folder(par.to_path_buf()),
                _ => Location::ThisPc,
            }),
        }
    }

    pub fn name(&self) -> String {
        match self {
            Location::ThisPc => "This PC".into(),
            Location::Search { root, query, .. } => {
                format!("{} - Search Results in {}", query, folder_name(root))
            }
            Location::Folder(p) => folder_name(p),
        }
    }
}

pub fn folder_name(p: &Path) -> String {
    if p.as_os_str().is_empty() {
        return "This PC".into();
    }
    match p.file_name() {
        Some(n) => n.to_string_lossy().into_owned(),
        None => {
            let s = p.to_string_lossy().into_owned();
            if s.len() <= 3 && s.ends_with('\\') {
                icons::drive_info(&s).0
            } else {
                s
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct FolderView {
    pub mode: ViewMode,
    pub sort: Column,
    pub asc: bool,
    pub cols: Vec<(Column, f32)>,
}

impl FolderView {
    pub fn for_location(loc: &Location) -> FolderView {
        let cols: Vec<Column> = match loc {
            Location::ThisPc => vec![Column::Name, Column::Type, Column::Total, Column::Free],
            Location::Search { .. } => vec![
                Column::Name,
                Column::Modified,
                Column::Type,
                Column::Size,
                Column::Folder,
            ],
            Location::Folder(_) => vec![Column::Name, Column::Modified, Column::Type, Column::Size],
        };
        FolderView {
            mode: if matches!(loc, Location::ThisPc) {
                ViewMode::Tiles
            } else {
                ViewMode::Details
            },
            sort: Column::Name,
            asc: true,
            cols: cols.into_iter().map(|c| (c, c.default_width())).collect(),
        }
    }

    pub fn to_kv(&self) -> String {
        let cols: Vec<String> = self
            .cols
            .iter()
            .map(|(c, w)| format!("{}:{}", c.id(), w.round()))
            .collect();
        format!(
            "view={}\nsort={}\nascending={}\ncolumns={}\n",
            self.mode.id(),
            self.sort.id(),
            self.asc,
            cols.join(",")
        )
    }

    pub fn apply_kv(&mut self, s: &str) {
        let m = store::parse_kv(s);
        if let Some(v) = m.get("view").and_then(|v| ViewMode::from_id(v)) {
            self.mode = v;
        }
        if let Some(c) = m.get("sort").and_then(|v| Column::from_id(v)) {
            if self.cols.iter().any(|(k, _)| *k == c) {
                self.sort = c;
            }
        }
        if let Some(a) = m.get("ascending") {
            self.asc = a != "false";
        }
        if let Some(cols) = m.get("columns") {
            for part in cols.split(',') {
                if let Some((id, w)) = part.split_once(':') {
                    if let (Some(c), Ok(w)) = (Column::from_id(id), w.parse::<f32>()) {
                        if let Some(slot) = self.cols.iter_mut().find(|(k, _)| *k == c) {
                            slot.1 = w.clamp(40.0, 2000.0);
                        }
                    }
                }
            }
        }
    }

    pub fn details_width(&self) -> f32 {
        self.cols.iter().map(|(_, w)| w).sum::<f32>() + 16.0
    }
}

pub struct Tab {
    pub id: u64,
    pub loc: Location,
    /// Shell icon of the location (tab and address bar), -1 for searches.
    pub icon: i32,
    pub back: Vec<Location>,
    pub fwd: Vec<Location>,
    pub entries: Vec<Entry>,
    /// Visible entries (indices into `entries`) in display order.
    pub order: Vec<u32>,
    pub sel: Vec<bool>,
    /// Focused item and shift-selection anchor (positions in `order`).
    pub focus: Option<usize>,
    pub anchor: Option<usize>,
    pub scroll: f32,
    pub view: FolderView,
    pub gen: u64,
    pub loading: bool,
    pub load_started: Instant,
    pub error: Option<String>,
    pub db: Option<Arc<Mutex<ThumbDb>>>,
    cancel: Arc<AtomicBool>,
    watcher: Option<Watcher>,
    /// Waiting for a debounced refresh after a change on disk.
    pub refresh_due: bool,
    /// Select (and maybe rename) this name when it shows up after a refresh.
    pub pending_select: Option<(String, bool)>,
    /// Scroll position to restore when going back/forward.
    scroll_memory: HashMap<String, f32>,
    /// Folder settings loaded from thumbs26.db (no heuristics then).
    has_saved_view: bool,
}

impl Tab {
    pub fn new(id: u64) -> Tab {
        Tab {
            id,
            loc: Location::ThisPc,
            icon: -1,
            back: Vec::new(),
            fwd: Vec::new(),
            entries: Vec::new(),
            order: Vec::new(),
            sel: Vec::new(),
            focus: None,
            anchor: None,
            scroll: 0.0,
            view: FolderView::for_location(&Location::ThisPc),
            gen: 0,
            loading: false,
            load_started: Instant::now(),
            error: None,
            db: None,
            cancel: Arc::new(AtomicBool::new(false)),
            watcher: None,
            refresh_due: false,
            pending_select: None,
            scroll_memory: HashMap::new(),
            has_saved_view: false,
        }
    }

    pub fn title(&self) -> String {
        self.loc.name()
    }

    pub fn close(self) {
        self.cancel.store(true, Ordering::Relaxed);
        if let Some(db) = self.db {
            maintain_db(db, &self.entries);
        }
    }

    pub fn selected(&self) -> Vec<usize> {
        self.order
            .iter()
            .enumerate()
            .filter(|(_, &i)| self.sel[i as usize])
            .map(|(p, _)| p)
            .collect()
    }

    pub fn selected_paths(&self) -> Vec<PathBuf> {
        self.order
            .iter()
            .filter(|&&i| self.sel[i as usize])
            .map(|&i| self.entries[i as usize].path.clone())
            .collect()
    }

    pub fn selection_count(&self) -> usize {
        self.order.iter().filter(|&&i| self.sel[i as usize]).count()
    }

    pub fn entry_at(&self, pos: usize) -> Option<&Entry> {
        self.order.get(pos).map(|&i| &self.entries[i as usize])
    }

    pub fn is_selected(&self, pos: usize) -> bool {
        self.order
            .get(pos)
            .map(|&i| self.sel[i as usize])
            .unwrap_or(false)
    }

    pub fn set_selected(&mut self, pos: usize, on: bool) {
        if let Some(&i) = self.order.get(pos) {
            self.sel[i as usize] = on;
        }
    }

    pub fn clear_selection(&mut self) {
        self.sel.iter_mut().for_each(|s| *s = false);
    }

    pub fn select_only(&mut self, pos: usize) {
        self.clear_selection();
        self.set_selected(pos, true);
        self.focus = Some(pos);
        self.anchor = Some(pos);
    }

    pub fn select_range(&mut self, a: usize, b: usize, add: bool) {
        if !add {
            self.clear_selection();
        }
        for p in a.min(b)..=a.max(b) {
            self.set_selected(p, true);
        }
    }

    pub fn select_all(&mut self) {
        for &i in &self.order {
            self.sel[i as usize] = true;
        }
    }

    pub fn invert_selection(&mut self) {
        for &i in &self.order {
            self.sel[i as usize] = !self.sel[i as usize];
        }
    }

    pub fn position_of_name(&self, name: &str) -> Option<usize> {
        self.order
            .iter()
            .position(|&i| self.entries[i as usize].name.eq_ignore_ascii_case(name))
    }

    /// Rebuilds the visible order (filtering + sorting), keeping the focused item.
    pub fn rebuild_order(&mut self, show_hidden: bool, show_protected: bool) {
        let focused = self.focus.and_then(|p| self.order.get(p).copied());
        let anchor = self.anchor.and_then(|p| self.order.get(p).copied());
        self.order = (0..self.entries.len() as u32)
            .filter(|&i| {
                let e = &self.entries[i as usize];
                if e.name.eq_ignore_ascii_case(store::FILE_NAME)
                    || e.name.eq_ignore_ascii_case("thumbs26.db.tmp")
                {
                    return false;
                }
                if e.protected() {
                    return show_protected;
                }
                !e.hidden() || show_hidden
            })
            .collect();
        fs::sort(&self.entries, &mut self.order, self.view.sort, self.view.asc);
        self.focus = focused.and_then(|i| self.order.iter().position(|&o| o == i));
        self.anchor = anchor.and_then(|i| self.order.iter().position(|&o| o == i));
    }

    pub fn save_view(&mut self) {
        if !matches!(self.loc, Location::Folder(_) | Location::ThisPc) {
            return;
        }
        self.has_saved_view = true;
        if let Some(db) = &self.db {
            let _ = db.lock().unwrap().set_settings(&self.view.to_kv());
        }
    }

    fn scroll_key(loc: &Location) -> String {
        match loc {
            Location::Folder(p) => crate::platform::folder_key(p),
            Location::ThisPc => "::thispc".into(),
            Location::Search { root, query, opts } => format!("{}?{}?{:?}", root.display(), query, opts),
        }
    }
}

/// Prunes thumbnails of deleted files and compacts the db, off the UI thread.
fn maintain_db(db: Arc<Mutex<ThumbDb>>, entries: &[Entry]) {
    if entries.is_empty() {
        return;
    }
    let names: std::collections::HashSet<String> =
        entries.iter().map(|e| e.name.to_lowercase()).collect();
    std::thread::spawn(move || {
        let mut db = db.lock().unwrap();
        db.prune(&|n| names.contains(n));
        let _ = db.compact_if_needed();
    });
}

fn this_pc_key() -> PathBuf {
    PathBuf::from("::ThisPC")
}

impl App {
    /// Starts showing `loc` in tab `ti`. `refresh` keeps selection/scroll and merges results.
    pub fn load(&mut self, ti: usize, loc: Location, refresh: bool) {
        let notifier = self.notifier.clone();
        let thumbs_loc = self.cfg.thumbs;
        let tab = &mut self.tabs[ti];
        tab.cancel.store(true, Ordering::Relaxed);
        tab.cancel = Arc::new(AtomicBool::new(false));
        tab.gen += 1;
        tab.refresh_due = false;
        if !refresh {
            let key = Tab::scroll_key(&tab.loc);
            tab.scroll_memory.insert(key, tab.scroll);
            if let Some(db) = tab.db.take() {
                maintain_db(db, &tab.entries);
            }
            tab.watcher = None;
            tab.entries.clear();
            tab.order.clear();
            tab.sel.clear();
            tab.focus = None;
            tab.anchor = None;
            tab.scroll = 0.0;
            tab.error = None;
            tab.has_saved_view = false;
            let keep_mode = matches!(loc, Location::Search { .. })
                .then(|| tab.view.mode)
                .filter(|_| tab.loc.base().is_some());
            tab.view = FolderView::for_location(&loc);
            if let Some(m) = keep_mode {
                tab.view.mode = m;
            }
        }
        if !refresh {
            tab.icon = match &loc {
                Location::ThisPc => super::nav::this_pc_icon(),
                Location::Folder(p) if p.to_string_lossy().starts_with(r"\\") => icons::folder_icon(),
                Location::Folder(p) => icons::any_icon(p),
                Location::Search { .. } => -1,
            };
        }
        tab.loc = loc.clone();
        tab.loading = true;
        tab.load_started = Instant::now();
        let (id, gen, cancel) = (tab.id, tab.gen, tab.cancel.clone());
        let open_db = !refresh || tab.db.is_none();
        match loc {
            Location::Search { root, query, opts } => {
                std::thread::spawn(move || {
                    unsafe {
                        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
                    }
                    let filter = opts.compile(&root);
                    let mut emit = |mut entries: Vec<Entry>| {
                        entries.retain(|e| filter.accepts(e));
                        if entries.is_empty() {
                            return;
                        }
                        notifier.send(Msg::Found {
                            tab: id,
                            gen,
                            entries,
                            done: false,
                        })
                    };
                    // The drive index answers instantly; walk folders only if it isn't ready
                    // (or for "current folder" searches, which only need one listing).
                    let indexed = opts.subfolders && crate::index::search(&root, &query, &cancel, &mut emit);
                    if !indexed {
                        fs::search(&root, &query, opts.subfolders, &cancel, &mut emit);
                    }
                    notifier.send(Msg::Found {
                        tab: id,
                        gen,
                        entries: Vec::new(),
                        done: true,
                    });
                });
            }
            other => {
                if !refresh {
                    if let Location::Folder(p) = &other {
                        tab.watcher = watch::watch(p, id, gen, notifier.clone());
                    }
                }
                std::thread::spawn(move || {
                    unsafe {
                        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
                    }
                    let (result, db_path) = match &other {
                        Location::ThisPc => (Ok(fs::list_this_pc()), this_pc_key()),
                        Location::Folder(p) => (fs::list_dir(p, &cancel), p.clone()),
                        _ => unreachable!(),
                    };
                    let db = open_db.then(|| thumbs::open_db(&db_path, thumbs_loc));
                    let (entries, error) = match result {
                        Ok(e) => (e, None),
                        Err(e) => (Vec::new(), Some(e)),
                    };
                    let own: Vec<(u32, PathBuf, bool)> = entries
                        .iter()
                        .enumerate()
                        .filter(|(_, e)| icons::needs_own_icon(&e.ext, e.is_dir, e.attrs))
                        .map(|(i, e)| (i as u32, e.path.clone(), e.is_dir))
                        .collect();
                    if cancel.load(Ordering::Relaxed) {
                        return;
                    }
                    notifier.send(Msg::Listed {
                        tab: id,
                        gen,
                        entries,
                        error,
                        db,
                    });
                    // Per-file icons in small batches so the first ones appear quickly.
                    for chunk in own.chunks(64) {
                        if cancel.load(Ordering::Relaxed) {
                            return;
                        }
                        let icons: Vec<(u32, i32)> = chunk
                            .iter()
                            .map(|(i, p, d)| (*i, icons::path_icon(p, *d)))
                            .filter(|(_, ic)| *ic >= 0)
                            .collect();
                        notifier.send(Msg::Icons {
                            tab: id,
                            gen,
                            icons,
                        });
                    }
                });
            }
        }
        self.effects.push(Effect::Timer {
            id: TIMER_LOADING,
            ms: 250,
        });
        if ti == self.active {
            self.sync_title();
        }
        self.invalidate();
    }

    pub fn navigate(&mut self, loc: Location) {
        let cur = self.tab().loc.clone();
        if cur == loc {
            self.refresh();
            return;
        }
        self.end_editing();
        let t = self.tab_mut();
        // Typing a search refines the current search instead of stacking history entries.
        let replace = matches!((&cur, &loc), (Location::Search { .. }, Location::Search { .. }));
        if !replace {
            t.back.push(cur);
        }
        t.fwd.clear();
        let ti = self.active;
        self.load(ti, loc, false);
        if self.focus == Focus::Address {
            self.focus = Focus::Items;
        }
    }

    pub fn go_back(&mut self) {
        let ti = self.active;
        let t = &mut self.tabs[ti];
        if let Some(loc) = t.back.pop() {
            let cur = t.loc.clone();
            t.fwd.push(cur);
            self.end_editing();
            self.load(ti, loc, false);
            self.restore_scroll(ti);
        }
    }

    pub fn go_forward(&mut self) {
        let ti = self.active;
        let t = &mut self.tabs[ti];
        if let Some(loc) = t.fwd.pop() {
            let cur = t.loc.clone();
            t.back.push(cur);
            self.end_editing();
            self.load(ti, loc, false);
            self.restore_scroll(ti);
        }
    }

    fn restore_scroll(&mut self, ti: usize) {
        let t = &mut self.tabs[ti];
        let key = Tab::scroll_key(&t.loc);
        if let Some(s) = t.scroll_memory.get(&key) {
            t.scroll = *s;
        }
    }

    pub fn go_up(&mut self) {
        let cur = self.tab().loc.clone();
        if let Some(parent) = cur.parent() {
            self.navigate(parent);
            // Select the folder we came from, like Explorer.
            if let Location::Folder(p) = &cur {
                if let Some(n) = p.file_name() {
                    self.tab_mut().pending_select = Some((n.to_string_lossy().into_owned(), false));
                }
            }
        }
    }

    pub fn refresh(&mut self) {
        let ti = self.active;
        let loc = self.tabs[ti].loc.clone();
        self.load(ti, loc, true);
    }

    /// Handles everything workers sent. Returns true if anything changed.
    pub fn drain(&mut self) -> bool {
        self.notifier.begin_drain();
        let mut changed = false;
        let mut thumbs_done = false;
        while let Ok(m) = self.rx.try_recv() {
            changed = true;
            match m {
                Msg::Listed {
                    tab,
                    gen,
                    entries,
                    error,
                    db,
                } => self.on_listed(tab, gen, entries, error, db),
                Msg::Found {
                    tab,
                    gen,
                    entries,
                    done,
                } => self.on_found(tab, gen, entries, done),
                Msg::Icons { tab, gen, icons } => {
                    if let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab && t.gen == gen) {
                        for (i, ic) in icons {
                            if let Some(e) = t.entries.get_mut(i as usize) {
                                e.icon = ic;
                            }
                        }
                    }
                }
                Msg::Thumb {
                    tab,
                    gen,
                    idx,
                    key,
                    px,
                } => {
                    thumbs_done = true;
                    let mut stale = None;
                    if let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab && t.gen == gen) {
                        if let Some(e) = t.entries.get_mut(idx as usize) {
                            if let Thumb::Ready(old, _) = e.thumb {
                                stale = Some(old);
                            }
                            e.thumb = match key {
                                Some(crate::scene::ImageKey::Thumb(id)) => Thumb::Ready(id, px),
                                _ => Thumb::None,
                            };
                        }
                    } else if let Some(crate::scene::ImageKey::Thumb(id)) = key {
                        stale = Some(id);
                    }
                    if let Some(old) = stale {
                        self.images.remove(&crate::scene::ImageKey::Thumb(old));
                    }
                }
                Msg::Changed { tab, gen } => {
                    if let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab && t.gen == gen) {
                        if !t.refresh_due {
                            t.refresh_due = true;
                            self.effects.push(Effect::Timer {
                                id: TIMER_REFRESH,
                                ms: 200,
                            });
                        }
                    }
                }
                Msg::Preview { gen, content } => self.on_preview(gen, content),
                Msg::QuickAccess(items) => {
                    self.nav.set_quick_access(items);
                }
                Msg::OpDone { folders } => {
                    let keys: Vec<String> =
                        folders.iter().map(|f| crate::platform::folder_key(f)).collect();
                    for t in &mut self.tabs {
                        if let Location::Folder(p) = &t.loc {
                            if keys.contains(&crate::platform::folder_key(p)) && !t.refresh_due {
                                t.refresh_due = true;
                                self.effects.push(Effect::Timer {
                                    id: TIMER_REFRESH,
                                    ms: 50,
                                });
                            }
                        }
                    }
                }
            }
        }
        if thumbs_done && self.pool.pending() == 0 {
            // Next batch beyond the visible range.
            self.request_thumbnails();
        }
        if changed {
            self.invalidate();
        }
        changed
    }

    /// Debounced refresh timer: reload tabs whose folders changed.
    pub fn refresh_due_tabs(&mut self) {
        for ti in 0..self.tabs.len() {
            if self.tabs[ti].refresh_due {
                let loc = self.tabs[ti].loc.clone();
                self.load(ti, loc, true);
            }
        }
    }

    fn on_listed(
        &mut self,
        tab: u64,
        gen: u64,
        mut entries: Vec<Entry>,
        error: Option<String>,
        db: Option<ThumbDb>,
    ) {
        let (show_hidden, show_protected) = (self.cfg.show_hidden, self.cfg.show_protected);
        let Some(ti) = self.tabs.iter().position(|t| t.id == tab && t.gen == gen) else {
            return;
        };
        let t = &mut self.tabs[ti];
        t.loading = false;
        t.error = error;
        let refresh = !t.entries.is_empty() || t.db.is_some() && db.is_none();
        if let Some(db) = db {
            let db = Arc::new(Mutex::new(db));
            if !t.has_saved_view {
                let saved = db.lock().unwrap().settings().map(str::to_string);
                if let Some(s) = saved {
                    t.view.apply_kv(&s);
                    t.has_saved_view = true;
                } else if matches!(t.loc, Location::Folder(_)) {
                    // Explorer-like folder templates: mostly pictures/videos -> Large icons.
                    let files: Vec<&Entry> = entries.iter().filter(|e| !e.is_dir).collect();
                    let media = files.iter().filter(|e| e.is_media()).count();
                    if files.len() >= 4 && media * 3 >= files.len() * 2 {
                        t.view.mode = ViewMode::Large;
                    }
                }
            }
            t.db = Some(db);
        }
        if refresh {
            // Keep selection, thumbnails and resolved icons for unchanged items.
            let old: HashMap<String, (bool, Thumb, i32, u64, u64)> = t
                .entries
                .iter()
                .enumerate()
                .map(|(i, e)| {
                    (
                        e.name.to_lowercase(),
                        (t.sel[i], e.thumb, e.icon, e.size, e.modified),
                    )
                })
                .collect();
            let focused = t
                .focus
                .and_then(|p| t.order.get(p))
                .map(|&i| t.entries[i as usize].name.to_lowercase());
            let mut sel = vec![false; entries.len()];
            for (i, e) in entries.iter_mut().enumerate() {
                if let Some(&(s, th, ic, size, mtime)) = old.get(&e.name.to_lowercase()) {
                    sel[i] = s;
                    if size == e.size && mtime == e.modified {
                        e.thumb = match th {
                            Thumb::Pending(_) => Thumb::Unknown,
                            other => other,
                        };
                        if icons::needs_own_icon(&e.ext, e.is_dir, e.attrs) {
                            e.icon = ic;
                        }
                    }
                }
            }
            t.entries = entries;
            t.sel = sel;
            t.focus = None;
            t.anchor = None;
            t.rebuild_order(show_hidden, show_protected);
            if let Some(name) = focused {
                t.focus = t.position_of_name(&name);
                t.anchor = t.focus;
            }
        } else {
            t.sel = vec![false; entries.len()];
            t.entries = entries;
            t.rebuild_order(show_hidden, show_protected);
            let key = Tab::scroll_key(&t.loc);
            if let Some(s) = t.scroll_memory.get(&key) {
                t.scroll = *s;
            }
        }
        if let Some((name, rename)) = t.pending_select.take() {
            if let Some(pos) = t.position_of_name(&name) {
                t.select_only(pos);
                self.ensure_visible(pos);
                if rename && ti == self.active {
                    self.begin_rename();
                }
            } else if t.loading {
                t.pending_select = Some((name, rename));
            }
        }
        self.release_thumbnails();
        self.request_thumbnails();
        self.invalidate();
    }

    fn on_found(&mut self, tab: u64, gen: u64, entries: Vec<Entry>, done: bool) {
        let (show_hidden, show_protected) = (self.cfg.show_hidden, self.cfg.show_protected);
        let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab && t.gen == gen) else {
            return;
        };
        if done {
            t.loading = false;
        }
        if !entries.is_empty() {
            t.sel.extend(std::iter::repeat_n(false, entries.len()));
            t.entries.extend(entries);
            t.rebuild_order(show_hidden, show_protected);
            self.request_thumbnails();
        }
        self.invalidate();
    }

    /// Current grid for the active tab (as last laid out).
    pub fn grid(&self) -> Grid {
        let t = self.tab();
        Grid::new(
            t.view.mode,
            self.geo.items,
            t.order.len(),
            self.cfg.compact,
            t.view.details_width(),
        )
    }

    pub fn clamp_scroll(&mut self) {
        let g = self.grid();
        let t = self.tab_mut();
        t.scroll = t.scroll.clamp(0.0, g.max_scroll());
    }

    pub fn ensure_visible(&mut self, pos: usize) {
        let g = self.grid();
        let t = self.tab_mut();
        t.scroll = g.scroll_to(pos, t.scroll);
        self.request_thumbnails();
    }

    /// Queues thumbnails: visible items first, then the next screenful.
    pub fn request_thumbnails(&mut self) {
        let g = self.grid();
        let scale = self.scale;
        let t = &mut self.tabs[self.active];
        if !t.view.mode.shows_thumbnails() || t.db.is_none() && t.loc.folder().is_some() {
            self.pool.set(Vec::new());
            return;
        }
        let px = ((t.view.mode.icon() * scale).ceil() as u32).min(thumbs::STORE_DIM);
        for e in &mut t.entries {
            if let Thumb::Pending(_) = e.thumb {
                e.thumb = Thumb::Unknown;
            }
        }
        let vis = g.visible(t.scroll);
        let ahead = vis.end..(vis.end + vis.len() * 2).min(t.order.len());
        let behind = vis.start.saturating_sub(vis.len())..vis.start;
        let mut jobs = Vec::new();
        let central_db = t.db.clone();
        for pos in vis.chain(ahead).chain(behind.rev()) {
            let idx = t.order[pos];
            let e = &mut t.entries[idx as usize];
            if e.is_dir {
                continue;
            }
            let need = match e.thumb {
                Thumb::Unknown => true,
                Thumb::Ready(_, have) => have < px,
                _ => false,
            };
            if !need {
                continue;
            }
            // Search results come from many folders: use each file's own folder db.
            let db = match (&t.loc, &central_db) {
                (Location::Search { .. }, _) | (_, None) => {
                    let parent = e.path.parent().map(Path::to_path_buf).unwrap_or_default();
                    Arc::new(Mutex::new(thumbs::open_db(&parent, self.cfg.thumbs)))
                }
                (_, Some(db)) => db.clone(),
            };
            e.thumb = Thumb::Pending(px);
            jobs.push(thumbs::Req {
                tab: t.id,
                gen: t.gen,
                idx,
                path: e.path.clone(),
                name: e.name.clone(),
                size: e.size,
                mtime: e.modified,
                px,
                db,
            });
            if jobs.len() >= 400 {
                break;
            }
        }
        self.pool.set(jobs);
    }

    pub fn set_view_mode(&mut self, mode: ViewMode) {
        let focus = self.tab().focus;
        let t = self.tab_mut();
        if t.view.mode == mode {
            return;
        }
        t.view.mode = mode;
        t.save_view();
        // Keep the focused item in view across layouts.
        t.scroll = 0.0;
        if let Some(f) = focus {
            self.ensure_visible(f);
        }
        self.request_thumbnails();
        self.invalidate();
    }

    pub fn set_sort(&mut self, col: fs::Column, asc: Option<bool>) {
        let (sh, sp) = (self.cfg.show_hidden, self.cfg.show_protected);
        let t = self.tab_mut();
        let asc = asc.unwrap_or(if t.view.sort == col { !t.view.asc } else { true });
        t.view.sort = col;
        t.view.asc = asc;
        t.rebuild_order(sh, sp);
        t.save_view();
        self.request_thumbnails();
        self.invalidate();
    }

    pub fn reapply_filters(&mut self) {
        let (sh, sp) = (self.cfg.show_hidden, self.cfg.show_protected);
        for t in &mut self.tabs {
            t.rebuild_order(sh, sp);
        }
        self.clamp_scroll();
        self.request_thumbnails();
        self.invalidate();
    }
}
