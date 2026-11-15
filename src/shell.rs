//! Shell integration: the real context menus (with every installed extension), IFileOperation for
//! copy/move/delete/rename (native progress, conflict and UAC dialogs), the clipboard, properties,
//! opening files, and OLE drag & drop that delegates to the shell's own drop targets.

use std::cell::RefCell;
use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::path::{Path, PathBuf};

use windows::core::{implement, w, Interface, Ref, Result, BOOL, PCSTR, PCWSTR, PSTR};
use windows::Win32::Foundation::{
    GlobalFree, HANDLE, HGLOBAL, HWND, LPARAM, LRESULT, POINT, POINTL, WPARAM,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, IDataObject, CLSCTX_INPROC_SERVER,
    COINIT_APARTMENTTHREADED,
};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, IsClipboardFormatAvailable, OpenClipboard,
    RegisterClipboardFormatW, SetClipboardData,
};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::System::Ole::{
    IDropTarget, IDropTarget_Impl, CF_HDROP, DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_LINK,
    DROPEFFECT_MOVE, DROPEFFECT_NONE,
};
use windows::Win32::System::SystemServices::MODIFIERKEYS_FLAGS;
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::msg::{Msg, Notifier};
use crate::platform::{path_from_wide, wide};

/// Shell item for a filesystem path.
pub fn item(path: &Path) -> Result<IShellItem> {
    unsafe { SHCreateItemFromParsingName(PCWSTR(wide(path).as_ptr()), None) }
}

/// Shell item array for paths (must share a parent folder for context menus).
pub fn item_array(paths: &[PathBuf]) -> Result<IShellItemArray> {
    unsafe {
        let mut pidls: Vec<*const ITEMIDLIST> = Vec::with_capacity(paths.len());
        for p in paths {
            let mut pidl: *mut ITEMIDLIST = std::ptr::null_mut();
            if SHParseDisplayName(PCWSTR(wide(p).as_ptr()), None, &mut pidl, 0, None).is_ok()
                && !pidl.is_null()
            {
                pidls.push(pidl);
            }
        }
        let r = SHCreateShellItemArrayFromIDLists(&pidls);
        for p in pidls {
            CoTaskMemFree(Some(p as _));
        }
        r
    }
}

/// Opens a file with its default handler (or runs it).
pub fn open(hwnd: HWND, path: &Path) {
    let file = wide(path);
    let dir = path.parent().map(wide);
    let mut sei = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_INVOKEIDLIST | SEE_MASK_FLAG_LOG_USAGE,
        hwnd,
        lpFile: PCWSTR(file.as_ptr()),
        lpDirectory: dir
            .as_ref()
            .map(|d| PCWSTR(d.as_ptr()))
            .unwrap_or(PCWSTR::null()),
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };
    unsafe {
        let _ = ShellExecuteExW(&mut sei);
    }
}

pub fn open_with(hwnd: HWND, path: &Path) {
    let file = wide(path);
    let info = OPENASINFO {
        pcszFile: PCWSTR(file.as_ptr()),
        pcszClass: PCWSTR::null(),
        oaifInFlags: OAIF_ALLOW_REGISTRATION | OAIF_EXEC,
    };
    unsafe {
        let _ = SHOpenWithDialog(Some(hwnd), &info);
    }
}

pub fn open_terminal(dir: &Path) {
    let d = dir.to_string_lossy().trim_end_matches('\\').to_string();
    let args = wide(format!("-d \"{d}\\\""));
    let dirw = wide(dir);
    unsafe {
        let r = ShellExecuteW(
            None,
            w!("open"),
            w!("wt.exe"),
            PCWSTR(args.as_ptr()),
            PCWSTR(dirw.as_ptr()),
            SW_SHOWNORMAL,
        );
        if r.0 as isize <= 32 {
            let _ = ShellExecuteW(
                None,
                w!("open"),
                w!("powershell.exe"),
                PCWSTR::null(),
                PCWSTR(dirw.as_ptr()),
                SW_SHOWNORMAL,
            );
        }
    }
}

/// Target of a `.lnk` shortcut, if it points at a filesystem path.
pub fn resolve_link(path: &Path) -> Option<PathBuf> {
    unsafe {
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).ok()?;
        let pf: windows::Win32::System::Com::IPersistFile = link.cast().ok()?;
        pf.Load(PCWSTR(wide(path).as_ptr()), windows::Win32::System::Com::STGM_READ)
            .ok()?;
        let mut buf = [0u16; 1024];
        link.GetPath(&mut buf, std::ptr::null_mut(), 0).ok()?;
        let p = path_from_wide(&buf);
        (!p.as_os_str().is_empty()).then_some(p)
    }
}

pub fn properties(hwnd: HWND, paths: &[PathBuf]) {
    unsafe {
        if paths.len() == 1 {
            let _ = SHObjectProperties(
                Some(hwnd),
                SHOP_FILEPATH,
                PCWSTR(wide(&paths[0]).as_ptr()),
                PCWSTR::null(),
            );
        } else if let Ok(arr) = item_array(paths) {
            if let Ok(data) = arr.BindToHandler::<_, IDataObject>(None, &BHID_DataObject) {
                let _ = SHMultiFileProperties(&data, 0);
            }
        }
    }
}

// ---- context menus ------------------------------------------------------------------------------

thread_local! {
    /// The shell menu being tracked; owner-drawn submenus (Open with, Send to, New) need their
    /// messages forwarded to it.
    static ACTIVE_MENU: RefCell<Option<IContextMenu>> = const { RefCell::new(None) };
}

/// Forwards menu messages to an active shell context menu. Returns Some if it handled the message.
pub fn forward_menu_msg(msg: u32, wparam: WPARAM, lparam: LPARAM) -> Option<LRESULT> {
    if !matches!(
        msg,
        WM_INITMENUPOPUP | WM_DRAWITEM | WM_MEASUREITEM | WM_MENUCHAR | WM_MENUSELECT
    ) {
        return None;
    }
    ACTIVE_MENU.with(|m| {
        let m = m.borrow();
        let cm = m.as_ref()?;
        unsafe {
            if let Ok(cm3) = cm.cast::<IContextMenu3>() {
                let mut result = LRESULT(0);
                if cm3.HandleMenuMsg2(msg, wparam, lparam, Some(&mut result)).is_ok() {
                    return Some(result);
                }
            } else if let Ok(cm2) = cm.cast::<IContextMenu2>() {
                if cm2.HandleMenuMsg(msg, wparam, lparam).is_ok() {
                    return Some(LRESULT(0));
                }
            }
        }
        None
    })
}

fn verb_of(cm: &IContextMenu, id: u32) -> String {
    let mut buf = [0u8; 128];
    unsafe {
        if cm
            .GetCommandString(id as usize, GCS_VERBA, None, PSTR(buf.as_mut_ptr()), buf.len() as u32)
            .is_ok()
        {
            let len = buf.iter().position(|&c| c == 0).unwrap_or(0);
            return String::from_utf8_lossy(&buf[..len]).to_lowercase();
        }
    }
    String::new()
}

fn invoke(cm: &IContextMenu, hwnd: HWND, id: u32, pt: POINT, dir: Option<&Path>) {
    let dirw = dir.map(wide);
    let dir_a: Option<Vec<u8>> = dir.map(|d| {
        let mut v = d.to_string_lossy().into_owned().into_bytes();
        v.push(0);
        v
    });
    // CMIC_MASK_UNICODE has the same value as SEE_MASK_UNICODE.
    let mut mask = SEE_MASK_UNICODE | CMIC_MASK_PTINVOKE;
    unsafe {
        use windows::Win32::UI::Input::KeyboardAndMouse::{GetKeyState, VK_CONTROL, VK_SHIFT};
        if GetKeyState(VK_CONTROL.0 as i32) < 0 {
            mask |= CMIC_MASK_CONTROL_DOWN;
        }
        if GetKeyState(VK_SHIFT.0 as i32) < 0 {
            mask |= CMIC_MASK_SHIFT_DOWN;
        }
    }
    let info = CMINVOKECOMMANDINFOEX {
        cbSize: std::mem::size_of::<CMINVOKECOMMANDINFOEX>() as u32,
        fMask: mask,
        hwnd,
        lpVerb: PCSTR(id as usize as *const u8),
        lpVerbW: PCWSTR(id as usize as *const u16),
        lpDirectory: dir_a
            .as_ref()
            .map(|d| PCSTR(d.as_ptr()))
            .unwrap_or(PCSTR::null()),
        lpDirectoryW: dirw
            .as_ref()
            .map(|d| PCWSTR(d.as_ptr()))
            .unwrap_or(PCWSTR::null()),
        nShow: SW_SHOWNORMAL.0,
        ptInvoke: pt,
        ..Default::default()
    };
    unsafe {
        let _ = cm.InvokeCommand(&info as *const _ as *const CMINVOKECOMMANDINFO);
    }
}

fn track(menu: HMENU, cm: Option<&IContextMenu>, hwnd: HWND, pt: POINT) -> u32 {
    ACTIVE_MENU.with(|m| *m.borrow_mut() = cm.cloned());
    let id = unsafe {
        TrackPopupMenuEx(
            menu,
            (TPM_RETURNCMD | TPM_RIGHTBUTTON).0,
            pt.x,
            pt.y,
            hwnd,
            None,
        )
    };
    ACTIVE_MENU.with(|m| *m.borrow_mut() = None);
    id.0 as u32
}

pub enum MenuResult {
    None,
    /// The user picked the shell's Rename verb; we rename inline instead.
    Rename,
    /// Something likely changed (delete, paste, new...).
    Changed,
    /// One of our own items (ids >= OWN_BASE) was chosen.
    Own(u32),
}

pub const OWN_BASE: u32 = 0x8000;
const SHELL_LAST: u32 = 0x7FFF;

/// The full classic shell context menu for items (Open with, Send to, 7-Zip, Properties...).
/// `own` items are prepended (id, label; label "-" = separator).
pub fn item_menu(
    hwnd: HWND,
    paths: &[PathBuf],
    pt: POINT,
    extended: bool,
    own: &[(u32, &str)],
) -> MenuResult {
    let Ok(arr) = item_array(paths) else {
        return MenuResult::None;
    };
    unsafe {
        let Ok(cm) = arr.BindToHandler::<_, IContextMenu>(None, &BHID_SFUIObject) else {
            return MenuResult::None;
        };
        let Ok(menu) = CreatePopupMenu() else {
            return MenuResult::None;
        };
        let mut flags = CMF_NORMAL | CMF_EXPLORE | CMF_CANRENAME;
        if extended {
            flags |= CMF_EXTENDEDVERBS;
        }
        let _ = cm.QueryContextMenu(menu, 0, 1, SHELL_LAST, flags);
        append_own(menu, own, true);
        let id = track(menu, Some(&cm), hwnd, pt);
        let result = if id >= OWN_BASE {
            MenuResult::Own(id)
        } else if id >= 1 {
            let verb = verb_of(&cm, id - 1);
            if verb == "rename" {
                MenuResult::Rename
            } else {
                let dir = paths.first().and_then(|p| p.parent());
                invoke(&cm, hwnd, id - 1, pt, dir);
                MenuResult::Changed
            }
        } else {
            MenuResult::None
        };
        let _ = DestroyMenu(menu);
        result
    }
}

fn append_own(menu: HMENU, own: &[(u32, &str)], at_end: bool) {
    unsafe {
        if at_end && !own.is_empty() && GetMenuItemCount(Some(menu)) > 0 {
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
        }
        for (id, label) in own {
            if *label == "-" {
                let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
            } else {
                let l = wide(label);
                let _ = AppendMenuW(menu, MF_STRING, *id as usize, PCWSTR(l.as_ptr()));
            }
        }
    }
}

/// Our own submenu: (label, items (id, label, checked)).
pub type Submenu<'a> = (&'a str, Vec<(u32, String, bool)>);

/// Folder background menu: our View/Sort submenus and items, then the shell's background menu
/// (New ▸ with every registered type, Paste, Open in Terminal, Properties...).
pub fn background_menu(
    hwnd: HWND,
    folder: Option<&Path>,
    pt: POINT,
    subs: &[Submenu],
    own: &[(u32, &str)],
) -> MenuResult {
    unsafe {
        let Ok(menu) = CreatePopupMenu() else {
            return MenuResult::None;
        };
        for (label, items) in subs {
            if let Ok(sub) = CreatePopupMenu() {
                for (id, text, checked) in items {
                    if text == "-" {
                        let _ = AppendMenuW(sub, MF_SEPARATOR, 0, None);
                        continue;
                    }
                    let t = wide(text);
                    let flags = if *checked { MF_STRING | MF_CHECKED } else { MF_STRING };
                    let _ = AppendMenuW(sub, flags, *id as usize, PCWSTR(t.as_ptr()));
                }
                let l = wide(label);
                let _ = AppendMenuW(menu, MF_POPUP, sub.0 as usize, PCWSTR(l.as_ptr()));
            }
        }
        append_own(menu, own, false);
        let cm = folder.and_then(|f| background_cm(hwnd, f));
        if let Some(cm) = &cm {
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
            let n = GetMenuItemCount(Some(menu)).max(0) as u32;
            let _ = cm.QueryContextMenu(menu, n, 1, SHELL_LAST, CMF_NORMAL | CMF_EXPLORE);
        }
        let id = track(menu, cm.as_ref(), hwnd, pt);
        let result = if id >= OWN_BASE {
            MenuResult::Own(id)
        } else if id >= 1 {
            if let Some(cm) = &cm {
                invoke(cm, hwnd, id - 1, pt, folder);
            }
            MenuResult::Changed
        } else {
            MenuResult::None
        };
        let _ = DestroyMenu(menu);
        result
    }
}

fn background_cm(hwnd: HWND, folder: &Path) -> Option<IContextMenu> {
    unsafe {
        let it = item(folder).ok()?;
        let sf: IShellFolder = it.BindToHandler(None, &BHID_SFObject).ok()?;
        sf.CreateViewObject(hwnd).ok()
    }
}

/// A popup menu of (id, label, checked, enabled); returns the chosen id.
/// Labels: "-" is a separator, ">Title" opens a submenu, "<" closes it.
pub fn popup(hwnd: HWND, pt: POINT, items: &[(u32, String, bool, bool)]) -> Option<u32> {
    unsafe {
        let root = CreatePopupMenu().ok()?;
        let mut stack = vec![root];
        for (id, text, checked, enabled) in items {
            let menu = *stack.last().unwrap();
            if text == "-" {
                let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
                continue;
            }
            if let Some(title) = text.strip_prefix('>') {
                if let Ok(sub) = CreatePopupMenu() {
                    let t = wide(title);
                    let _ = AppendMenuW(menu, MF_POPUP, sub.0 as usize, PCWSTR(t.as_ptr()));
                    stack.push(sub);
                }
                continue;
            }
            if text == "<" {
                if stack.len() > 1 {
                    stack.pop();
                }
                continue;
            }
            let t = wide(text);
            let mut flags = MF_STRING;
            if *checked {
                flags |= MF_CHECKED;
            }
            if !*enabled {
                flags |= MF_GRAYED;
            }
            let _ = AppendMenuW(menu, flags, *id as usize, PCWSTR(t.as_ptr()));
        }
        let id = track(root, None, hwnd, pt);
        let _ = DestroyMenu(root);
        (id > 0).then_some(id)
    }
}

// ---- file operations ----------------------------------------------------------------------------

pub enum Op {
    Copy(Vec<PathBuf>, PathBuf),
    Move(Vec<PathBuf>, PathBuf),
    Delete(Vec<PathBuf>, bool),
    Rename(PathBuf, String),
}

/// Runs a file operation on its own STA thread so the window stays responsive; the shell shows its
/// usual progress, conflict, and elevation dialogs owned by our window.
pub fn run_op(hwnd: HWND, op: Op, notifier: Notifier) {
    let h = hwnd.0 as isize;
    std::thread::spawn(move || unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let hwnd = HWND(h as *mut _);
        let mut folders: Vec<PathBuf> = Vec::new();
        let r = (|| -> Result<()> {
            let fo: IFileOperation = CoCreateInstance(&FileOperation, None, CLSCTX_INPROC_SERVER)?;
            fo.SetOwnerWindow(hwnd)?;
            let mut flags = FOF_ALLOWUNDO | FOF_NOCONFIRMMKDIR;
            match &op {
                Op::Copy(src, dst) | Op::Move(src, dst) => {
                    let same = src.iter().all(|p| p.parent() == Some(dst.as_path()));
                    if same {
                        // Pasting into the source folder makes "name - Copy", like Explorer.
                        flags |= FOF_RENAMEONCOLLISION;
                    }
                    fo.SetOperationFlags(flags)?;
                    let arr = item_array(src)?;
                    let dest = item(dst)?;
                    if matches!(op, Op::Copy(..)) {
                        fo.CopyItems(&arr, &dest)?;
                    } else {
                        fo.MoveItems(&arr, &dest)?;
                        folders.extend(src.iter().filter_map(|p| p.parent().map(Path::to_path_buf)));
                    }
                    folders.push(dst.clone());
                }
                Op::Delete(paths, permanent) => {
                    if !permanent {
                        flags |= FOFX_RECYCLEONDELETE | FOF_NOCONFIRMATION;
                    } else {
                        flags &= !FOF_ALLOWUNDO;
                        flags |= FOF_WANTNUKEWARNING;
                    }
                    fo.SetOperationFlags(flags)?;
                    fo.DeleteItems(&item_array(paths)?)?;
                    folders.extend(paths.iter().filter_map(|p| p.parent().map(Path::to_path_buf)));
                }
                Op::Rename(path, name) => {
                    fo.SetOperationFlags(flags)?;
                    fo.RenameItem(&item(path)?, PCWSTR(wide(name).as_ptr()), None)?;
                    folders.extend(path.parent().map(Path::to_path_buf));
                }
            }
            fo.PerformOperations()?;
            Ok(())
        })();
        let _ = r;
        notifier.send(Msg::OpDone { folders });
    });
}

pub fn new_folder(dir: &Path) -> std::io::Result<String> {
    let mut name = "New folder".to_string();
    let mut n = 2;
    while dir.join(&name).exists() {
        name = format!("New folder ({n})");
        n += 1;
    }
    std::fs::create_dir(dir.join(&name))?;
    Ok(name)
}

// ---- clipboard ----------------------------------------------------------------------------------

fn drop_effect_format() -> u32 {
    unsafe { RegisterClipboardFormatW(w!("Preferred DropEffect")) }
}

fn global_from(bytes: &[u8]) -> Option<HGLOBAL> {
    unsafe {
        let h = GlobalAlloc(GMEM_MOVEABLE, bytes.len()).ok()?;
        let p = GlobalLock(h) as *mut u8;
        if p.is_null() {
            let _ = GlobalFree(Some(h));
            return None;
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), p, bytes.len());
        let _ = GlobalUnlock(h);
        Some(h)
    }
}

fn open_clipboard(hwnd: HWND) -> bool {
    (0..8).any(|i| {
        if i > 0 {
            std::thread::sleep(std::time::Duration::from_millis(12));
        }
        unsafe { OpenClipboard(Some(hwnd)).is_ok() }
    })
}

/// Puts files on the clipboard (CF_HDROP + Preferred DropEffect), as Explorer's Copy/Cut do.
pub fn set_clipboard_files(hwnd: HWND, paths: &[PathBuf], cut: bool) -> bool {
    // DROPFILES header (20 bytes) followed by double-null-terminated UTF-16 paths.
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&20u32.to_le_bytes()); // pFiles
    bytes.extend_from_slice(&0i32.to_le_bytes()); // pt.x
    bytes.extend_from_slice(&0i32.to_le_bytes()); // pt.y
    bytes.extend_from_slice(&0u32.to_le_bytes()); // fNC
    bytes.extend_from_slice(&1u32.to_le_bytes()); // fWide
    for p in paths {
        for u in wide(p) {
            bytes.extend_from_slice(&u.to_le_bytes());
        }
    }
    bytes.extend_from_slice(&0u16.to_le_bytes());
    let effect = if cut { DROPEFFECT_MOVE.0 } else { DROPEFFECT_COPY.0 };
    let (Some(hdrop), Some(heffect)) = (global_from(&bytes), global_from(&effect.to_le_bytes()))
    else {
        return false;
    };
    if !open_clipboard(hwnd) {
        unsafe {
            let _ = GlobalFree(Some(hdrop));
            let _ = GlobalFree(Some(heffect));
        }
        return false;
    }
    unsafe {
        let _ = EmptyClipboard();
        let ok = SetClipboardData(CF_HDROP.0 as u32, Some(HANDLE(hdrop.0))).is_ok();
        if !ok {
            let _ = GlobalFree(Some(hdrop));
        }
        if SetClipboardData(drop_effect_format(), Some(HANDLE(heffect.0))).is_err() {
            let _ = GlobalFree(Some(heffect));
        }
        let _ = CloseClipboard();
        ok
    }
}

pub fn clipboard_has_files() -> bool {
    unsafe { IsClipboardFormatAvailable(CF_HDROP.0 as u32).is_ok() }
}

/// Files on the clipboard and whether they were cut.
pub fn get_clipboard_files(hwnd: HWND) -> Option<(Vec<PathBuf>, bool)> {
    if !clipboard_has_files() || !open_clipboard(hwnd) {
        return None;
    }
    let result = unsafe {
        (|| {
            let h = GetClipboardData(CF_HDROP.0 as u32).ok()?;
            let hdrop = HDROP(h.0);
            let n = DragQueryFileW(hdrop, u32::MAX, None);
            let mut files = Vec::with_capacity(n as usize);
            for i in 0..n {
                let len = DragQueryFileW(hdrop, i, None);
                let mut buf = vec![0u16; len as usize + 1];
                DragQueryFileW(hdrop, i, Some(&mut buf));
                files.push(PathBuf::from(OsString::from_wide(&buf[..len as usize])));
            }
            let mut cut = false;
            if let Ok(he) = GetClipboardData(drop_effect_format()) {
                let p = GlobalLock(HGLOBAL(he.0)) as *const u32;
                if !p.is_null() {
                    cut = *p & DROPEFFECT_MOVE.0 != 0;
                    let _ = GlobalUnlock(HGLOBAL(he.0));
                }
            }
            Some((files, cut))
        })()
    };
    unsafe {
        let _ = CloseClipboard();
    }
    result
}

pub fn clear_clipboard(hwnd: HWND) {
    if open_clipboard(hwnd) {
        unsafe {
            let _ = EmptyClipboard();
            let _ = CloseClipboard();
        }
    }
}

// ---- drag & drop --------------------------------------------------------------------------------

/// Starts a shell drag of the given items (drag images, all drop targets, any app). Blocks until
/// the drop finishes; call with no UI state borrowed.
pub fn drag(hwnd: HWND, paths: &[PathBuf]) -> Option<DROPEFFECT> {
    unsafe {
        let arr = item_array(paths).ok()?;
        let data: IDataObject = arr.BindToHandler(None, &BHID_DataObject).ok()?;
        let effect = SHDoDragDrop(
            Some(hwnd),
            &data,
            None,
            DROPEFFECT_COPY | DROPEFFECT_MOVE | DROPEFFECT_LINK,
        )
        .ok()?;
        Some(effect)
    }
}

/// What the window under a drag offers: the folder (or item) to drop onto at a screen point.
pub trait DropHost {
    fn target_at(&self, pt: POINT) -> Option<PathBuf>;
    fn set_drop_highlight(&self, target: Option<&Path>);
    fn dropped(&self);
}

/// Our IDropTarget: forwards everything to the shell's drop target for whatever folder (or file,
/// e.g. an .exe or .zip) is under the cursor, so drops behave exactly like in Explorer.
#[implement(IDropTarget)]
pub struct DropTarget {
    hwnd: HWND,
    host: Box<dyn DropHost>,
    helper: Option<IDropTargetHelper>,
    state: RefCell<DropState>,
}

#[derive(Default)]
struct DropState {
    data: Option<IDataObject>,
    path: Option<PathBuf>,
    target: Option<IDropTarget>,
}

impl DropTarget {
    pub fn new(hwnd: HWND, host: Box<dyn DropHost>) -> DropTarget {
        let helper =
            unsafe { CoCreateInstance(&CLSID_DragDropHelper, None, CLSCTX_INPROC_SERVER).ok() };
        DropTarget {
            hwnd,
            host,
            helper,
            state: RefCell::new(DropState::default()),
        }
    }
}

fn shell_drop_target(hwnd: HWND, path: &Path) -> Option<IDropTarget> {
    unsafe {
        let it = item(path).ok()?;
        let _ = hwnd;
        it.BindToHandler(None, &BHID_SFUIObject).ok()
    }
}

impl DropTarget_Impl {
    /// Switches the delegate when the cursor moves onto a different folder.
    fn retarget(&self, keys: MODIFIERKEYS_FLAGS, pt: &POINTL, effect: *mut DROPEFFECT) {
        let p = POINT { x: pt.x, y: pt.y };
        let path = self.host.target_at(p);
        let mut st = self.state.borrow_mut();
        if st.path != path {
            if let Some(t) = st.target.take() {
                unsafe {
                    let _ = t.DragLeave();
                }
            }
            st.path = path.clone();
            if let (Some(path), Some(data)) = (&path, st.data.clone()) {
                if let Some(t) = shell_drop_target(self.hwnd, path) {
                    let mut e = unsafe { *effect };
                    unsafe {
                        let _ = t.DragEnter(&data, keys, *pt, &mut e);
                    }
                    st.target = Some(t);
                }
            }
            self.host.set_drop_highlight(path.as_deref());
        }
        unsafe {
            if let Some(t) = &st.target {
                let _ = t.DragOver(keys, *pt, effect);
            } else {
                *effect = DROPEFFECT_NONE;
            }
        }
    }
}

impl IDropTarget_Impl for DropTarget_Impl {
    fn DragEnter(
        &self,
        pdataobj: Ref<'_, IDataObject>,
        grfkeystate: MODIFIERKEYS_FLAGS,
        pt: &POINTL,
        pdweffect: *mut DROPEFFECT,
    ) -> Result<()> {
        let data = pdataobj.ok()?.clone();
        {
            let mut st = self.state.borrow_mut();
            *st = DropState {
                data: Some(data.clone()),
                ..Default::default()
            };
        }
        let allowed = unsafe { *pdweffect };
        self.retarget(grfkeystate, pt, pdweffect);
        if let Some(h) = &self.helper {
            let mut p = POINT { x: pt.x, y: pt.y };
            unsafe {
                let _ = h.DragEnter(self.hwnd, &data, &mut p, *pdweffect);
            }
        }
        let _ = allowed;
        Ok(())
    }

    fn DragOver(
        &self,
        grfkeystate: MODIFIERKEYS_FLAGS,
        pt: &POINTL,
        pdweffect: *mut DROPEFFECT,
    ) -> Result<()> {
        self.retarget(grfkeystate, pt, pdweffect);
        if let Some(h) = &self.helper {
            let mut p = POINT { x: pt.x, y: pt.y };
            unsafe {
                let _ = h.DragOver(&mut p, *pdweffect);
            }
        }
        Ok(())
    }

    fn DragLeave(&self) -> Result<()> {
        let mut st = self.state.borrow_mut();
        if let Some(t) = st.target.take() {
            unsafe {
                let _ = t.DragLeave();
            }
        }
        *st = DropState::default();
        drop(st);
        self.host.set_drop_highlight(None);
        if let Some(h) = &self.helper {
            unsafe {
                let _ = h.DragLeave();
            }
        }
        Ok(())
    }

    fn Drop(
        &self,
        pdataobj: Ref<'_, IDataObject>,
        grfkeystate: MODIFIERKEYS_FLAGS,
        pt: &POINTL,
        pdweffect: *mut DROPEFFECT,
    ) -> Result<()> {
        self.retarget(grfkeystate, pt, pdweffect);
        let target = self.state.borrow_mut().target.take();
        if let Some(h) = &self.helper {
            if let Ok(data) = pdataobj.ok() {
                let mut p = POINT { x: pt.x, y: pt.y };
                unsafe {
                    let _ = h.Drop(data, &mut p, *pdweffect);
                }
            }
        }
        match (target, pdataobj.ok()) {
            (Some(t), Ok(data)) => unsafe {
                let _ = t.Drop(data, grfkeystate, *pt, pdweffect);
            },
            _ => unsafe { *pdweffect = DROPEFFECT_NONE },
        }
        *self.state.borrow_mut() = DropState::default();
        self.host.set_drop_highlight(None);
        self.host.dropped();
        Ok(())
    }
}

pub fn register_drop_target(hwnd: HWND, host: Box<dyn DropHost>) {
    let target: IDropTarget = DropTarget::new(hwnd, host).into();
    unsafe {
        let _ = windows::Win32::System::Ole::RegisterDragDrop(hwnd, &target);
    }
}

#[allow(dead_code)]
fn _unused(_: BOOL) {}

// ---- Quick access -------------------------------------------------------------------------------

/// Windows' own Quick access pins (the list File Explorer shows), as (path, display name).
/// Only pinned filesystem folders are returned (not "frequent" ones). None if the shell
/// namespace isn't available, so callers can fall back to the known folders.
pub fn quick_access() -> Option<Vec<(PathBuf, String)>> {
    use windows::Win32::Foundation::PROPERTYKEY;
    use windows::Win32::UI::Shell::PropertiesSystem::PSGetPropertyKeyFromName;
    unsafe {
        let home: IShellItem = SHCreateItemFromParsingName(
            w!("shell:::{679f85cb-0220-4080-b29b-5540cc05aab6}"),
            None,
        )
        .ok()?;
        let items: IEnumShellItems = home.BindToHandler(None, &BHID_EnumItems).ok()?;
        let mut pinned_key = PROPERTYKEY::default();
        let has_key = PSGetPropertyKeyFromName(w!("System.Home.IsPinned"), &mut pinned_key).is_ok();
        let mut out = Vec::new();
        loop {
            let mut batch: [Option<IShellItem>; 1] = [None];
            let mut fetched = 0u32;
            if items.Next(&mut batch, Some(&mut fetched)).is_err() || fetched == 0 {
                break;
            }
            let Some(it) = batch[0].take() else { break };
            if has_key {
                if let Ok(i2) = it.cast::<IShellItem2>() {
                    if let Ok(pinned) = i2.GetBool(&pinned_key) {
                        if !pinned.as_bool() {
                            continue;
                        }
                    }
                }
            }
            let Ok(p) = it.GetDisplayName(SIGDN_FILESYSPATH) else { continue };
            let path = PathBuf::from(OsString::from_wide(p.as_wide()));
            CoTaskMemFree(Some(p.0 as _));
            let name = match it.GetDisplayName(SIGDN_NORMALDISPLAY) {
                Ok(n) => {
                    let s = String::from_utf16_lossy(n.as_wide());
                    CoTaskMemFree(Some(n.0 as _));
                    s
                }
                Err(_) => crate::ui::tab::folder_name(&path),
            };
            if path.is_dir() {
                out.push((path, name));
            }
        }
        Some(out)
    }
}

/// Invokes a canonical shell verb ("pintohome", "unpinfromhome", ...) on a path.
pub fn invoke_verb(hwnd: HWND, path: &Path, verb: &str) -> bool {
    let Ok(arr) = item_array(&[path.to_path_buf()]) else {
        return false;
    };
    unsafe {
        let Ok(cm) = arr.BindToHandler::<_, IContextMenu>(None, &BHID_SFUIObject) else {
            return false;
        };
        // Some handlers only respond to verbs after the menu has been built.
        if let Ok(menu) = CreatePopupMenu() {
            let _ = cm.QueryContextMenu(menu, 0, 1, SHELL_LAST, CMF_NORMAL);
            let _ = DestroyMenu(menu);
        }
        let mut verb_a = verb.as_bytes().to_vec();
        verb_a.push(0);
        let verb_w = wide(verb);
        let info = CMINVOKECOMMANDINFOEX {
            cbSize: std::mem::size_of::<CMINVOKECOMMANDINFOEX>() as u32,
            // CMIC_MASK_FLAG_NO_UI shares SEE_MASK_FLAG_NO_UI's value.
            fMask: SEE_MASK_UNICODE | SEE_MASK_FLAG_NO_UI,
            hwnd,
            lpVerb: PCSTR(verb_a.as_ptr()),
            lpVerbW: PCWSTR(verb_w.as_ptr()),
            nShow: SW_SHOWNORMAL.0,
            ..Default::default()
        };
        cm.InvokeCommand(&info as *const _ as *const CMINVOKECOMMANDINFO).is_ok()
    }
}
