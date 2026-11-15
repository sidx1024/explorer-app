//! Platform-neutral UI core.
//!
//! `App` receives abstract input ([`Input`]), paints into a [`Scene`](crate::scene::Scene), and asks
//! the host for anything that needs the OS ([`Effect`]: menus, shell operations, timers...). The
//! Win32 host lives in `window.rs`; a remote (web) host would feed the same inputs and stream the
//! same scenes (docs/ARCHITECTURE.md).

pub mod layout;
mod input;
pub mod nav;
mod paint;
pub mod tab;
pub mod textfield;
mod preview_pane;

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::mpsc::Receiver;
use std::sync::Arc;
use std::time::Instant;

use crate::config::Config;
use crate::msg::{Msg, Notifier};
use crate::scene::{ImageKey, ImageStore, Rect};
use crate::shell::Op;
use crate::theme::Theme;
use crate::thumbs;

pub use self::nav::Nav;
pub use self::tab::{Location, Tab};
use self::textfield::TextField;

#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct Mods {
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Button {
    Left,
    Right,
    Middle,
    Back,
    Forward,
}

/// Input in DIPs relative to the client area.
#[derive(Clone, Copy, Debug)]
pub enum Input {
    Move { x: f32, y: f32 },
    Down { x: f32, y: f32, button: Button, double: bool, mods: Mods },
    Up { x: f32, y: f32, button: Button, mods: Mods },
    Wheel { x: f32, y: f32, delta: f32, horizontal: bool, mods: Mods },
    Leave,
    Key { vk: u32, mods: Mods },
    Char(char),
    /// The window lost focus or a modal loop took the mouse.
    CaptureLost,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ViewMode {
    ExtraLarge,
    Large,
    Medium,
    Small,
    List,
    Details,
    Tiles,
}

impl ViewMode {
    pub const ALL: [ViewMode; 7] = [
        ViewMode::ExtraLarge,
        ViewMode::Large,
        ViewMode::Medium,
        ViewMode::Small,
        ViewMode::List,
        ViewMode::Details,
        ViewMode::Tiles,
    ];
    pub fn label(self) -> &'static str {
        match self {
            ViewMode::ExtraLarge => "Extra large icons",
            ViewMode::Large => "Large icons",
            ViewMode::Medium => "Medium icons",
            ViewMode::Small => "Small icons",
            ViewMode::List => "List",
            ViewMode::Details => "Details",
            ViewMode::Tiles => "Tiles",
        }
    }
    pub fn id(self) -> &'static str {
        match self {
            ViewMode::ExtraLarge => "xl",
            ViewMode::Large => "large",
            ViewMode::Medium => "medium",
            ViewMode::Small => "small",
            ViewMode::List => "list",
            ViewMode::Details => "details",
            ViewMode::Tiles => "tiles",
        }
    }
    pub fn from_id(s: &str) -> Option<ViewMode> {
        ViewMode::ALL.into_iter().find(|m| m.id() == s)
    }
    /// Icon edge in DIPs.
    pub fn icon(self) -> f32 {
        match self {
            ViewMode::ExtraLarge => 256.0,
            ViewMode::Large => 96.0,
            ViewMode::Medium => 48.0,
            ViewMode::Tiles => 48.0,
            _ => 16.0,
        }
    }
    pub fn shows_thumbnails(self) -> bool {
        matches!(
            self,
            ViewMode::ExtraLarge | ViewMode::Large | ViewMode::Medium | ViewMode::Tiles
        )
    }
}

/// Command bar buttons and other named actions.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Cmd {
    New,
    Cut,
    Copy,
    Paste,
    Rename,
    Delete,
    Sort,
    View,
    More,
    Properties,
    Preview,
    SearchOptions,
    CloseSearch,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Hit {
    None,
    Tab(usize),
    TabClose(usize),
    NewTab,
    /// Window caption buttons (HTMINBUTTON / HTMAXBUTTON / HTCLOSE); handled as non-client.
    Caption(u32),
    Back,
    Forward,
    Up,
    Refresh,
    Crumb(usize),
    CrumbChevron(usize),
    CrumbOverflow,
    Address,
    Search,
    SearchClear,
    Command(Cmd),
    Nav(usize),
    NavExpand(usize),
    NavArea,
    NavSplitter,
    Header(usize),
    HeaderEdge(usize),
    HeaderArea,
    Item(usize),
    Items,
    ScrollThumb,
    ScrollTrack,
    NavScrollThumb,
    StatusDetails,
    StatusTiles,
    PreviewSplitter,
    Preview,
    Rename,
}

impl Hit {
    pub fn is_button(self) -> bool {
        matches!(
            self,
            Hit::Tab(_)
                | Hit::TabClose(_)
                | Hit::NewTab
                | Hit::Back
                | Hit::Forward
                | Hit::Up
                | Hit::Refresh
                | Hit::Crumb(_)
                | Hit::CrumbChevron(_)
                | Hit::CrumbOverflow
                | Hit::SearchClear
                | Hit::Command(_)
                | Hit::Nav(_)
                | Hit::NavExpand(_)
                | Hit::Header(_)
                | Hit::StatusDetails
                | Hit::StatusTiles
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Focus {
    Items,
    Address,
    Search,
    Rename,
    Nav,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cursor {
    Arrow,
    SizeWE,
    IBeam,
}

/// Menus the UI asks the host to show; the choice comes back through `App::menu_chosen`.
#[derive(Clone, Debug)]
pub enum MenuKind {
    New,
    Sort,
    View,
    More,
    /// Subfolders of breadcrumb segment `n` (paths listed in `items`).
    Crumb(Vec<PathBuf>),
    TabMenu(usize),
    SearchOptions,
}

pub enum Effect {
    Capture(bool),
    /// Popup at a client point (DIPs) with (id, label, checked, enabled) items.
    Menu {
        kind: MenuKind,
        x: f32,
        y: f32,
        items: Vec<(u32, String, bool, bool)>,
    },
    ItemMenu {
        paths: Vec<PathBuf>,
        x: f32,
        y: f32,
        extended: bool,
    },
    BackgroundMenu {
        folder: Option<PathBuf>,
        x: f32,
        y: f32,
    },
    Open(PathBuf),
    Properties(Vec<PathBuf>),
    FileOp(Op),
    SetClipboard { paths: Vec<PathBuf>, cut: bool },
    Paste(PathBuf),
    Drag(Vec<PathBuf>),
    Timer { id: usize, ms: u32 },
    KillTimer(usize),
    Title(String),
    /// Show, move or remove the native preview handler (path, handler CLSID, rect in DIPs).
    NativePreview(Option<(PathBuf, windows::core::GUID, Rect)>),
    /// Window/taskbar icon: a system image list index, or -1 for the app icon.
    WindowIcon(i32),
    SaveConfig,
    ApplyTheme,
    OpenTerminal(PathBuf),
    /// Pin or unpin folders in Windows' Quick access (shared with File Explorer).
    Pin { paths: Vec<PathBuf>, pin: bool },
    NewWindow(Option<PathBuf>),
    CloseWindow,
    Beep,
}

pub const TIMER_CARET: usize = 1;
pub const TIMER_REFRESH: usize = 2;
pub const TIMER_LOADING: usize = 3;
pub const TIMER_SEARCH: usize = 4;
pub const TIMER_TOOLTIP: usize = 5;
pub const TIMER_PREVIEW: usize = 6;

pub enum Drag {
    None,
    /// Mouse is down on an item; becomes a shell drag once it moves far enough.
    Pending { x: f32, y: f32, pos: usize, select_on_up: bool },
    Band { x0: f32, y0: f32, base: Vec<bool>, x1: f32, y1: f32 },
    Column { col: usize, x: f32, w: f32 },
    Splitter { x: f32, w: f32 },
    PreviewSplitter { x: f32, w: f32 },
    Thumb { y: f32, scroll: f32, nav: bool },
    Button(Hit),
    Text { field: Focus },
}

pub struct App {
    pub cfg: Config,
    pub theme: Theme,
    pub tabs: Vec<Tab>,
    pub active: usize,
    pub nav: Nav,
    /// DIP size of the client area and pixels per DIP.
    pub size: (f32, f32),
    pub scale: f32,
    pub hits: Vec<(Rect, Hit)>,
    pub hover: Hit,
    pub mouse: (f32, f32),
    pub drag: Drag,
    pub focus: Focus,
    pub address: Option<TextField>,
    pub search: TextField,
    pub rename: Option<(usize, TextField)>,
    pub caret_on: bool,
    pub window_active: bool,
    pub maximized: bool,
    pub caption_hover: u32,
    pub caption_pressed: u32,
    pub effects: Vec<Effect>,
    pub notifier: Notifier,
    pub rx: Receiver<Msg>,
    pub images: Arc<ImageStore>,
    pub pool: thumbs::Pool,
    pub cut: HashSet<PathBuf>,
    pub drop_target: Option<PathBuf>,
    pub typeahead: (String, Instant),
    pub dirty: bool,
    next_tab_id: u64,
    /// Rects of things laid out during paint that input needs (DIPs).
    pub geo: paint::Geometry,
    pub tooltip: Option<(String, f32, f32)>,
    pub preview: PreviewState,
}

impl App {
    pub fn new(
        cfg: Config,
        theme: Theme,
        notifier: Notifier,
        rx: Receiver<Msg>,
        images: Arc<ImageStore>,
        start: Location,
    ) -> App {
        let pool = thumbs::Pool::new(4, notifier.clone(), images.clone());
        let nav = Nav::new();
        let mut app = App {
            cfg,
            theme,
            tabs: Vec::new(),
            active: 0,
            nav,
            size: (800.0, 600.0),
            scale: 1.0,
            hits: Vec::new(),
            hover: Hit::None,
            mouse: (-1.0, -1.0),
            drag: Drag::None,
            focus: Focus::Items,
            address: None,
            search: TextField::default(),
            rename: None,
            caret_on: true,
            window_active: true,
            maximized: false,
            caption_hover: 0,
            caption_pressed: 0,
            effects: Vec::new(),
            notifier,
            rx,
            images,
            pool,
            cut: HashSet::new(),
            drop_target: None,
            typeahead: (String::new(), Instant::now()),
            dirty: true,
            next_tab_id: 1,
            geo: Default::default(),
            tooltip: None,
            preview: PreviewState::default(),
        };
        app.new_tab(start);
        app
    }

    pub fn tab(&self) -> &Tab {
        &self.tabs[self.active]
    }

    pub fn tab_mut(&mut self) -> &mut Tab {
        &mut self.tabs[self.active]
    }

    pub fn invalidate(&mut self) {
        self.dirty = true;
    }

    pub fn new_tab(&mut self, loc: Location) {
        let id = self.next_tab_id;
        self.next_tab_id += 1;
        let tab = Tab::new(id);
        let at = if self.tabs.is_empty() {
            0
        } else {
            self.active + 1
        };
        self.tabs.insert(at, tab);
        self.active = at;
        self.end_editing();
        self.load(at, loc, false);
        self.sync_title();
    }

    pub fn close_tab(&mut self, i: usize) {
        if i >= self.tabs.len() {
            return;
        }
        if self.tabs.len() == 1 {
            self.effects.push(Effect::CloseWindow);
            return;
        }
        let tab = self.tabs.remove(i);
        tab.close();
        if self.active > i || self.active >= self.tabs.len() {
            self.active = self.active.saturating_sub(1);
        }
        self.end_editing();
        self.release_thumbnails();
        self.sync_title();
        self.request_thumbnails();
        self.invalidate();
    }

    pub fn activate_tab(&mut self, i: usize) {
        if i < self.tabs.len() && i != self.active {
            self.end_editing();
            self.active = i;
            self.sync_title();
            self.request_thumbnails();
            self.invalidate();
        }
    }

    pub fn sync_title(&mut self) {
        let t = self.tab().title();
        self.effects.push(Effect::Title(t));
        self.effects.push(Effect::WindowIcon(self.tab().icon));
        let s = match &self.tab().loc {
            Location::Search { query, .. } => query.clone(),
            _ => String::new(),
        };
        if self.focus != Focus::Search {
            self.search.set(&s);
        }
    }

    pub fn end_editing(&mut self) {
        self.address = None;
        self.rename = None;
        if matches!(self.focus, Focus::Address | Focus::Rename) {
            self.focus = Focus::Items;
        }
    }

    /// Drops decoded thumbnails no tab references any more.
    pub fn release_thumbnails(&mut self) {
        let mut keep = HashSet::new();
        for t in &self.tabs {
            for e in &t.entries {
                if let crate::fs::Thumb::Ready(id, _) = e.thumb {
                    keep.insert(id);
                }
            }
        }
        if let Some(ImageKey::Thumb(id)) = self.preview.image {
            keep.insert(id);
        }
        self.images.retain_thumbs(|id| keep.contains(&id));
    }

    pub fn icon_key(&self, index: i32, dip: f32) -> ImageKey {
        ImageKey::SysIcon {
            index,
            list: crate::icons::list_for_px(dip * self.scale),
        }
    }

    pub fn take_effects(&mut self) -> Vec<Effect> {
        std::mem::take(&mut self.effects)
    }
}

/// Menu ids the host needs for the items it adds to shell context menus.
pub mod input_ids {
    pub use super::input::{
        M_ITEM_COPY_PATH, M_ITEM_PIN, M_OPEN_TAB, M_OPEN_WINDOW, M_PASTE, M_REFRESH,
    };
}

impl App {
    /// Re-reads Windows' Quick access pins off the UI thread (they can change in File Explorer).
    pub fn refresh_quick_access(&self) {
        let notifier = self.notifier.clone();
        std::thread::spawn(move || {
            unsafe {
                let _ = windows::Win32::System::Com::CoInitializeEx(
                    None,
                    windows::Win32::System::Com::COINIT_APARTMENTTHREADED,
                );
            }
            notifier.send(Msg::QuickAccess(nav::load_quick_access()));
        });
    }
}

/// What the preview pane shows.
pub enum PreviewView {
    Message(&'static str),
    Loading,
    Image,
    Text(Vec<String>),
    Native(windows::core::GUID),
    Info,
}

pub struct PreviewState {
    /// File being previewed: path, extension, is folder.
    pub target: Option<(PathBuf, String, bool)>,
    pub gen: u64,
    pub view: PreviewView,
    pub image: Option<ImageKey>,
    pub scroll: f32,
    /// What the host currently shows natively (to send changes only).
    pub native_shown: Option<(PathBuf, windows::core::GUID, Rect)>,
}

impl Default for PreviewState {
    fn default() -> Self {
        PreviewState {
            target: None,
            gen: 0,
            view: PreviewView::Message("Select a file to preview."),
            image: None,
            scroll: 0.0,
            native_shown: None,
        }
    }
}
