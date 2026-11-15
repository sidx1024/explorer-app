//! Win32 host for the UI core: window, custom frame (Snap Layouts keep working), input translation,
//! painting through Direct2D, timers, and executing the UI's effects (menus, shell calls, ...).

use std::path::{Path, PathBuf};
use std::sync::mpsc::channel;
use std::sync::Arc;

use windows::core::{w, Result, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{AdjustWindowRectExForDpi, GetDpiForWindow, GetSystemMetricsForDpi};
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::config::Config;
use crate::icons;
use crate::msg::{Notifier, WM_APP_MSG};
use crate::platform::wide;
use crate::render::Gfx;
use crate::scene::{rgba, ImageKey, ImageStore, Scene};
use crate::shell::{self, MenuResult, Op, OWN_BASE};
use crate::theme;
use crate::ui::layout::TAB_H;
use crate::ui::{App, Button, Cursor, Effect, Hit, Input, Location, Mods};

const TIMER_THEME: usize = 50;
const WM_MOUSELEAVE: u32 = 0x02A3;
const WM_NCMOUSELEAVE: u32 = 0x02A2;

struct Host {
    hwnd: HWND,
    app: App,
    gfx: Gfx,
    scene: Scene,
    tracking: bool,
    high_surrogate: Option<u16>,
    /// Current window icon (index, small, big); the HICONs are ours to destroy.
    icon: (i32, HICON, HICON),
    /// Native shell preview handler shown in the preview pane.
    native_preview: Option<crate::preview::NativePreview>,
}

unsafe fn host<'a>(hwnd: HWND) -> Option<&'a mut Host> {
    (GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Host).as_mut()
}

fn lo(l: LPARAM) -> i32 {
    (l.0 & 0xffff) as i16 as i32
}

fn hi(l: LPARAM) -> i32 {
    ((l.0 >> 16) & 0xffff) as i16 as i32
}

fn mods() -> Mods {
    unsafe {
        Mods {
            ctrl: GetKeyState(VK_CONTROL.0 as i32) < 0,
            shift: GetKeyState(VK_SHIFT.0 as i32) < 0,
            alt: GetKeyState(VK_MENU.0 as i32) < 0,
        }
    }
}

pub fn run(start: Option<PathBuf>) -> Result<()> {
    let cfg = Config::load();
    let theme = theme::load(cfg.theme, cfg.mica);
    let (tx, rx) = channel();
    let notifier = Notifier::new(tx);
    let images = Arc::new(ImageStore::default());

    // A file argument opens its folder with the file selected.
    let (loc, select) = match start {
        Some(p) if p.is_dir() => (Location::Folder(p), None),
        Some(p) if p.exists() => (
            p.parent()
                .map(|d| Location::Folder(d.to_path_buf()))
                .unwrap_or(Location::ThisPc),
            p.file_name().map(|n| n.to_string_lossy().into_owned()),
        ),
        _ => (Location::ThisPc, None),
    };

    unsafe {
        let instance = GetModuleHandleW(None)?;
        let class = w!("ExplorerAppWindow");
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW | CS_DBLCLKS,
            lpfnWndProc: Some(wndproc),
            hInstance: instance.into(),
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            hbrBackground: HBRUSH(std::ptr::null_mut()),
            lpszClassName: class,
            hIcon: app_icon(instance.into(), SM_CXICON),
            hIconSm: app_icon(instance.into(), SM_CXSMICON),
            ..Default::default()
        };
        RegisterClassExW(&wc);

        let (x, y, w, h, maximized) = initial_placement(&cfg);
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            class,
            w!("Explorer App"),
            // Clip children so Direct2D never paints over a hosted preview handler.
            WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN,
            x,
            y,
            w,
            h,
            None,
            None,
            Some(instance.into()),
            None,
        )?;
        crate::platform::set_main_hwnd(hwnd);
        if cfg.index_drives {
            crate::index::start();
        }
        let _ = SetWindowPos(
            hwnd,
            None,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
        );
        theme::apply_to_window(hwnd, &theme);
        theme::apply_to_menus(&theme);

        let mut gfx = Gfx::new()?;
        gfx.set_text_scale(theme.text_scale);
        let mut app = App::new(cfg, theme, notifier.clone(), rx, images, loc);
        app.scale = GetDpiForWindow(hwnd) as f32 / 96.0;
        if let Some(name) = select {
            app.tab_mut().pending_select = Some((name, false));
        }
        let host = Box::new(Host {
            hwnd,
            app,
            gfx,
            scene: Scene::default(),
            tracking: false,
            high_surrogate: None,
            icon: (-1, HICON::default(), HICON::default()),
            native_preview: None,
        });
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, Box::into_raw(host) as isize);
        notifier.set_hwnd(hwnd);
        shell::register_drop_target(hwnd, Box::new(DropBridge { hwnd: hwnd.0 as isize }));
        flush(hwnd);

        let _ = ShowWindow(hwnd, if maximized { SW_SHOWMAXIMIZED } else { SW_SHOWNORMAL });
        let _ = UpdateWindow(hwnd);

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    Ok(())
}

unsafe fn app_icon(instance: HINSTANCE, metric: SYSTEM_METRICS_INDEX) -> HICON {
    let size = GetSystemMetrics(metric);
    LoadImageW(Some(instance), PCWSTR(1 as _), IMAGE_ICON, size, size, LR_DEFAULTCOLOR)
        .map(|h| HICON(h.0))
        .unwrap_or_default()
}

/// Saved placement if it's still on a connected monitor, else 80% of the work area, centered.
unsafe fn initial_placement(cfg: &Config) -> (i32, i32, i32, i32, bool) {
    if let Some([l, t, r, b, max]) = cfg.window {
        let rect = RECT { left: l + 40, top: t, right: r - 40, bottom: t + 40 };
        if r - l >= 400 && b - t >= 300 && !MonitorFromRect(&rect, MONITOR_DEFAULTTONULL).is_invalid() {
            return (l, t, r - l, b - t, max != 0);
        }
    }
    let mut work = RECT::default();
    let _ = SystemParametersInfoW(
        SPI_GETWORKAREA,
        0,
        Some(&mut work as *mut _ as _),
        SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
    );
    let (ww, wh) = (work.right - work.left, work.bottom - work.top);
    let (w, h) = (ww * 4 / 5, wh * 4 / 5);
    (work.left + (ww - w) / 2, work.top + (wh - h) / 2, w, h, false)
}

unsafe fn save_placement(hwnd: HWND, cfg: &mut Config) {
    let mut wp = WINDOWPLACEMENT {
        length: std::mem::size_of::<WINDOWPLACEMENT>() as u32,
        ..Default::default()
    };
    if GetWindowPlacement(hwnd, &mut wp).is_err() {
        return;
    }
    let maximized = wp.showCmd == SW_SHOWMAXIMIZED.0 as u32
        || (wp.showCmd == SW_SHOWMINIMIZED.0 as u32 && (wp.flags.0 & WPF_RESTORETOMAXIMIZED.0) != 0);
    // rcNormalPosition is in workspace coordinates; convert to screen coordinates.
    let mut r = wp.rcNormalPosition;
    let mut mi = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if GetMonitorInfoW(MonitorFromRect(&r, MONITOR_DEFAULTTONEAREST), &mut mi).as_bool() {
        let (dx, dy) = (mi.rcWork.left - mi.rcMonitor.left, mi.rcWork.top - mi.rcMonitor.top);
        r = RECT { left: r.left + dx, top: r.top + dy, right: r.right + dx, bottom: r.bottom + dy };
    }
    cfg.window = Some([r.left, r.top, r.right, r.bottom, maximized as i32]);
}

/// Provides bitmaps to the renderer: shell icons are extracted on first use, thumbnails come
/// from the workers through the image store.
fn provider(images: &ImageStore) -> impl Fn(&ImageKey) -> Option<Arc<crate::scene::Bitmap>> + '_ {
    move |key| {
        if let Some(b) = images.get(key) {
            return Some(b);
        }
        match *key {
            ImageKey::SysIcon { index, list } => {
                let bmp = icons::extract(index, list)?;
                images.put(*key, bmp);
                images.get(key)
            }
            ImageKey::Thumb(_) => None,
        }
    }
}

unsafe fn paint(h: &mut Host) {
    let Host { hwnd, app, gfx, scene, .. } = h;
    app.paint(scene, gfx);
    let clear = if app.theme.mica { rgba(0, 0, 0, 0.0) } else { app.theme.content };
    let images = app.images.clone();
    let p = provider(&images);
    if !gfx.draw(*hwnd, scene, app.scale, clear, &p) {
        // Device lost: repaint with a fresh target.
        let _ = InvalidateRect(Some(*hwnd), None, false);
    }
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        if let Some(r) = shell::forward_menu_msg(msg, wparam, lparam) {
            return r;
        }
        let r = handle(hwnd, msg, wparam, lparam);
        flush(hwnd);
        r
    }
}

fn to_input_point(h: &Host, l: LPARAM) -> (f32, f32) {
    (lo(l) as f32 / h.app.scale, hi(l) as f32 / h.app.scale)
}

unsafe fn handle(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // Remove the standard caption but keep the resize borders; the tab strip is the title bar.
    if msg == WM_NCCALCSIZE {
        let style = GetWindowLongPtrW(hwnd, GWL_STYLE) as u32;
        if style & WS_CAPTION.0 == WS_CAPTION.0 {
            let r = if wparam.0 != 0 {
                &mut (*(lparam.0 as *mut NCCALCSIZE_PARAMS)).rgrc[0]
            } else {
                &mut *(lparam.0 as *mut RECT)
            };
            let dpi = GetDpiForWindow(hwnd);
            let pad = GetSystemMetricsForDpi(SM_CXPADDEDBORDER, dpi);
            let fx = GetSystemMetricsForDpi(SM_CXFRAME, dpi) + pad;
            let fy = GetSystemMetricsForDpi(SM_CYFRAME, dpi) + pad;
            r.left += fx;
            r.right -= fx;
            r.bottom -= fy;
            if IsZoomed(hwnd).as_bool() {
                r.top += fy;
            }
            return LRESULT(0);
        }
    }
    let Some(h) = host(hwnd) else {
        return DefWindowProcW(hwnd, msg, wparam, lparam);
    };
    match msg {
        WM_PAINT => {
            let _ = ValidateRect(Some(hwnd), None);
            paint(h);
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_SIZE => {
            let (w, ht) = ((lparam.0 & 0xffff) as u32, ((lparam.0 >> 16) & 0xffff) as u32);
            h.gfx.resize(w, ht);
            h.app.size = (w as f32 / h.app.scale, ht as f32 / h.app.scale);
            h.app.maximized = IsZoomed(hwnd).as_bool();
            h.app.request_thumbnails();
            h.app.invalidate();
            LRESULT(0)
        }
        WM_APP_MSG => {
            h.app.drain();
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            if !h.tracking {
                let mut tme = TRACKMOUSEEVENT {
                    cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE,
                    hwndTrack: hwnd,
                    dwHoverTime: 0,
                };
                let _ = TrackMouseEvent(&mut tme);
                h.tracking = true;
            }
            if h.app.caption_hover != 0 {
                h.app.caption_hover = 0;
                h.app.invalidate();
            }
            let (x, y) = to_input_point(h, lparam);
            let before = (h.app.hover, h.app.cursor());
            h.app.input(Input::Move { x, y }, &h.gfx);
            // Only repaint when something visible changed (hover or a drag in progress).
            if before == (h.app.hover, h.app.cursor()) && matches!(h.app.drag, crate::ui::Drag::None) {
                h.app.dirty = false;
            }
            LRESULT(0)
        }
        WM_MOUSELEAVE => {
            h.tracking = false;
            h.app.input(Input::Leave, &h.gfx);
            LRESULT(0)
        }
        WM_LBUTTONDOWN | WM_LBUTTONDBLCLK | WM_RBUTTONDOWN | WM_MBUTTONDOWN | WM_XBUTTONDOWN => {
            let (x, y) = to_input_point(h, lparam);
            let button = match msg {
                WM_RBUTTONDOWN => Button::Right,
                WM_MBUTTONDOWN => Button::Middle,
                WM_XBUTTONDOWN => {
                    if ((wparam.0 >> 16) & 0xffff) as u16 == XBUTTON1 {
                        Button::Back
                    } else {
                        Button::Forward
                    }
                }
                _ => Button::Left,
            };
            let _ = SetFocus(Some(hwnd));
            h.app.input(
                Input::Down { x, y, button, double: msg == WM_LBUTTONDBLCLK, mods: mods() },
                &h.gfx,
            );
            LRESULT(if msg == WM_XBUTTONDOWN { 1 } else { 0 })
        }
        WM_LBUTTONUP | WM_RBUTTONUP | WM_MBUTTONUP => {
            let (x, y) = to_input_point(h, lparam);
            let button = match msg {
                WM_RBUTTONUP => Button::Right,
                WM_MBUTTONUP => Button::Middle,
                _ => Button::Left,
            };
            h.app.input(Input::Up { x, y, button, mods: mods() }, &h.gfx);
            LRESULT(0)
        }
        WM_CAPTURECHANGED => {
            if !matches!(h.app.drag, crate::ui::Drag::None) && HWND(lparam.0 as *mut _) != hwnd {
                h.app.input(Input::CaptureLost, &h.gfx);
            }
            LRESULT(0)
        }
        WM_MOUSEWHEEL | WM_MOUSEHWHEEL => {
            let mut pt = POINT { x: lo(lparam), y: hi(lparam) };
            let _ = ScreenToClient(hwnd, &mut pt);
            let delta = ((wparam.0 >> 16) & 0xffff) as i16 as f32 / 120.0;
            let (x, y) = (pt.x as f32 / h.app.scale, pt.y as f32 / h.app.scale);
            h.app.input(
                Input::Wheel { x, y, delta, horizontal: msg == WM_MOUSEHWHEEL, mods: mods() },
                &h.gfx,
            );
            LRESULT(0)
        }
        // Keyboard context menu (Menu key, Shift+F10) arrives as WM_CONTEXTMENU at (-1, -1).
        WM_CONTEXTMENU if lparam.0 as i32 == -1 => {
            h.app.input(Input::Key { vk: 0x5D, mods: mods() }, &h.gfx);
            LRESULT(0)
        }
        WM_KEYDOWN if wparam.0 == 0x5D => LRESULT(0),
        WM_KEYDOWN => {
            h.app.input(Input::Key { vk: wparam.0 as u32, mods: mods() }, &h.gfx);
            LRESULT(0)
        }
        WM_SYSKEYDOWN => {
            let vk = wparam.0 as u32;
            // Alt+Left/Right/Up/D/Enter are ours; everything else (Alt+F4, Alt+Space) is the system's.
            if matches!(vk, 0x25 | 0x26 | 0x27 | 0x44 | 0x0D | 0x50) {
                h.app.input(Input::Key { vk, mods: mods() }, &h.gfx);
                LRESULT(0)
            } else {
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
        }
        WM_SYSCHAR if matches!(wparam.0 as u8, b'd' | b'D' | b'\r') => LRESULT(0),
        WM_CHAR => {
            let u = wparam.0 as u16;
            let c = if (0xD800..0xDC00).contains(&u) {
                h.high_surrogate = Some(u);
                None
            } else if (0xDC00..0xE000).contains(&u) {
                h.high_surrogate
                    .take()
                    .and_then(|hs| char::decode_utf16([hs, u]).next())
                    .and_then(|r| r.ok())
            } else {
                char::from_u32(u as u32)
            };
            if let Some(c) = c.filter(|c| !c.is_control()) {
                h.app.input(Input::Char(c), &h.gfx);
            }
            LRESULT(0)
        }
        WM_TIMER => {
            if wparam.0 == TIMER_THEME {
                let _ = KillTimer(Some(hwnd), TIMER_THEME);
                apply_theme(h);
            } else {
                h.app.on_timer(wparam.0);
            }
            LRESULT(0)
        }
        WM_SETCURSOR if (lparam.0 & 0xffff) as u32 == HTCLIENT => {
            let id = match h.app.cursor() {
                Cursor::SizeWE => IDC_SIZEWE,
                Cursor::IBeam => IDC_IBEAM,
                Cursor::Arrow => IDC_ARROW,
            };
            if let Ok(c) = LoadCursorW(None, id) {
                SetCursor(Some(c));
            }
            LRESULT(1)
        }
        WM_NCHITTEST => {
            let d = DefWindowProcW(hwnd, msg, wparam, lparam);
            if d.0 != HTCLIENT as isize {
                return d;
            }
            LRESULT(nc_hit_test(h, lo(lparam), hi(lparam)) as isize)
        }
        WM_NCMOUSEMOVE => {
            let code = wparam.0 as u32;
            let code = if matches!(code, HTMINBUTTON | HTMAXBUTTON | HTCLOSE) { code } else { 0 };
            if code != h.app.caption_hover || h.app.hover != Hit::None {
                h.app.caption_hover = code;
                h.app.hover = Hit::None;
                h.app.invalidate();
            }
            let mut tme = TRACKMOUSEEVENT {
                cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                dwFlags: TME_LEAVE | TME_NONCLIENT,
                hwndTrack: hwnd,
                dwHoverTime: 0,
            };
            let _ = TrackMouseEvent(&mut tme);
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_NCMOUSELEAVE => {
            if h.app.caption_hover != 0 || h.app.caption_pressed != 0 {
                h.app.caption_hover = 0;
                h.app.caption_pressed = 0;
                h.app.invalidate();
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_NCLBUTTONDOWN | WM_NCLBUTTONDBLCLK
            if matches!(wparam.0 as u32, HTMINBUTTON | HTMAXBUTTON | HTCLOSE) =>
        {
            h.app.caption_pressed = wparam.0 as u32;
            h.app.invalidate();
            LRESULT(0)
        }
        WM_NCLBUTTONUP => {
            let code = wparam.0 as u32;
            let pressed = std::mem::take(&mut h.app.caption_pressed);
            if pressed != 0 {
                h.app.invalidate();
            }
            if !matches!(code, HTMINBUTTON | HTMAXBUTTON | HTCLOSE) {
                return DefWindowProcW(hwnd, msg, wparam, lparam);
            }
            if pressed == code {
                let cmd = match code {
                    HTMINBUTTON => SC_MINIMIZE,
                    HTMAXBUTTON if IsZoomed(hwnd).as_bool() => SC_RESTORE,
                    HTMAXBUTTON => SC_MAXIMIZE,
                    _ => SC_CLOSE,
                };
                let _ = PostMessageW(Some(hwnd), WM_SYSCOMMAND, WPARAM(cmd as usize), LPARAM(0));
            }
            LRESULT(0)
        }
        WM_ACTIVATE => {
            h.app.window_active = (wparam.0 & 0xffff) as u32 != WA_INACTIVE;
            if h.app.window_active {
                // Pins may have changed in File Explorer meanwhile.
                h.app.refresh_quick_access();
            }
            if !h.app.window_active {
                h.app.caption_hover = 0;
                h.app.caption_pressed = 0;
            }
            h.app.invalidate();
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_SETTINGCHANGE | WM_SYSCOLORCHANGE | WM_THEMECHANGED => {
            SetTimer(Some(hwnd), TIMER_THEME, 150, None);
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_DEVICECHANGE => {
            // DBT_DEVICEARRIVAL / DBT_DEVICEREMOVECOMPLETE: drives came or went.
            if matches!(wparam.0, 0x8000 | 0x8004) {
                h.app.nav.refresh_drives();
                for i in 0..h.app.tabs.len() {
                    if h.app.tabs[i].loc == Location::ThisPc {
                        h.app.load(i, Location::ThisPc, true);
                    }
                }
                h.app.invalidate();
            }
            LRESULT(1)
        }
        WM_DPICHANGED => {
            h.app.scale = ((wparam.0 >> 16) & 0xffff) as f32 / 96.0;
            let r = &*(lparam.0 as *const RECT);
            let _ = SetWindowPos(
                hwnd,
                None,
                r.left,
                r.top,
                r.right - r.left,
                r.bottom - r.top,
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
            h.app.invalidate();
            LRESULT(0)
        }
        WM_GETMINMAXINFO => {
            let mmi = &mut *(lparam.0 as *mut MINMAXINFO);
            let s = h.app.scale;
            let mut r = RECT { left: 0, top: 0, right: (560.0 * s) as i32, bottom: (360.0 * s) as i32 };
            let style = WINDOW_STYLE(GetWindowLongPtrW(hwnd, GWL_STYLE) as u32);
            let _ = AdjustWindowRectExForDpi(&mut r, style, false, WINDOW_EX_STYLE::default(), (s * 96.0) as u32);
            mmi.ptMinTrackSize = POINT { x: r.right - r.left, y: r.bottom - r.top };
            LRESULT(0)
        }
        WM_DESTROY => {
            let _ = windows::Win32::System::Ole::RevokeDragDrop(hwnd);
            save_placement(hwnd, &mut h.app.cfg);
            h.app.cfg.save();
            let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Host;
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            let host = Box::from_raw(ptr);
            for t in host.app.tabs {
                t.close();
            }
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

unsafe fn nc_hit_test(h: &Host, sx: i32, sy: i32) -> u32 {
    let mut pt = POINT { x: sx, y: sy };
    let _ = ScreenToClient(h.hwnd, &mut pt);
    let dpi = GetDpiForWindow(h.hwnd);
    let frame = GetSystemMetricsForDpi(SM_CYFRAME, dpi) + GetSystemMetricsForDpi(SM_CXPADDEDBORDER, dpi);
    let mut rc = RECT::default();
    let _ = GetClientRect(h.hwnd, &mut rc);
    if !IsZoomed(h.hwnd).as_bool() && pt.y < frame {
        return if pt.x < frame * 2 {
            HTTOPLEFT
        } else if pt.x > rc.right - frame * 2 {
            HTTOPRIGHT
        } else {
            HTTOP
        };
    }
    let (x, y) = (pt.x as f32 / h.app.scale, pt.y as f32 / h.app.scale);
    if y >= TAB_H {
        return HTCLIENT;
    }
    match h.app.hit_test(x, y) {
        // Returning HTMAXBUTTON is what makes Windows 11 show Snap Layouts on hover.
        Hit::Caption(code) => code,
        Hit::Tab(_) | Hit::TabClose(_) | Hit::NewTab => HTCLIENT,
        _ => HTCAPTION,
    }
}

unsafe fn apply_theme(h: &mut Host) {
    // Preview handlers take colors at creation: recreate with the new theme.
    h.native_preview = None;
    h.app.preview.native_shown = None;
    let th = theme::load(h.app.cfg.theme, h.app.cfg.mica);
    theme::apply_to_window(h.hwnd, &th);
    theme::apply_to_menus(&th);
    h.gfx.set_text_scale(th.text_scale);
    h.app.theme = th;
    h.app.invalidate();
}

fn client_to_screen(hwnd: HWND, scale: f32, x: f32, y: f32) -> POINT {
    let mut pt = POINT { x: (x * scale) as i32, y: (y * scale) as i32 };
    unsafe {
        let _ = ClientToScreen(hwnd, &mut pt);
    }
    pt
}

fn spawn_self(arg: Option<&Path>) {
    if let Ok(exe) = std::env::current_exe() {
        let mut cmd = std::process::Command::new(exe);
        if let Some(a) = arg {
            cmd.arg(a);
        }
        let _ = cmd.spawn();
    }
}

/// Runs effects with no `&mut Host` held across calls that pump messages (menus, drag & drop,
/// shell dialogs), since those re-enter the window procedure.
unsafe fn flush(hwnd: HWND) {
    loop {
        let Some(h) = host(hwnd) else { return };
        let effects = h.app.take_effects();
        if effects.is_empty() {
            if h.app.dirty {
                h.app.dirty = false;
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
            return;
        }
        for e in effects {
            run_effect(hwnd, e);
        }
    }
}

unsafe fn run_effect(hwnd: HWND, e: Effect) {
    let Some(h) = host(hwnd) else { return };
    let scale = h.app.scale;
    match e {
        Effect::Capture(true) => {
            SetCapture(hwnd);
        }
        Effect::Capture(false) => {
            if GetCapture() == hwnd {
                let _ = ReleaseCapture();
            }
        }
        Effect::Menu { kind, x, y, items } => {
            let pt = client_to_screen(hwnd, scale, x, y);
            if let Some(id) = shell::popup(hwnd, pt, &items) {
                if let Some(h) = host(hwnd) {
                    h.app.menu_chosen(Some(kind), id);
                }
            }
        }
        Effect::ItemMenu { paths, x, y, extended } => {
            let pt = client_to_screen(hwnd, scale, x, y);
            let all_dirs = paths.iter().all(|p| p.is_dir());
            let mut own: Vec<(u32, &str)> = Vec::new();
            use crate::ui::input_ids::*;
            if all_dirs {
                own.push((OWN_BASE + M_OPEN_TAB, "Open in new tab"));
                own.push((OWN_BASE + M_OPEN_WINDOW, "Open in new window"));
            }
            let selected = h.app.tab().selected_paths() == paths;
            match shell::item_menu(hwnd, &paths, pt, extended, &own) {
                MenuResult::Rename if selected => {
                    if let Some(h) = host(hwnd) {
                        h.app.begin_rename();
                    }
                }
                MenuResult::Own(id) => {
                    if let Some(h) = host(hwnd) {
                        h.app.menu_chosen(None, id - OWN_BASE);
                    }
                }
                // The shell menu has "Pin to / Unpin from Quick access" among its verbs.
                MenuResult::Changed => {
                    if let Some(h) = host(hwnd) {
                        h.app.refresh_quick_access();
                    }
                }
                _ => {}
            }
        }
        Effect::BackgroundMenu { folder, x, y } => {
            let pt = client_to_screen(hwnd, scale, x, y);
            let view = h.app.view_items(OWN_BASE);
            let sort = h.app.sort_items(OWN_BASE);
            use crate::ui::input_ids::*;
            let own = [(OWN_BASE + M_REFRESH, "Refresh")];
            let subs: Vec<shell::Submenu> = vec![
                ("View", view.into_iter().map(|(i, l, c, _)| (i, l, c)).collect()),
                ("Sort by", sort.into_iter().map(|(i, l, c, _)| (i, l, c)).collect()),
            ];
            if let MenuResult::Own(id) = shell::background_menu(hwnd, folder.as_deref(), pt, &subs, &own) {
                if let Some(h) = host(hwnd) {
                    h.app.menu_chosen(None, id - OWN_BASE);
                }
            }
        }
        Effect::Open(p) => shell::open(hwnd, &p),
        Effect::Properties(paths) => shell::properties(hwnd, &paths),
        Effect::FileOp(op) => shell::run_op(hwnd, op, h.app.notifier.clone()),
        Effect::SetClipboard { paths, cut } => {
            shell::set_clipboard_files(hwnd, &paths, cut);
        }
        Effect::Paste(dir) => {
            if let Some((files, cut)) = shell::get_clipboard_files(hwnd) {
                let notifier = h.app.notifier.clone();
                if cut {
                    shell::clear_clipboard(hwnd);
                    h.app.pasted();
                    shell::run_op(hwnd, Op::Move(files, dir), notifier);
                } else {
                    shell::run_op(hwnd, Op::Copy(files, dir), notifier);
                }
            }
        }
        Effect::Drag(paths) => {
            shell::drag(hwnd, &paths);
            if let Some(h) = host(hwnd) {
                h.app.drop_target = None;
                h.app.invalidate();
            }
        }
        Effect::Timer { id, ms } => {
            SetTimer(Some(hwnd), id, ms, None);
        }
        Effect::KillTimer(id) => {
            let _ = KillTimer(Some(hwnd), id);
        }
        Effect::Title(t) => {
            let _ = SetWindowTextW(hwnd, PCWSTR(wide(&t).as_ptr()));
        }
        Effect::WindowIcon(index) => set_window_icon(hwnd, h, index),
        Effect::NativePreview(want) => native_preview(hwnd, want),
        Effect::SaveConfig => h.app.cfg.save(),
        Effect::ApplyTheme => apply_theme(h),
        Effect::OpenTerminal(p) => shell::open_terminal(&p),
        Effect::Pin { paths, pin } => {
            let verb = if pin { "pintohome" } else { "unpinfromhome" };
            for p in &paths {
                shell::invoke_verb(hwnd, p, verb);
            }
            if let Some(h) = host(hwnd) {
                h.app.refresh_quick_access();
            }
        }
        Effect::NewWindow(p) => spawn_self(p.as_deref()),
        Effect::CloseWindow => {
            let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
        }
        Effect::Beep => {
            let _ = windows::Win32::System::Diagnostics::Debug::MessageBeep(MB_OK);
        }
    }
}

/// Lets the OLE drop target ask the UI what's under the cursor.
struct DropBridge {
    hwnd: isize,
}

impl shell::DropHost for DropBridge {
    fn target_at(&self, pt: POINT) -> Option<PathBuf> {
        unsafe {
            let hwnd = HWND(self.hwnd as *mut _);
            let h = host(hwnd)?;
            let mut p = pt;
            let _ = ScreenToClient(hwnd, &mut p);
            h.app.drop_target_at(p.x as f32 / h.app.scale, p.y as f32 / h.app.scale)
        }
    }

    fn set_drop_highlight(&self, target: Option<&Path>) {
        unsafe {
            let hwnd = HWND(self.hwnd as *mut _);
            if let Some(h) = host(hwnd) {
                // Highlight only items/nav rows, not the folder being viewed itself.
                let here = h.app.tab().loc.base().map(Path::to_path_buf);
                h.app.drop_target = target.map(Path::to_path_buf).filter(|t| Some(t) != here.as_ref());
                // Keep hover in sync with the drag position for visual feedback.
                let mut pt = POINT::default();
                let _ = GetCursorPos(&mut pt);
                let _ = ScreenToClient(hwnd, &mut pt);
                let (x, y) = (pt.x as f32 / h.app.scale, pt.y as f32 / h.app.scale);
                h.app.mouse = (x, y);
                h.app.hover = h.app.hit_test(x, y);
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
        }
    }

    fn dropped(&self) {}
}

/// Shows the current folder's icon on the taskbar button and in Alt+Tab, like File Explorer.
unsafe fn set_window_icon(hwnd: HWND, h: &mut Host, index: i32) {
    if h.icon.0 == index {
        return;
    }
    let (small, big) = match (
        icons::hicon(index, icons::LIST_SMALL),
        icons::hicon(index, icons::LIST_LARGE),
    ) {
        (Some(s), Some(b)) => (s, b),
        (s, b) => {
            for i in [s, b].into_iter().flatten() {
                let _ = DestroyIcon(i);
            }
            // Searches and unknown icons: back to the class (app) icon.
            (HICON::default(), HICON::default())
        }
    };
    SendMessageW(hwnd, WM_SETICON, Some(WPARAM(ICON_SMALL as usize)), Some(LPARAM(small.0 as isize)));
    SendMessageW(hwnd, WM_SETICON, Some(WPARAM(ICON_BIG as usize)), Some(LPARAM(big.0 as isize)));
    let (_, old_small, old_big) = std::mem::replace(&mut h.icon, (index, small, big));
    for i in [old_small, old_big] {
        if !i.is_invalid() {
            let _ = DestroyIcon(i);
        }
    }
}

/// Shows, moves or removes the shell preview handler for the preview pane.
unsafe fn native_preview(hwnd: HWND, want: Option<(PathBuf, windows::core::GUID, crate::scene::Rect)>) {
    let Some(h) = host(hwnd) else { return };
    let Some((path, clsid, r)) = want else {
        h.native_preview = None;
        return;
    };
    let s = h.app.scale;
    let rect = RECT {
        left: (r.l * s).round() as i32,
        top: (r.t * s).round() as i32,
        right: (r.r * s).round() as i32,
        bottom: (r.b * s).round() as i32,
    };
    if let Some(p) = h.native_preview.as_mut() {
        if p.path == path {
            p.set_rect(rect);
            return;
        }
    }
    // Unload the previous handler before starting the next one.
    h.native_preview = None;
    let (bg, fg) = (h.app.theme.content, h.app.theme.text);
    // Handlers may pump messages while loading; don't hold the Host borrow across it.
    let opened = crate::preview::NativePreview::open(hwnd, &clsid, &path, rect, bg, fg);
    let Some(h) = host(hwnd) else { return };
    // The selection may have moved on while the handler loaded.
    let still_wanted = h.app.preview.native_shown.as_ref().map(|w| &w.0) == Some(&path);
    match opened {
        Some(p) if still_wanted => h.native_preview = Some(p),
        Some(_) => {}
        None if still_wanted => h.app.native_preview_failed(),
        None => {}
    }
}
