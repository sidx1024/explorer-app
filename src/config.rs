//! Global settings in `%LOCALAPPDATA%\Explorer26\config.ini` (human-editable).
//!
//! Per-folder settings (view, sort, column widths) do not live here: they are stored with that
//! folder's thumbnails in its `thumbs26.db` (see `store.rs` / `thumbs.rs`).

use std::collections::HashMap;
use std::path::PathBuf;

use crate::store::parse_kv;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ThumbLocation {
    /// One `<hash>.thumbs26.db` per folder under `%LOCALAPPDATA%\Explorer26\folders`.
    Central,
    /// A hidden `thumbs26.db` inside each folder (falls back to central when not writable).
    InFolder,
}

#[derive(Clone)]
pub struct Config {
    pub theme: u32,
    pub mica: bool,
    pub show_hidden: bool,
    pub show_protected: bool,
    pub show_extensions: bool,
    pub compact: bool,
    pub thumbs: ThumbLocation,
    pub nav_width: f32,
    pub show_preview: bool,
    /// Keep an index of all fixed drives for instant search.
    pub index_drives: bool,
    /// Search options: include system files.
    pub search_system_files: bool,
    pub preview_width: f32,
    pub window: Option<[i32; 5]>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            theme: crate::theme::PREF_SYSTEM,
            mica: true,
            show_hidden: false,
            show_protected: false,
            show_extensions: true,
            compact: false,
            thumbs: ThumbLocation::Central,
            nav_width: 220.0,
            show_preview: false,
            index_drives: true,
            search_system_files: false,
            preview_width: 380.0,
            window: None,
        }
    }
}

pub fn path() -> PathBuf {
    crate::platform::app_data_dir().join("config.ini")
}

fn b(m: &HashMap<String, String>, k: &str, d: bool) -> bool {
    m.get(k)
        .map(|v| matches!(v.as_str(), "1" | "true" | "yes" | "on"))
        .unwrap_or(d)
}

impl Config {
    pub fn load() -> Config {
        let d = Config::default();
        let Ok(text) = std::fs::read_to_string(path()) else {
            return d;
        };
        let m = parse_kv(&text);
        Config {
            theme: match m.get("theme").map(String::as_str) {
                Some("light") => crate::theme::PREF_LIGHT,
                Some("dark") => crate::theme::PREF_DARK,
                _ => crate::theme::PREF_SYSTEM,
            },
            mica: b(&m, "mica", d.mica),
            show_hidden: b(&m, "show_hidden", d.show_hidden),
            show_protected: b(&m, "show_protected_os_files", d.show_protected),
            show_extensions: b(&m, "show_extensions", d.show_extensions),
            compact: b(&m, "compact_view", d.compact),
            thumbs: match m.get("thumbnail_store").map(String::as_str) {
                Some("folder") => ThumbLocation::InFolder,
                _ => ThumbLocation::Central,
            },
            nav_width: m
                .get("nav_width")
                .and_then(|v| v.parse().ok())
                .unwrap_or(d.nav_width),
            show_preview: b(&m, "preview_pane", d.show_preview),
            index_drives: b(&m, "index_drives", d.index_drives),
            search_system_files: b(&m, "search_system_files", d.search_system_files),
            preview_width: m
                .get("preview_width")
                .and_then(|v| v.parse().ok())
                .unwrap_or(d.preview_width),
            window: m.get("window").and_then(|v| {
                let p: Vec<i32> = v.split(',').filter_map(|x| x.trim().parse().ok()).collect();
                (p.len() == 5).then(|| [p[0], p[1], p[2], p[3], p[4]])
            }),
        }
    }

    pub fn save(&self) {
        let theme = match self.theme {
            crate::theme::PREF_LIGHT => "light",
            crate::theme::PREF_DARK => "dark",
            _ => "system",
        };
        let window = self
            .window
            .map(|w| format!("{},{},{},{},{}", w[0], w[1], w[2], w[3], w[4]))
            .unwrap_or_default();
        let text = format!(
            "; Explorer26 settings. Per-folder view settings are stored in each folder's thumbs26.db.\n\
             [appearance]\n\
             theme={theme}\n\
             mica={}\n\
             compact_view={}\n\
             \n\
             [files]\n\
             show_hidden={}\n\
             show_protected_os_files={}\n\
             show_extensions={}\n\
             ; central = %LOCALAPPDATA%\\Explorer26\\folders, folder = hidden thumbs26.db inside each folder\n\
             thumbnail_store={}\n\
             ; index fixed drives for instant search (stored in %LOCALAPPDATA%\\Explorer26\\index)\n\
             index_drives={}\n\
             search_system_files={}\n\
             \n\
             [layout]\n\
             nav_width={}\n\
             preview_pane={}\n\
             preview_width={}\n\
             window={window}\n",
            self.mica,
            self.compact,
            self.show_hidden,
            self.show_protected,
            self.show_extensions,
            match self.thumbs {
                ThumbLocation::Central => "central",
                ThumbLocation::InFolder => "folder",
            },
            self.index_drives,
            self.search_system_files,
            self.nav_width.round(),
            self.show_preview,
            self.preview_width.round(),
        );
        let p = path();
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        // Write-then-rename so a crash never leaves a half-written config.
        let tmp = p.with_extension("ini.tmp");
        if std::fs::write(&tmp, text).is_ok() {
            let _ = std::fs::rename(&tmp, &p);
        }
    }
}
