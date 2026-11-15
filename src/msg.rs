//! Messages from worker threads to the UI thread, delivered through a channel plus a posted
//! window message (coalesced so a burst of results wakes the UI once).

use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;

use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_APP};

use crate::fs::Entry;
use crate::scene::ImageKey;

pub const WM_APP_MSG: u32 = WM_APP + 1;

pub enum Msg {
    /// A folder listing finished (or failed).
    Listed {
        tab: u64,
        gen: u64,
        entries: Vec<Entry>,
        error: Option<String>,
        /// The folder's thumbs26.db (opened off the UI thread), unless refreshing.
        db: Option<crate::store::ThumbDb>,
    },
    /// Search results streaming in; `done` marks the end.
    Found {
        tab: u64,
        gen: u64,
        entries: Vec<Entry>,
        done: bool,
    },
    /// Per-file icons (exe, lnk, customized folders) resolved after the listing.
    Icons {
        tab: u64,
        gen: u64,
        icons: Vec<(u32, i32)>,
    },
    Thumb {
        tab: u64,
        gen: u64,
        idx: u32,
        key: Option<ImageKey>,
        px: u32,
    },
    /// The watched folder changed on disk.
    Changed { tab: u64, gen: u64 },
    /// A file operation finished; refresh tabs showing these folders.
    OpDone { folders: Vec<std::path::PathBuf> },
    /// Preview pane content loaded for generation `gen`.
    Preview { gen: u64, content: crate::preview::Content },
    /// Windows' Quick access pins were re-read.
    QuickAccess(Vec<crate::ui::nav::QuickItem>),
}

#[derive(Clone)]
pub struct Notifier {
    tx: Sender<Msg>,
    hwnd: Arc<AtomicIsize>,
    pending: Arc<AtomicBool>,
}

impl Notifier {
    pub fn new(tx: Sender<Msg>) -> Notifier {
        Notifier {
            tx,
            hwnd: Arc::new(AtomicIsize::new(0)),
            pending: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn set_hwnd(&self, hwnd: HWND) {
        self.hwnd.store(hwnd.0 as isize, Ordering::Release);
        self.wake();
    }

    pub fn send(&self, m: Msg) {
        if self.tx.send(m).is_ok() {
            self.wake();
        }
    }

    fn wake(&self) {
        let h = self.hwnd.load(Ordering::Acquire);
        if h != 0 && !self.pending.swap(true, Ordering::AcqRel) {
            unsafe {
                let _ = PostMessageW(
                    Some(HWND(h as *mut _)),
                    WM_APP_MSG,
                    WPARAM(0),
                    LPARAM(0),
                );
            }
        }
    }

    /// Called by the UI before draining the channel, so later sends post a new wake-up.
    pub fn begin_drain(&self) {
        self.pending.store(false, Ordering::Release);
    }
}
