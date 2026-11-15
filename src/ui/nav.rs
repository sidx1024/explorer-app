//! Navigation pane: Quick access (Windows' own pinned folders, shared with File Explorer) and
//! This PC with drives, as a lazily expanded tree (kept flattened: expanding inserts child rows
//! right after the parent).

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use windows::core::GUID;
use windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
use windows::Win32::UI::Shell::{
    FOLDERID_Desktop, FOLDERID_Documents, FOLDERID_Downloads, FOLDERID_Music, FOLDERID_Pictures,
    FOLDERID_Videos,
};

use crate::fs;
use crate::icons;
use crate::platform::known_folder;

use super::tab::{folder_name, Location};

#[derive(Clone)]
pub struct NavRow {
    pub label: String,
    pub loc: Location,
    pub icon: i32,
    pub depth: u8,
    pub expanded: bool,
    pub expandable: bool,
    /// Extra space above (section break).
    pub gap: bool,
    /// Pinned to Quick access (shows the pin glyph).
    pub pinned: bool,
}

pub struct Nav {
    pub rows: Vec<NavRow>,
    pub scroll: f32,
}

/// A Quick access entry: path, display name, icon, pinned.
pub type QuickItem = (PathBuf, String, i32, bool);

fn row(loc: Location, label: String, icon: i32, depth: u8) -> NavRow {
    NavRow {
        label,
        loc,
        icon,
        depth,
        expanded: false,
        expandable: true,
        gap: false,
        pinned: false,
    }
}

/// Reads Windows' Quick access pins; falls back to the standard user folders if the shell
/// namespace isn't available. Safe to call off the UI thread (needs COM).
pub fn load_quick_access() -> Vec<QuickItem> {
    if let Some(pins) = crate::shell::quick_access() {
        return pins
            .into_iter()
            .map(|(p, name)| {
                let icon = icons::any_icon(&p);
                (p, name, icon, true)
            })
            .collect();
    }
    let known: [&GUID; 6] = [
        &FOLDERID_Desktop,
        &FOLDERID_Downloads,
        &FOLDERID_Documents,
        &FOLDERID_Pictures,
        &FOLDERID_Music,
        &FOLDERID_Videos,
    ];
    known
        .into_iter()
        .filter_map(known_folder)
        .map(|p| {
            let icon = icons::any_icon(&p);
            (p.clone(), folder_name(&p), icon, false)
        })
        .collect()
}

impl Nav {
    pub fn new() -> Nav {
        let pc_icon = this_pc_icon();
        let mut pc = row(Location::ThisPc, "This PC".into(), pc_icon, 0);
        pc.gap = true;
        let mut nav = Nav {
            rows: vec![pc],
            scroll: 0.0,
        };
        nav.set_quick_access(load_quick_access());
        if let Some(i) = nav.rows.iter().position(|r| r.loc == Location::ThisPc) {
            nav.toggle(i);
        }
        nav
    }

    fn quick_end(&self) -> usize {
        self.rows
            .iter()
            .position(|r| r.loc == Location::ThisPc && r.depth == 0)
            .unwrap_or(0)
    }

    /// Replaces the Quick access section. Returns false (and keeps expanded subtrees) when the
    /// list is unchanged.
    pub fn set_quick_access(&mut self, items: Vec<QuickItem>) -> bool {
        let end = self.quick_end();
        let current: Vec<(&Location, &str, bool)> = self.rows[..end]
            .iter()
            .filter(|r| r.depth == 0)
            .map(|r| (&r.loc, r.label.as_str(), r.pinned))
            .collect();
        let same = current.len() == items.len()
            && current.iter().zip(&items).all(|((loc, label, pinned), (p, n, _, pin))| {
                loc.folder() == Some(p.as_path()) && label == n && pinned == pin
            });
        if same {
            return false;
        }
        let new_rows = items.into_iter().map(|(p, name, icon, pinned)| {
            let mut r = row(Location::Folder(p), name, icon, 0);
            r.pinned = pinned;
            r
        });
        self.rows.splice(0..end, new_rows);
        true
    }

    pub fn is_pinned(&self, p: &Path) -> bool {
        self.rows[..self.quick_end()]
            .iter()
            .any(|r| r.pinned && r.depth == 0 && r.loc.folder().is_some_and(|f| f == p))
    }

    pub fn toggle(&mut self, i: usize) {
        let Some(r) = self.rows.get(i).cloned() else {
            return;
        };
        if r.expanded {
            let end = (i + 1..self.rows.len())
                .find(|&j| self.rows[j].depth <= r.depth)
                .unwrap_or(self.rows.len());
            self.rows.drain(i + 1..end);
            self.rows[i].expanded = false;
            return;
        }
        let children = children_of(&r.loc, r.depth + 1);
        self.rows[i].expanded = true;
        self.rows[i].expandable = !children.is_empty();
        let tail = self.rows.split_off(i + 1);
        self.rows.extend(children);
        self.rows.extend(tail);
    }

    /// Re-reads drives (a USB stick was plugged in, ...).
    pub fn refresh_drives(&mut self) {
        if let Some(i) = self.rows.iter().position(|r| r.loc == Location::ThisPc) {
            if self.rows[i].expanded {
                self.toggle(i);
                self.toggle(i);
            }
        }
    }

}

pub(super) fn this_pc_icon() -> i32 {
    icons::known_folder_icon(&windows::Win32::UI::Shell::FOLDERID_ComputerFolder)
}

fn children_of(loc: &Location, depth: u8) -> Vec<NavRow> {
    match loc {
        Location::ThisPc => fs::list_this_pc()
            .into_iter()
            .map(|e| row(Location::Folder(e.path.clone()), e.name, e.icon, depth))
            .collect(),
        Location::Folder(p) => {
            let cancel = AtomicBool::new(false);
            let Ok(entries) = fs::list_dir(p, &cancel) else {
                return Vec::new();
            };
            let mut dirs: Vec<_> = entries
                .into_iter()
                .filter(|e| e.is_dir && !e.hidden() && e.attrs & FILE_ATTRIBUTE_REPARSE_POINT.0 == 0)
                .collect();
            dirs.sort_by(|a, b| fs::cmp_names(&a.name, &b.name));
            dirs.into_iter()
                .take(2000)
                .map(|e| {
                    let icon = if icons::needs_own_icon("", true, e.attrs) {
                        icons::path_icon(&e.path, true)
                    } else {
                        e.icon
                    };
                    row(Location::Folder(e.path.clone()), e.name, icon, depth)
                })
                .collect()
        }
        Location::Search { .. } => Vec::new(),
    }
}
