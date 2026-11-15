//! Follows Windows personalization (light/dark, accent, high contrast, text size) with
//! File Explorer's Windows 11 palette, and applies Mica Alt / dark frame to the window.

use std::sync::OnceLock;

use windows::core::{w, BOOL, PCSTR};
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Dwm::{
    DwmExtendFrameIntoClientArea, DwmSetWindowAttribute, DWMWA_USE_IMMERSIVE_DARK_MODE,
    DWMWINDOWATTRIBUTE,
};
use windows::Win32::Graphics::Gdi::{
    GetSysColor, COLOR_BTNFACE, COLOR_GRAYTEXT, COLOR_HIGHLIGHT, COLOR_HIGHLIGHTTEXT,
    COLOR_WINDOW, COLOR_WINDOWTEXT, SYS_COLOR_INDEX,
};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::UI::Accessibility::{HCF_HIGHCONTRASTON, HIGHCONTRASTW};
use windows::Win32::UI::Controls::MARGINS;
use windows::Win32::UI::WindowsAndMessaging::{
    SystemParametersInfoW, SPI_GETHIGHCONTRAST, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS,
};
use windows::UI::ViewManagement::{UIColorType, UISettings};

use crate::scene::{rgba, Color};

pub const PREF_SYSTEM: u32 = 0;
pub const PREF_LIGHT: u32 = 1;
pub const PREF_DARK: u32 = 2;

#[derive(Clone)]
pub struct Theme {
    pub dark: bool,
    pub high_contrast: bool,
    /// Mica Alt backdrop behind the tab strip (Windows 11 22H2+).
    pub mica: bool,
    pub text_scale: f32,
    /// Window background behind the tab strip (transparent when Mica shows through).
    pub chrome: Color,
    /// Selected tab, address bar row and command bar.
    pub toolbar: Color,
    /// Navigation pane and file list.
    pub content: Color,
    pub text: Color,
    pub text_dim: Color,
    pub text_disabled: Color,
    pub divider: Color,
    pub hover: Color,
    pub pressed: Color,
    pub field: Color,
    pub field_border: Color,
    pub field_focus: Color,
    pub accent: Color,
    pub item_hover: Color,
    pub item_sel: Color,
    pub item_sel_hover: Color,
    pub item_sel_border: Color,
    pub item_sel_text: Color,
    pub band: Color,
    pub band_border: Color,
    pub scroll: Color,
    pub tab_inactive_hover: Color,
    pub usage: Color,
    pub usage_full: Color,
    pub usage_track: Color,
    pub usage_border: Color,
}

fn sys(i: SYS_COLOR_INDEX) -> Color {
    let c = unsafe { GetSysColor(i) };
    rgba(
        (c & 0xff) as u8,
        ((c >> 8) & 0xff) as u8,
        ((c >> 16) & 0xff) as u8,
        1.0,
    )
}

fn high_contrast_on() -> bool {
    let mut hc = HIGHCONTRASTW {
        cbSize: std::mem::size_of::<HIGHCONTRASTW>() as u32,
        ..Default::default()
    };
    unsafe {
        SystemParametersInfoW(
            SPI_GETHIGHCONTRAST,
            hc.cbSize,
            Some(&mut hc as *mut _ as _),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
        .is_ok()
            && (hc.dwFlags & HCF_HIGHCONTRASTON) == HCF_HIGHCONTRASTON
    }
}

pub fn mica_supported() -> bool {
    static B: OnceLock<bool> = OnceLock::new();
    *B.get_or_init(|| crate::platform::windows_build() >= 22621)
}

pub fn load(pref: u32, allow_mica: bool) -> Theme {
    let ui = UISettings::new().ok();
    let color = |t: UIColorType| {
        ui.as_ref()
            .and_then(|u| u.GetColorValue(t).ok())
            .map(|c| rgba(c.R, c.G, c.B, 1.0))
    };
    let dark = match pref {
        PREF_LIGHT => false,
        PREF_DARK => true,
        _ => color(UIColorType::Background)
            .map(|c| c.r < 0.5)
            .unwrap_or(false),
    };
    let text_scale = ui
        .as_ref()
        .and_then(|u| u.TextScaleFactor().ok())
        .unwrap_or(1.0) as f32;

    if high_contrast_on() {
        let (window, text, hl, hl_text) = (
            sys(COLOR_WINDOW),
            sys(COLOR_WINDOWTEXT),
            sys(COLOR_HIGHLIGHT),
            sys(COLOR_HIGHLIGHTTEXT),
        );
        return Theme {
            dark,
            high_contrast: true,
            mica: false,
            text_scale,
            chrome: window,
            toolbar: window,
            content: window,
            text,
            text_dim: text,
            text_disabled: sys(COLOR_GRAYTEXT),
            divider: text,
            hover: hl,
            pressed: hl,
            field: window,
            field_border: text,
            field_focus: hl,
            accent: hl,
            item_hover: sys(COLOR_BTNFACE),
            item_sel: hl,
            item_sel_hover: hl,
            item_sel_border: text,
            item_sel_text: hl_text,
            band: hl.with_alpha(0.3),
            band_border: hl,
            scroll: text,
            tab_inactive_hover: hl,
            usage: hl,
            usage_full: hl,
            usage_track: sys(COLOR_GRAYTEXT),
            usage_border: text,
        };
    }

    let mica = allow_mica && mica_supported();
    if dark {
        let accent = color(UIColorType::AccentLight2).unwrap_or(rgba(96, 205, 255, 1.0));
        Theme {
            dark,
            high_contrast: false,
            mica,
            text_scale,
            chrome: if mica { rgba(0, 0, 0, 0.0) } else { rgba(32, 32, 32, 1.0) },
            toolbar: rgba(43, 43, 43, 1.0),
            content: rgba(25, 25, 25, 1.0),
            text: rgba(255, 255, 255, 1.0),
            text_dim: rgba(255, 255, 255, 0.62),
            text_disabled: rgba(255, 255, 255, 0.36),
            divider: rgba(255, 255, 255, 0.08),
            hover: rgba(255, 255, 255, 0.06),
            pressed: rgba(255, 255, 255, 0.04),
            field: rgba(255, 255, 255, 0.06),
            field_border: rgba(255, 255, 255, 0.07),
            field_focus: accent,
            accent,
            item_hover: rgba(255, 255, 255, 0.07),
            item_sel: rgba(255, 255, 255, 0.13),
            item_sel_hover: rgba(255, 255, 255, 0.17),
            item_sel_border: rgba(255, 255, 255, 0.0),
            item_sel_text: rgba(255, 255, 255, 1.0),
            band: accent.with_alpha(0.25),
            band_border: accent,
            scroll: rgba(255, 255, 255, 0.55),
            tab_inactive_hover: rgba(255, 255, 255, 0.05),
            // Explorer uses the same bar colors in both themes.
            usage: rgba(38, 160, 218, 1.0),
            usage_full: rgba(218, 38, 38, 1.0),
            usage_track: rgba(64, 64, 64, 1.0),
            usage_border: rgba(96, 96, 96, 1.0),
        }
    } else {
        let accent = color(UIColorType::AccentDark1).unwrap_or(rgba(0, 95, 184, 1.0));
        Theme {
            dark,
            high_contrast: false,
            mica,
            text_scale,
            chrome: if mica { rgba(0, 0, 0, 0.0) } else { rgba(238, 238, 238, 1.0) },
            toolbar: rgba(249, 249, 249, 1.0),
            content: rgba(255, 255, 255, 1.0),
            text: rgba(0, 0, 0, 0.9),
            text_dim: rgba(0, 0, 0, 0.6),
            text_disabled: rgba(0, 0, 0, 0.36),
            divider: rgba(0, 0, 0, 0.08),
            hover: rgba(0, 0, 0, 0.04),
            pressed: rgba(0, 0, 0, 0.025),
            field: rgba(255, 255, 255, 1.0),
            field_border: rgba(0, 0, 0, 0.08),
            field_focus: accent,
            accent,
            item_hover: rgba(229, 243, 255, 1.0),
            item_sel: rgba(204, 232, 255, 1.0),
            item_sel_hover: rgba(189, 224, 252, 1.0),
            item_sel_border: rgba(153, 209, 255, 1.0),
            item_sel_text: rgba(0, 0, 0, 0.9),
            band: rgba(0, 120, 215, 0.2),
            band_border: rgba(0, 120, 215, 1.0),
            scroll: rgba(0, 0, 0, 0.45),
            tab_inactive_hover: rgba(0, 0, 0, 0.04),
            usage: rgba(38, 160, 218, 1.0),
            usage_full: rgba(218, 38, 38, 1.0),
            usage_track: rgba(230, 230, 230, 1.0),
            usage_border: rgba(188, 188, 188, 1.0),
        }
    }
}

const DWMWA_SYSTEMBACKDROP_TYPE: DWMWINDOWATTRIBUTE = DWMWINDOWATTRIBUTE(38);
const DWMSBT_NONE: u32 = 1;
const DWMSBT_TABBEDWINDOW: u32 = 4;

/// Dark/light frame, and the Mica Alt backdrop (the material File Explorer's tab strip uses).
pub fn apply_to_window(hwnd: HWND, theme: &Theme) {
    unsafe {
        let dark = BOOL((theme.dark && !theme.high_contrast) as i32);
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_USE_IMMERSIVE_DARK_MODE,
            &dark as *const _ as _,
            4,
        );
        if mica_supported() {
            let kind = if theme.mica {
                DWMSBT_TABBEDWINDOW
            } else {
                DWMSBT_NONE
            };
            let _ = DwmSetWindowAttribute(
                hwnd,
                DWMWA_SYSTEMBACKDROP_TYPE,
                &kind as *const _ as _,
                4,
            );
            // The backdrop shows through wherever the client area is transparent.
            let m = if theme.mica {
                MARGINS {
                    cxLeftWidth: -1,
                    cxRightWidth: -1,
                    cyTopHeight: -1,
                    cyBottomHeight: -1,
                }
            } else {
                MARGINS::default()
            };
            let _ = DwmExtendFrameIntoClientArea(hwnd, &m);
        }
    }
}

type SetPreferredAppMode = extern "system" fn(i32) -> i32;
type FlushMenuThemes = extern "system" fn();

fn menu_theme_fns() -> Option<(SetPreferredAppMode, FlushMenuThemes)> {
    static FNS: OnceLock<Option<(usize, usize)>> = OnceLock::new();
    let fns = FNS.get_or_init(|| unsafe {
        if crate::platform::windows_build() < 18362 {
            return None;
        }
        let uxtheme = LoadLibraryW(w!("uxtheme.dll")).ok()?;
        let set = GetProcAddress(uxtheme, PCSTR(135 as *const u8))?;
        let flush = GetProcAddress(uxtheme, PCSTR(136 as *const u8))?;
        Some((set as usize, flush as usize))
    });
    fns.map(|(set, flush)| unsafe {
        (
            std::mem::transmute::<usize, SetPreferredAppMode>(set),
            std::mem::transmute::<usize, FlushMenuThemes>(flush),
        )
    })
}

/// Make Win32 popup menus (including shell context menus) follow the theme. Undocumented uxtheme
/// ordinals, the same ones Windows' own Win32 apps use; silently does nothing where unavailable.
pub fn apply_to_menus(theme: &Theme) {
    let Some((set_mode, flush)) = menu_theme_fns() else {
        return;
    };
    set_mode(if theme.high_contrast {
        0
    } else if theme.dark {
        2
    } else {
        3
    });
    flush();
}
