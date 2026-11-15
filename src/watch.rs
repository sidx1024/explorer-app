//! Watches the open folder with ReadDirectoryChangesW so the view refreshes live. Changes to our own
//! `thumbs26.db` are ignored, otherwise writing thumbnails would keep refreshing the folder.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, ReadDirectoryChangesW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OVERLAPPED,
    FILE_LIST_DIRECTORY, FILE_NOTIFY_CHANGE_ATTRIBUTES, FILE_NOTIFY_CHANGE_DIR_NAME,
    FILE_NOTIFY_CHANGE_FILE_NAME, FILE_NOTIFY_CHANGE_LAST_WRITE, FILE_NOTIFY_CHANGE_SIZE,
    FILE_NOTIFY_INFORMATION, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows::Win32::System::Threading::{
    CreateEventW, SetEvent, WaitForMultipleObjects, INFINITE,
};

use crate::msg::{Msg, Notifier};
use crate::platform::long_path;

pub struct Watcher {
    stop: HANDLE,
    stopped: Arc<AtomicBool>,
}

// The event handle is only signaled/closed, which is thread-safe.
unsafe impl Send for Watcher {}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        unsafe {
            let _ = SetEvent(self.stop);
        }
    }
}

struct SendHandle(HANDLE);
unsafe impl Send for SendHandle {}

pub fn watch(dir: &Path, tab: u64, gen: u64, notifier: Notifier) -> Option<Watcher> {
    unsafe {
        let h = CreateFileW(
            PCWSTR(long_path(dir).as_ptr()),
            FILE_LIST_DIRECTORY.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OVERLAPPED,
            None,
        )
        .ok()?;
        let stop = CreateEventW(None, true, false, PCWSTR::null()).ok()?;
        let io_event = CreateEventW(None, true, false, PCWSTR::null()).ok()?;
        let stopped = Arc::new(AtomicBool::new(false));
        let (dir_h, stop_h, ev_h) = (SendHandle(h), SendHandle(stop), SendHandle(io_event));
        let stopped2 = stopped.clone();
        std::thread::spawn(move || {
            let (dir_h, stop_h, ev_h) = (dir_h, stop_h, ev_h);
            run(dir_h.0, stop_h.0, ev_h.0, tab, gen, notifier, stopped2);
            let _ = CloseHandle(dir_h.0);
            let _ = CloseHandle(ev_h.0);
            let _ = CloseHandle(stop_h.0);
        });
        Some(Watcher { stop, stopped })
    }
}

unsafe fn run(
    dir: HANDLE,
    stop: HANDLE,
    ev: HANDLE,
    tab: u64,
    gen: u64,
    notifier: Notifier,
    stopped: Arc<AtomicBool>,
) {
    // DWORD-aligned buffer as ReadDirectoryChangesW requires.
    let mut buf = vec![0u32; 16 * 1024];
    let filter = FILE_NOTIFY_CHANGE_FILE_NAME
        | FILE_NOTIFY_CHANGE_DIR_NAME
        | FILE_NOTIFY_CHANGE_SIZE
        | FILE_NOTIFY_CHANGE_LAST_WRITE
        | FILE_NOTIFY_CHANGE_ATTRIBUTES;
    loop {
        let mut ov = OVERLAPPED {
            hEvent: ev,
            ..Default::default()
        };
        if ReadDirectoryChangesW(
            dir,
            buf.as_mut_ptr() as _,
            (buf.len() * 4) as u32,
            false,
            filter,
            None,
            Some(&mut ov),
            None,
        )
        .is_err()
        {
            return;
        }
        let r = WaitForMultipleObjects(&[ev, stop], false, INFINITE);
        if r != WAIT_OBJECT_0 || stopped.load(Ordering::Acquire) {
            let _ = CancelIoEx(dir, Some(&ov));
            let mut n = 0;
            let _ = GetOverlappedResult(dir, &ov, &mut n, true);
            return;
        }
        let mut n = 0u32;
        if GetOverlappedResult(dir, &ov, &mut n, false).is_err() {
            return;
        }
        let relevant = if n == 0 {
            true // buffer overflow: something changed, details unknown
        } else {
            any_relevant(&buf, n as usize)
        };
        if relevant {
            notifier.send(Msg::Changed { tab, gen });
        }
        let _ = windows::Win32::System::Threading::ResetEvent(ev);
    }
}

unsafe fn any_relevant(buf: &[u32], len: usize) -> bool {
    let base = buf.as_ptr() as *const u8;
    let mut off = 0usize;
    loop {
        if off + std::mem::size_of::<FILE_NOTIFY_INFORMATION>() > len {
            return false;
        }
        let info = &*(base.add(off) as *const FILE_NOTIFY_INFORMATION);
        let name_len = info.FileNameLength as usize / 2;
        let name = std::slice::from_raw_parts(info.FileName.as_ptr(), name_len);
        let name = String::from_utf16_lossy(name).to_lowercase();
        if !name.starts_with(crate::store::FILE_NAME) {
            return true;
        }
        if info.NextEntryOffset == 0 {
            return false;
        }
        off += info.NextEntryOffset as usize;
    }
}
