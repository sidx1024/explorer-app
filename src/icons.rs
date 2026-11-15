//! Real shell icons and type names (the same ones File Explorer shows), via SHGetFileInfo and the
//! system image list. Extension-based lookups are cached; per-file icons (exe, lnk, ico, customized
//! folders) are resolved in a second pass off the UI thread.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};

use windows::core::PCWSTR;
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, DeleteDC, DeleteObject, GetDIBits, GetObjectW, BITMAP, BITMAPINFO,
    BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HBITMAP, HGDIOBJ,
};
use windows::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_READONLY,
    FILE_ATTRIBUTE_SYSTEM, FILE_FLAGS_AND_ATTRIBUTES,
};
use windows::Win32::UI::Controls::{IImageList, ILD_TRANSPARENT};
use windows::Win32::UI::Shell::{
    SHGetFileInfoW, SHGetImageList, SHFILEINFOW, SHGFI_DISPLAYNAME, SHGFI_FLAGS, SHGFI_ICON,
    SHGFI_OVERLAYINDEX, SHGFI_SYSICONINDEX, SHGFI_TYPENAME, SHGFI_USEFILEATTRIBUTES,
    SHIL_EXTRALARGE, SHIL_JUMBO, SHIL_LARGE, SHIL_SMALL,
};
use windows::Win32::UI::WindowsAndMessaging::{DestroyIcon, GetIconInfo, HICON, ICONINFO};

use crate::platform::{from_wide, wide};
use crate::scene::Bitmap;

pub const LIST_SMALL: u8 = SHIL_SMALL as u8;
pub const LIST_LARGE: u8 = SHIL_LARGE as u8;
pub const LIST_XL: u8 = SHIL_EXTRALARGE as u8;
pub const LIST_JUMBO: u8 = SHIL_JUMBO as u8;

#[derive(Default)]
struct Cache {
    ext_icon: HashMap<String, i32>,
    type_name: HashMap<String, Arc<str>>,
}

fn cache() -> &'static Mutex<Cache> {
    static C: OnceLock<Mutex<Cache>> = OnceLock::new();
    C.get_or_init(Default::default)
}

fn file_info(path: &str, attrs: u32, flags: SHGFI_FLAGS) -> Option<SHFILEINFOW> {
    let mut sfi = SHFILEINFOW::default();
    let w = wide(path);
    let r = unsafe {
        SHGetFileInfoW(
            PCWSTR(w.as_ptr()),
            FILE_FLAGS_AND_ATTRIBUTES(attrs),
            Some(&mut sfi),
            std::mem::size_of::<SHFILEINFOW>() as u32,
            flags,
        )
    };
    if flags.0 & SHGFI_ICON.0 != 0 && !sfi.hIcon.is_invalid() {
        unsafe {
            let _ = DestroyIcon(sfi.hIcon);
        }
    }
    (r != 0).then_some(sfi)
}

/// Icon for files of this extension (no disk access).
pub fn ext_icon(ext: &str) -> i32 {
    if let Some(i) = cache().lock().unwrap().ext_icon.get(ext) {
        return *i;
    }
    let probe = if ext.is_empty() {
        "file".to_string()
    } else {
        format!("file{ext}")
    };
    let icon = file_info(
        &probe,
        FILE_ATTRIBUTE_NORMAL.0,
        SHGFI_SYSICONINDEX | SHGFI_USEFILEATTRIBUTES,
    )
    .map(|s| s.iIcon)
    .unwrap_or(-1);
    cache()
        .lock()
        .unwrap()
        .ext_icon
        .insert(ext.to_string(), icon);
    icon
}

pub fn folder_icon() -> i32 {
    static I: OnceLock<i32> = OnceLock::new();
    *I.get_or_init(|| {
        file_info(
            "folder",
            FILE_ATTRIBUTE_DIRECTORY.0,
            SHGFI_SYSICONINDEX | SHGFI_USEFILEATTRIBUTES,
        )
        .map(|s| s.iIcon)
        .unwrap_or(-1)
    })
}

pub fn folder_type_name() -> Arc<str> {
    static T: OnceLock<Arc<str>> = OnceLock::new();
    T.get_or_init(|| {
        file_info(
            "folder",
            FILE_ATTRIBUTE_DIRECTORY.0,
            SHGFI_TYPENAME | SHGFI_USEFILEATTRIBUTES,
        )
        .map(|s| from_wide(&s.szTypeName))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "File folder".into())
        .into()
    })
    .clone()
}

/// "JPEG File", "Text Document", ... as registered on this PC.
pub fn type_name(ext: &str) -> Arc<str> {
    if let Some(t) = cache().lock().unwrap().type_name.get(ext) {
        return t.clone();
    }
    let probe = format!("file{ext}");
    let name = file_info(
        &probe,
        FILE_ATTRIBUTE_NORMAL.0,
        SHGFI_TYPENAME | SHGFI_USEFILEATTRIBUTES,
    )
    .map(|s| from_wide(&s.szTypeName))
    .filter(|s| !s.is_empty())
    .unwrap_or_else(|| {
        if ext.is_empty() {
            "File".to_string()
        } else {
            format!("{} File", ext.trim_start_matches('.').to_uppercase())
        }
    });
    let name: Arc<str> = name.into();
    cache()
        .lock()
        .unwrap()
        .type_name
        .insert(ext.to_string(), name.clone());
    name
}

/// Whether this item needs its own icon looked up from disk instead of the per-extension one.
pub fn needs_own_icon(ext: &str, is_dir: bool, attrs: u32) -> bool {
    if is_dir {
        // Customized folders (desktop.ini) carry the read-only or system attribute.
        return attrs & (FILE_ATTRIBUTE_READONLY.0 | FILE_ATTRIBUTE_SYSTEM.0) != 0;
    }
    matches!(
        ext,
        ".exe" | ".lnk" | ".ico" | ".url" | ".cur" | ".ani" | ".msc" | ".scr" | ".cpl"
            | ".appref-ms" | ".website"
    )
}

/// The item's own icon, with the shortcut overlay for links.
pub fn path_icon(path: &Path, is_dir: bool) -> i32 {
    let p = path.to_string_lossy();
    let overlay = !is_dir && p.to_lowercase().ends_with(".lnk");
    let flags = if overlay {
        SHGFI_SYSICONINDEX | SHGFI_ICON | SHGFI_OVERLAYINDEX
    } else {
        SHGFI_SYSICONINDEX
    };
    file_info(&p, 0, flags).map(|s| s.iIcon).unwrap_or(-1)
}

/// Display name, icon and type for a drive root ("Local Disk (C:)").
pub fn drive_info(root: &str) -> (String, i32, String) {
    match file_info(root, 0, SHGFI_DISPLAYNAME | SHGFI_SYSICONINDEX | SHGFI_TYPENAME) {
        Some(s) => (
            from_wide(&s.szDisplayName),
            s.iIcon,
            from_wide(&s.szTypeName),
        ),
        None => (root.to_string(), folder_icon(), "Drive".into()),
    }
}

/// Icon of any shell path (known folders, drives) for the navigation pane.
pub fn any_icon(path: &Path) -> i32 {
    file_info(&path.to_string_lossy(), 0, SHGFI_SYSICONINDEX)
        .map(|s| s.iIcon)
        .unwrap_or_else(folder_icon)
}

/// The smallest image list whose icons are at least `px` physical pixels.
pub fn list_for_px(px: f32) -> u8 {
    let sizes = list_sizes();
    for (list, size) in [
        (LIST_SMALL, sizes[0]),
        (LIST_LARGE, sizes[1]),
        (LIST_XL, sizes[2]),
    ] {
        if size as f32 >= px - 0.5 {
            return list;
        }
    }
    LIST_JUMBO
}

/// Pixel sizes of the small, large and extra-large system image lists (system-DPI dependent).
fn list_sizes() -> [i32; 3] {
    static S: OnceLock<[i32; 3]> = OnceLock::new();
    *S.get_or_init(|| {
        let mut out = [16, 32, 48];
        for (i, list) in [SHIL_SMALL, SHIL_LARGE, SHIL_EXTRALARGE].into_iter().enumerate() {
            if let Ok(il) = unsafe { SHGetImageList::<IImageList>(list as i32) } {
                let (mut cx, mut cy) = (0, 0);
                if unsafe { il.GetIconSize(&mut cx, &mut cy) }.is_ok() && cx > 0 {
                    out[i] = cx;
                }
            }
        }
        out
    })
}

/// Renders a system image list icon to premultiplied BGRA.
pub fn extract(index: i32, list: u8) -> Option<Bitmap> {
    if index < 0 {
        return None;
    }
    let il: IImageList = unsafe { SHGetImageList(list as i32).ok()? };
    let overlay = ((index as u32) >> 24) & 0xF;
    let base = index & 0x00FF_FFFF;
    let flags = ILD_TRANSPARENT.0 | (overlay << 8);
    let hicon = unsafe { il.GetIcon(base, flags).ok()? };
    let bmp = hicon_to_bitmap(hicon);
    unsafe {
        let _ = DestroyIcon(hicon);
    }
    let mut bmp = bmp?;
    if list == LIST_JUMBO {
        // Types without a 256 px icon come back as a small icon in the top-left corner of the
        // jumbo canvas; crop it so it's drawn centered at its real size instead.
        if let Some(b) = crop_small_jumbo(&bmp) {
            bmp = b;
        }
    }
    Some(bmp)
}

fn crop_small_jumbo(b: &Bitmap) -> Option<Bitmap> {
    let (w, h) = (b.w as usize, b.h as usize);
    let (mut maxx, mut maxy) = (0usize, 0usize);
    for y in 0..h {
        for x in 0..w {
            if b.data[(y * w + x) * 4 + 3] != 0 {
                maxx = maxx.max(x + 1);
                maxy = maxy.max(y + 1);
            }
        }
    }
    if maxx == 0 || maxx > 64 || maxy > 64 || w < 128 {
        return None;
    }
    let side = maxx.max(maxy);
    let mut data = vec![0u8; side * side * 4];
    for y in 0..maxy {
        let src = &b.data[(y * w) * 4..(y * w + maxx) * 4];
        data[(y * side) * 4..(y * side + maxx) * 4].copy_from_slice(src);
    }
    Some(Bitmap {
        w: side as u32,
        h: side as u32,
        data,
    })
}

/// HICON to premultiplied BGRA, synthesizing alpha from the mask for old icons without it.
pub fn hicon_to_bitmap(hicon: HICON) -> Option<Bitmap> {
    unsafe {
        let mut info = ICONINFO::default();
        GetIconInfo(hicon, &mut info).ok()?;
        let color = info.hbmColor;
        let mask = info.hbmMask;
        let result = (|| {
            if color.is_invalid() {
                return None;
            }
            let (w, h, mut px) = hbitmap_pixels(color)?;
            if px.chunks_exact(4).all(|p| p[3] == 0) {
                // No alpha channel: opaque where the AND mask is 0.
                if let Some((mw, mh, m)) = hbitmap_pixels(mask) {
                    if mw == w && mh >= h {
                        for (i, p) in px.chunks_exact_mut(4).enumerate() {
                            p[3] = if m[i * 4] == 0 { 255 } else { 0 };
                        }
                    }
                } else {
                    px.chunks_exact_mut(4).for_each(|p| p[3] = 255);
                }
            }
            premultiply(&mut px);
            Some(Bitmap { w, h, data: px })
        })();
        if !color.is_invalid() {
            let _ = DeleteObject(HGDIOBJ(color.0));
        }
        if !mask.is_invalid() {
            let _ = DeleteObject(HGDIOBJ(mask.0));
        }
        result
    }
}

/// Top-down 32 bpp pixels of a bitmap.
pub fn hbitmap_pixels(hbm: HBITMAP) -> Option<(u32, u32, Vec<u8>)> {
    unsafe {
        let mut bm = BITMAP::default();
        if GetObjectW(
            HGDIOBJ(hbm.0),
            std::mem::size_of::<BITMAP>() as i32,
            Some(&mut bm as *mut _ as _),
        ) == 0
        {
            return None;
        }
        let (w, h) = (bm.bmWidth, bm.bmHeight.abs());
        if w <= 0 || h <= 0 {
            return None;
        }
        let mut bi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut px = vec![0u8; (w * h * 4) as usize];
        let dc = CreateCompatibleDC(None);
        let lines = GetDIBits(
            dc,
            hbm,
            0,
            h as u32,
            Some(px.as_mut_ptr() as _),
            &mut bi,
            DIB_RGB_COLORS,
        );
        let _ = DeleteDC(dc);
        (lines > 0).then_some((w as u32, h as u32, px))
    }
}

pub fn premultiply(px: &mut [u8]) {
    for p in px.chunks_exact_mut(4) {
        let a = p[3] as u32;
        if a < 255 {
            p[0] = ((p[0] as u32 * a + 127) / 255) as u8;
            p[1] = ((p[1] as u32 * a + 127) / 255) as u8;
            p[2] = ((p[2] as u32 * a + 127) / 255) as u8;
        }
    }
}

/// Icon of a virtual known folder (This PC, Recycle Bin...) via its PIDL.
pub fn known_folder_icon(id: &windows::core::GUID) -> i32 {
    use windows::Win32::UI::Shell::{SHGetKnownFolderIDList, KF_FLAG_DEFAULT, SHGFI_PIDL};
    unsafe {
        let Ok(pidl) = SHGetKnownFolderIDList(id, KF_FLAG_DEFAULT.0 as u32, None) else {
            return folder_icon();
        };
        let mut sfi = SHFILEINFOW::default();
        let r = SHGetFileInfoW(
            PCWSTR(pidl as *const u16),
            FILE_FLAGS_AND_ATTRIBUTES(0),
            Some(&mut sfi),
            std::mem::size_of::<SHFILEINFOW>() as u32,
            SHGFI_PIDL | SHGFI_SYSICONINDEX,
        );
        windows::Win32::System::Com::CoTaskMemFree(Some(pidl as _));
        if r != 0 { sfi.iIcon } else { folder_icon() }
    }
}

/// A system image list icon as an HICON (caller destroys it), e.g. for the window/taskbar icon.
pub fn hicon(index: i32, list: u8) -> Option<HICON> {
    if index < 0 {
        return None;
    }
    unsafe {
        let il: IImageList = SHGetImageList(list as i32).ok()?;
        il.GetIcon(index & 0x00FF_FFFF, ILD_TRANSPARENT.0).ok()
    }
}
