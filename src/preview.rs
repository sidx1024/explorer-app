//! Preview pane content.
//!
//! What to show for a file is decided off the UI thread:
//! 1. Images: decoded directly with WIC at pane size (EXIF orientation applied), drawn into the scene.
//! 2. A registered shell preview handler (Office, PDF, fonts, media...): hosted natively with
//!    `IPreviewHandler`, exactly like File Explorer (see [`NativePreview`]).
//! 3. Text and source files: our own monospace text view.
//! 4. Anything else with a thumbnail (videos without a handler...), else icon + file details.
//!
//! Everything except (2) is part of the scene, so it streams to a remote canvas too.

use std::path::{Path, PathBuf};

use windows::core::{Interface, GUID, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HWND, RECT};
use windows::Win32::Graphics::Gdi::LOGFONTW;
use windows::Win32::System::Com::{
    CLSIDFromString, CoCreateInstance, CLSCTX_INPROC_SERVER, CLSCTX_LOCAL_SERVER, STGM_READ,
    STGM_SHARE_DENY_NONE,
};
use windows::Win32::UI::Shell::PropertiesSystem::{
    IInitializeWithFile, IInitializeWithStream,
};
use windows::Win32::UI::Shell::{
    AssocQueryStringW, IInitializeWithItem, IPreviewHandler, IPreviewHandlerVisuals, SHCreateStreamOnFileEx,
    ASSOCF_INIT_DEFAULTTOSTAR, ASSOCSTR_SHELLEXTENSION,
};

use crate::platform::{from_wide, wide};
use crate::scene::{Bitmap, Color};

/// The shell extension point for preview handlers.
const PREVIEW_HANDLER_IID: &str = "{8895b1c6-b41f-4c1c-a562-0d564250836f}";

pub enum Content {
    /// A message instead of content ("Select a file to preview.").
    Message(&'static str),
    Loading,
    Image(Bitmap),
    Text(Vec<String>),
    /// Hosted by a native preview handler.
    Native(GUID),
    /// Big icon and details (no preview available).
    Info,
}

/// The preview handler registered for an extension, if any.
pub fn handler_for(ext: &str) -> Option<GUID> {
    if ext.is_empty() {
        return None;
    }
    let mut buf = [0u16; 64];
    let mut len = buf.len() as u32;
    unsafe {
        AssocQueryStringW(
            ASSOCF_INIT_DEFAULTTOSTAR,
            ASSOCSTR_SHELLEXTENSION,
            PCWSTR(wide(ext).as_ptr()),
            PCWSTR(wide(PREVIEW_HANDLER_IID).as_ptr()),
            Some(windows::core::PWSTR(buf.as_mut_ptr())),
            &mut len,
        )
        .ok()
        .ok()?;
        let s = from_wide(&buf);
        CLSIDFromString(PCWSTR(wide(&s).as_ptr())).ok()
    }
}

/// Decides and loads the content for a file (runs on a worker thread with COM initialized).
pub fn load(path: &Path, ext: &str, is_dir: bool, w: u32, h: u32) -> Content {
    if is_dir {
        return Content::Info;
    }
    let image = crate::fs::is_image_ext(ext);
    if !image {
        if let Some(clsid) = handler_for(ext) {
            return Content::Native(clsid);
        }
        if let Some(lines) = read_text(path) {
            return Content::Text(lines);
        }
    }
    let (w, h) = (w.clamp(64, 4096), h.clamp(64, 4096));
    // Images: decode directly at pane size (fast); the shell is the fallback for formats WIC
    // can't read and for videos and other files with thumbnail providers.
    let direct = if image { crate::thumbs::decode_file(path, w, h) } else { None };
    match direct.or_else(|| crate::thumbs::extract_any(path, w.max(h).min(1024))) {
        Some(b) => Content::Image(b),
        None => Content::Info,
    }
}

/// Up to 256 KB / 5000 lines of a file, if it looks like text (UTF-8 or UTF-16 with BOM).
fn read_text(path: &Path) -> Option<Vec<String>> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).ok()?;
    let mut buf = vec![0u8; 256 * 1024];
    let n = f.read(&mut buf).ok()?;
    buf.truncate(n);
    let text = if buf.starts_with(&[0xFF, 0xFE]) {
        let units: Vec<u16> = buf[2..]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        String::from_utf16_lossy(&units)
    } else {
        let body = buf.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(&buf);
        // Binary files have NULs early on.
        if body.iter().take(8192).any(|&b| b == 0) {
            return None;
        }
        let s = String::from_utf8_lossy(body);
        // Mostly-garbage decodes are binary in disguise.
        let bad = s.chars().take(8192).filter(|&c| c == '\u{FFFD}').count();
        if bad > 16 {
            return None;
        }
        s.into_owned()
    };
    Some(
        text.lines()
            .take(5000)
            .map(|l| l.replace('\t', "    "))
            .collect(),
    )
}

/// A shell preview handler living inside our window.
pub struct NativePreview {
    handler: IPreviewHandler,
    pub path: PathBuf,
    pub rect: RECT,
}

impl NativePreview {
    pub fn open(
        hwnd: HWND,
        clsid: &GUID,
        path: &Path,
        rect: RECT,
        bg: Color,
        fg: Color,
    ) -> Option<NativePreview> {
        unsafe {
            // Most handlers run out of process (prevhost.exe); some are in-process.
            let handler: IPreviewHandler =
                CoCreateInstance(clsid, None, CLSCTX_LOCAL_SERVER | CLSCTX_INPROC_SERVER).ok()?;
            let p = wide(path);
            let mode = (STGM_READ | STGM_SHARE_DENY_NONE).0;
            let ok = if let Ok(init) = handler.cast::<IInitializeWithStream>() {
                SHCreateStreamOnFileEx(PCWSTR(p.as_ptr()), mode, 0, false, None)
                    .and_then(|stream| init.Initialize(&stream, STGM_READ.0))
                    .is_ok()
            } else if let Ok(init) = handler.cast::<IInitializeWithFile>() {
                init.Initialize(PCWSTR(p.as_ptr()), STGM_READ.0).is_ok()
            } else if let Ok(init) = handler.cast::<IInitializeWithItem>() {
                crate::shell::item(path)
                    .and_then(|item| init.Initialize(&item, STGM_READ.0))
                    .is_ok()
            } else {
                false
            };
            if !ok {
                return None;
            }
            if let Ok(v) = handler.cast::<IPreviewHandlerVisuals>() {
                let _ = v.SetBackgroundColor(colorref(bg));
                let _ = v.SetTextColor(colorref(fg));
                let mut font = LOGFONTW {
                    lfHeight: -12,
                    ..Default::default()
                };
                for (i, c) in "Segoe UI".encode_utf16().enumerate() {
                    font.lfFaceName[i] = c;
                }
                let _ = v.SetFont(&font);
            }
            handler.SetWindow(hwnd, &rect).ok()?;
            if handler.DoPreview().is_err() {
                let _ = handler.Unload();
                return None;
            }
            Some(NativePreview {
                handler,
                path: path.to_path_buf(),
                rect,
            })
        }
    }

    pub fn set_rect(&mut self, rect: RECT) {
        if rect != self.rect {
            self.rect = rect;
            unsafe {
                let _ = self.handler.SetRect(&rect);
            }
        }
    }
}

impl Drop for NativePreview {
    fn drop(&mut self) {
        unsafe {
            let _ = self.handler.Unload();
        }
    }
}

fn colorref(c: Color) -> COLORREF {
    let b = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u32;
    COLORREF(b(c.r) | b(c.g) << 8 | b(c.b) << 16)
}
