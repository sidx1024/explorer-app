//! Small Win32 helpers: strings, file attributes, known folders, locale formatting.

use std::ffi::OsString;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

use windows::core::{w, GUID, PCWSTR};
use windows::Win32::Foundation::{FILETIME, SYSTEMTIME};
use windows::Win32::Globalization::{GetDateFormatEx, GetTimeFormatEx, DATE_SHORTDATE, TIME_NOSECONDS};
use windows::Win32::Storage::FileSystem::{
    GetFileAttributesW, SetFileAttributesW, FILE_ATTRIBUTE_HIDDEN, FILE_ATTRIBUTE_SYSTEM,
    FILE_FLAGS_AND_ATTRIBUTES, INVALID_FILE_ATTRIBUTES,
};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Registry::{RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ};
use windows::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};
use windows::Win32::UI::Shell::{
    SHGetKnownFolderPath, StrFormatByteSizeEx, StrFormatKBSizeW, KF_FLAG_DEFAULT,
    SFBS_FLAGS_ROUND_TO_NEAREST_DISPLAYED_DIGIT,
};

/// Null-terminated UTF-16 for Win32 calls.
pub fn wide(s: impl AsRef<std::ffi::OsStr>) -> Vec<u16> {
    s.as_ref().encode_wide().chain(std::iter::once(0)).collect()
}

pub fn from_wide(buf: &[u16]) -> String {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len])
}

pub fn path_from_wide(buf: &[u16]) -> PathBuf {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    PathBuf::from(OsString::from_wide(&buf[..len]))
}

/// Path usable with Win32 file APIs beyond MAX_PATH.
pub fn long_path(p: &Path) -> Vec<u16> {
    let s = p.as_os_str();
    if s.len() < 240 {
        return wide(s);
    }
    let text = p.to_string_lossy();
    if text.starts_with(r"\\?\") {
        wide(s)
    } else if let Some(unc) = text.strip_prefix(r"\\") {
        wide(format!(r"\\?\UNC\{unc}"))
    } else {
        wide(format!(r"\\?\{text}"))
    }
}

pub fn file_attributes(p: &Path) -> Option<u32> {
    let a = unsafe { GetFileAttributesW(PCWSTR(long_path(p).as_ptr())) };
    (a != INVALID_FILE_ATTRIBUTES).then_some(a)
}

pub fn set_hidden_system(p: &Path) {
    if let Some(a) = file_attributes(p) {
        let a = a | FILE_ATTRIBUTE_HIDDEN.0 | FILE_ATTRIBUTE_SYSTEM.0;
        unsafe {
            let _ = SetFileAttributesW(
                PCWSTR(long_path(p).as_ptr()),
                FILE_FLAGS_AND_ATTRIBUTES(a),
            );
        }
    }
}

pub fn clear_hidden_system(p: &Path) {
    if let Some(a) = file_attributes(p) {
        let mask = FILE_ATTRIBUTE_HIDDEN.0 | FILE_ATTRIBUTE_SYSTEM.0;
        if a & mask != 0 {
            unsafe {
                let _ = SetFileAttributesW(
                    PCWSTR(long_path(p).as_ptr()),
                    FILE_FLAGS_AND_ATTRIBUTES(a & !mask),
                );
            }
        }
    }
}

pub fn known_folder(id: &GUID) -> Option<PathBuf> {
    unsafe {
        let p = SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, None).ok()?;
        let path = PathBuf::from(OsString::from_wide(p.as_wide()));
        CoTaskMemFree(Some(p.0 as _));
        Some(path)
    }
}

/// %LOCALAPPDATA%\Explorer App (redirected per package when running from MSIX).
pub fn app_data_dir() -> PathBuf {
    let base = known_folder(&windows::Win32::UI::Shell::FOLDERID_LocalAppData)
        .or_else(|| std::env::var_os("LOCALAPPDATA").map(PathBuf::from))
        .unwrap_or_else(std::env::temp_dir);
    let dir = base.join("Explorer App");
    // One-time move from the app's previous name (keeps settings, thumbnails and the index).
    // Done once, before any thread can create the new folder.
    static MIGRATED: std::sync::Once = std::sync::Once::new();
    MIGRATED.call_once(|| {
        let old = base.join("Explorer26");
        if !dir.exists() && old.is_dir() {
            let _ = std::fs::rename(&old, &dir);
        }
    });
    dir
}

/// FILETIME (100 ns ticks since 1601) as local "short date + time", like Explorer's Date column.
pub fn format_date(ft: u64) -> String {
    if ft == 0 {
        return String::new();
    }
    unsafe {
        let f = FILETIME {
            dwLowDateTime: ft as u32,
            dwHighDateTime: (ft >> 32) as u32,
        };
        let mut utc = SYSTEMTIME::default();
        if FileTimeToSystemTime(&f, &mut utc).is_err() {
            return String::new();
        }
        let mut local = SYSTEMTIME::default();
        if SystemTimeToTzSpecificLocalTime(None, &utc, &mut local).is_err() {
            local = utc;
        }
        let mut d = [0u16; 64];
        let mut t = [0u16; 64];
        GetDateFormatEx(PCWSTR::null(), DATE_SHORTDATE, Some(&local), PCWSTR::null(), Some(&mut d), PCWSTR::null());
        GetTimeFormatEx(PCWSTR::null(), TIME_NOSECONDS, Some(&local), PCWSTR::null(), Some(&mut t));
        format!("{} {}", from_wide(&d), from_wide(&t))
    }
}

/// "1,234 KB" as in Explorer's Size column.
pub fn format_kb(size: u64) -> String {
    let mut buf = [0u16; 64];
    unsafe {
        let _ = StrFormatKBSizeW(size as i64, &mut buf);
    }
    from_wide(&buf)
}

/// "2.34 MB" as in the status bar and properties.
pub fn format_bytes(size: u64) -> String {
    let mut buf = [0u16; 64];
    unsafe {
        if StrFormatByteSizeEx(size, SFBS_FLAGS_ROUND_TO_NEAREST_DISPLAYED_DIGIT, &mut buf).is_err() {
            return format!("{size} bytes");
        }
    }
    from_wide(&buf)
}

/// Windows build number (e.g. 22631), read from the registry since GetVersionEx is shimmed.
pub fn windows_build() -> u32 {
    let mut buf = [0u16; 16];
    let mut size = std::mem::size_of_val(&buf) as u32;
    let r = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            w!(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion"),
            w!("CurrentBuildNumber"),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr() as _),
            Some(&mut size),
        )
    };
    if r.is_err() {
        return 0;
    }
    from_wide(&buf).parse().unwrap_or(0)
}

/// 64-bit FNV-1a, used to name per-folder cache files.
pub fn fnv1a64(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

/// Case-insensitive normalized key for a folder path (Windows paths are case-insensitive).
pub fn folder_key(p: &Path) -> String {
    let mut s = p.to_string_lossy().to_lowercase();
    while s.len() > 3 && (s.ends_with('\\') || s.ends_with('/')) {
        s.pop();
    }
    s
}

static MAIN_HWND: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);

pub fn set_main_hwnd(h: windows::Win32::Foundation::HWND) {
    MAIN_HWND.store(h.0 as isize, std::sync::atomic::Ordering::Release);
}

pub fn main_hwnd() -> windows::Win32::Foundation::HWND {
    windows::Win32::Foundation::HWND(
        MAIN_HWND.load(std::sync::atomic::Ordering::Acquire) as *mut _,
    )
}

const CF_UNICODETEXT: u32 = 13;

pub fn set_clipboard_text(s: &str) {
    use windows::Win32::Foundation::{GlobalFree, HANDLE};
    use windows::Win32::System::DataExchange::*;
    use windows::Win32::System::Memory::*;
    let w = wide(s);
    unsafe {
        let Ok(h) = GlobalAlloc(GMEM_MOVEABLE, w.len() * 2) else {
            return;
        };
        let p = GlobalLock(h) as *mut u16;
        if p.is_null() {
            let _ = GlobalFree(Some(h));
            return;
        }
        std::ptr::copy_nonoverlapping(w.as_ptr(), p, w.len());
        let _ = GlobalUnlock(h);
        if OpenClipboard(Some(main_hwnd())).is_err() {
            let _ = GlobalFree(Some(h));
            return;
        }
        let _ = EmptyClipboard();
        if SetClipboardData(CF_UNICODETEXT, Some(HANDLE(h.0))).is_err() {
            let _ = GlobalFree(Some(h));
        }
        let _ = CloseClipboard();
    }
}

pub fn get_clipboard_text() -> Option<String> {
    use windows::Win32::Foundation::HGLOBAL;
    use windows::Win32::System::DataExchange::*;
    use windows::Win32::System::Memory::*;
    unsafe {
        OpenClipboard(Some(main_hwnd())).ok()?;
        let r = (|| {
            let h = GetClipboardData(CF_UNICODETEXT).ok()?;
            let p = GlobalLock(HGLOBAL(h.0)) as *const u16;
            if p.is_null() {
                return None;
            }
            let mut n = 0;
            while *p.add(n) != 0 {
                n += 1;
            }
            let s = String::from_utf16_lossy(std::slice::from_raw_parts(p, n));
            let _ = GlobalUnlock(HGLOBAL(h.0));
            Some(s)
        })();
        let _ = CloseClipboard();
        r
    }
}
