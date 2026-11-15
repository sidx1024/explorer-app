#![windows_subsystem = "windows"]

mod config;
mod fs;
mod icons;
mod index;
mod msg;
mod platform;
mod preview;
mod render;
mod scene;
mod shell;
mod store;
mod theme;
mod thumbs;
mod ui;
mod watch;
mod window;

use std::path::PathBuf;

use windows::Win32::System::Ole::OleInitialize;
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};

fn main() {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        // OLE (not just COM): drag & drop and the clipboard need it on the UI thread.
        let _ = OleInitialize(None);
    }
    let path = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .map(|p| std::path::absolute(&p).unwrap_or(p));
    if let Err(e) = window::run(path) {
        eprintln!("explorer26: {e}");
    }
}
