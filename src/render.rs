//! Direct2D / DirectWrite backend for [`Scene`]. The render target runs at 96 DPI so one unit is one
//! physical pixel; the scene (in DIPs) is drawn under a scale transform.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

use windows::core::{w, Interface, Result, BOOL, PCWSTR};
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::UI::WindowsAndMessaging::GetClientRect;
use windows_numerics::Matrix3x2;

use crate::scene::{Align, Bitmap, Cmd, Color, Font, ImageKey, Measure, Rect, Scene};

struct Target {
    rt: ID2D1HwndRenderTarget,
    dc: Option<ID2D1DeviceContext>,
    brush: ID2D1SolidColorBrush,
}

pub struct Gfx {
    factory: ID2D1Factory1,
    dwrite: IDWriteFactory,
    target: Option<Target>,
    formats: RefCell<HashMap<(Font, Align, bool), IDWriteTextFormat>>,
    widths: RefCell<HashMap<(String, Font), f32>>,
    bitmaps: HashMap<ImageKey, (ID2D1Bitmap, u64)>,
    frame: u64,
    icon_family: PCWSTR,
    mono_family: PCWSTR,
    text_scale: f32,
}

fn d2c(c: Color) -> D2D1_COLOR_F {
    D2D1_COLOR_F {
        r: c.r,
        g: c.g,
        b: c.b,
        a: c.a,
    }
}

fn d2r(r: Rect, s: f32) -> D2D_RECT_F {
    D2D_RECT_F {
        left: r.l * s,
        top: r.t * s,
        right: r.r * s,
        bottom: r.b * s,
    }
}

fn utf16(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

/// Segoe Fluent Icons ships with Windows 11; Windows 10 has Segoe MDL2 Assets with the same code points.
fn icon_family(dwrite: &IDWriteFactory) -> PCWSTR {
    unsafe {
        let mut fonts = None;
        let (mut index, mut exists) = (0u32, BOOL(0));
        if dwrite.GetSystemFontCollection(&mut fonts, false).is_ok() {
            if let Some(fonts) = fonts {
                let _ = fonts.FindFamilyName(w!("Segoe Fluent Icons"), &mut index, &mut exists);
            }
        }
        if exists.as_bool() {
            w!("Segoe Fluent Icons")
        } else {
            w!("Segoe MDL2 Assets")
        }
    }
}

impl Gfx {
    pub fn new() -> Result<Self> {
        unsafe {
            let factory: ID2D1Factory1 =
                D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
            let dwrite: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
            let icon_family = icon_family(&dwrite);
            let mono_family = mono_family(&dwrite);
            Ok(Gfx {
                factory,
                dwrite,
                target: None,
                formats: RefCell::new(HashMap::new()),
                widths: RefCell::new(HashMap::new()),
                bitmaps: HashMap::new(),
                frame: 0,
                icon_family,
                mono_family,
                text_scale: 1.0,
            })
        }
    }

    pub fn set_text_scale(&mut self, s: f32) {
        let s = s.clamp(1.0, 2.25);
        if (s - self.text_scale).abs() > 0.001 {
            self.text_scale = s;
            self.formats.borrow_mut().clear();
            self.widths.borrow_mut().clear();
        }
    }

    fn format(&self, font: Font, align: Align, wrap: bool) -> Option<IDWriteTextFormat> {
        if let Some(f) = self.formats.borrow().get(&(font, align, wrap)) {
            return Some(f.clone());
        }
        let ts = self.text_scale;
        let (family, weight, size) = match font {
            Font::Ui => (w!("Segoe UI"), DWRITE_FONT_WEIGHT_NORMAL, 12.0 * ts),
            Font::UiSemibold => (w!("Segoe UI"), DWRITE_FONT_WEIGHT_SEMI_BOLD, 12.0 * ts),
            Font::Small => (w!("Segoe UI"), DWRITE_FONT_WEIGHT_NORMAL, 12.0),
            Font::Icon16 => (self.icon_family, DWRITE_FONT_WEIGHT_NORMAL, 16.0),
            Font::Icon14 => (self.icon_family, DWRITE_FONT_WEIGHT_NORMAL, 14.0),
            Font::Mono => (self.mono_family, DWRITE_FONT_WEIGHT_NORMAL, 12.0),
            Font::Icon12 => (self.icon_family, DWRITE_FONT_WEIGHT_NORMAL, 12.0),
            Font::Icon10 => (self.icon_family, DWRITE_FONT_WEIGHT_NORMAL, 10.0),
            Font::Icon8 => (self.icon_family, DWRITE_FONT_WEIGHT_NORMAL, 8.0),
        };
        unsafe {
            let f = self
                .dwrite
                .CreateTextFormat(
                    family,
                    None,
                    weight,
                    DWRITE_FONT_STYLE_NORMAL,
                    DWRITE_FONT_STRETCH_NORMAL,
                    size,
                    w!(""),
                )
                .ok()?;
            let _ = f.SetTextAlignment(match align {
                Align::Left => DWRITE_TEXT_ALIGNMENT_LEADING,
                Align::Center => DWRITE_TEXT_ALIGNMENT_CENTER,
                Align::Right => DWRITE_TEXT_ALIGNMENT_TRAILING,
            });
            if wrap {
                let _ = f.SetWordWrapping(DWRITE_WORD_WRAPPING_EMERGENCY_BREAK);
                let _ = f.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_NEAR);
            } else {
                let _ = f.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP);
                let _ = f.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER);
            }
            if let Ok(ellipsis) = self.dwrite.CreateEllipsisTrimmingSign(&f) {
                let trimming = DWRITE_TRIMMING {
                    granularity: DWRITE_TRIMMING_GRANULARITY_CHARACTER,
                    delimiter: 0,
                    delimiterCount: 0,
                };
                let _ = f.SetTrimming(&trimming, &ellipsis);
            }
            self.formats
                .borrow_mut()
                .insert((font, align, wrap), f.clone());
            Some(f)
        }
    }

    pub fn ensure_target(&mut self, hwnd: HWND) -> Result<()> {
        if self.target.is_some() {
            return Ok(());
        }
        unsafe {
            let mut rc = RECT::default();
            GetClientRect(hwnd, &mut rc)?;
            let props = D2D1_RENDER_TARGET_PROPERTIES {
                r#type: D2D1_RENDER_TARGET_TYPE_DEFAULT,
                pixelFormat: D2D1_PIXEL_FORMAT {
                    format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
                },
                dpiX: 96.0,
                dpiY: 96.0,
                ..Default::default()
            };
            let hprops = D2D1_HWND_RENDER_TARGET_PROPERTIES {
                hwnd,
                pixelSize: D2D_SIZE_U {
                    width: rc.right.max(1) as u32,
                    height: rc.bottom.max(1) as u32,
                },
                presentOptions: D2D1_PRESENT_OPTIONS_NONE,
            };
            let rt = self.factory.CreateHwndRenderTarget(&props, &hprops)?;
            rt.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
            let brush = rt.CreateSolidColorBrush(&d2c(Color::default()), None)?;
            let dc = rt.cast::<ID2D1DeviceContext>().ok();
            self.bitmaps.clear();
            self.target = Some(Target { rt, dc, brush });
        }
        Ok(())
    }

    pub fn resize(&mut self, w: u32, h: u32) {
        if let Some(t) = &self.target {
            let size = D2D_SIZE_U {
                width: w.max(1),
                height: h.max(1),
            };
            if unsafe { t.rt.Resize(&size) }.is_err() {
                self.target = None;
            }
        }
    }

    pub fn forget_image(&mut self, key: &ImageKey) {
        self.bitmaps.remove(key);
    }

    fn bitmap(
        &mut self,
        key: ImageKey,
        provider: &dyn Fn(&ImageKey) -> Option<Arc<Bitmap>>,
    ) -> Option<ID2D1Bitmap> {
        let frame = self.frame;
        if let Some((b, used)) = self.bitmaps.get_mut(&key) {
            *used = frame;
            return Some(b.clone());
        }
        let src = provider(&key)?;
        let t = self.target.as_ref()?;
        let props = D2D1_BITMAP_PROPERTIES {
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: DXGI_FORMAT_B8G8R8A8_UNORM,
                alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
            },
            dpiX: 96.0,
            dpiY: 96.0,
        };
        let bmp = unsafe {
            t.rt.CreateBitmap(
                D2D_SIZE_U {
                    width: src.w,
                    height: src.h,
                },
                Some(src.data.as_ptr() as _),
                src.w * 4,
                &props,
            )
            .ok()?
        };
        self.bitmaps.insert(key, (bmp.clone(), frame));
        Some(bmp)
    }

    /// Draws a frame. `clear` is the background (transparent to show the Mica backdrop).
    /// Returns false if the device was lost (the caller should repaint).
    pub fn draw(
        &mut self,
        hwnd: HWND,
        scene: &Scene,
        scale: f32,
        clear: Color,
        provider: &dyn Fn(&ImageKey) -> Option<Arc<Bitmap>>,
    ) -> bool {
        if self.ensure_target(hwnd).is_err() {
            return false;
        }
        self.frame += 1;
        // Evict GPU bitmaps not drawn for a while once the cache grows large.
        if self.bitmaps.len() > 3000 {
            let f = self.frame;
            self.bitmaps.retain(|_, (_, used)| f - *used < 30);
        }
        let s = scale;
        {
            let t = self.target.as_ref().unwrap();
            unsafe {
                t.rt.BeginDraw();
                t.rt.SetTransform(&Matrix3x2::identity());
                t.rt.Clear(Some(&d2c(clear)));
            }
        }
        for cmd in &scene.cmds {
            match cmd {
                Cmd::Image { r, key, opacity } => {
                    if let Some(bmp) = self.bitmap(*key, provider) {
                        let t = self.target.as_ref().unwrap();
                        draw_bitmap(t, &bmp, *r, s, *opacity);
                    }
                }
                Cmd::Text {
                    r,
                    s: text,
                    font,
                    color,
                    align,
                    wrap,
                } => {
                    if let Some(fmt) = self.format(*font, *align, *wrap) {
                        let t = self.target.as_ref().unwrap();
                        unsafe {
                            // Text is laid out in DIPs and scaled with the transform so wrapping is DPI-stable.
                            t.rt.SetTransform(&Matrix3x2 {
                                M11: s,
                                M12: 0.0,
                                M21: 0.0,
                                M22: s,
                                M31: 0.0,
                                M32: 0.0,
                            });
                            t.brush.SetColor(&d2c(*color));
                            let rr = d2r(*r, 1.0);
                            t.rt.DrawText(
                                &utf16(text),
                                &fmt,
                                &rr,
                                &t.brush,
                                D2D1_DRAW_TEXT_OPTIONS_CLIP,
                                DWRITE_MEASURING_MODE_NATURAL,
                            );
                            t.rt.SetTransform(&Matrix3x2::identity());
                        }
                    }
                }
                _ => {
                    let t = self.target.as_ref().unwrap();
                    draw_simple(t, cmd, s);
                }
            }
        }
        let t = self.target.as_ref().unwrap();
        if unsafe { t.rt.EndDraw(None, None) }.is_err() {
            self.target = None;
            return false;
        }
        true
    }
}

fn draw_bitmap(t: &Target, bmp: &ID2D1Bitmap, r: Rect, s: f32, opacity: f32) {
    unsafe {
        let size = bmp.GetPixelSize();
        if size.width == 0 || size.height == 0 {
            return;
        }
        let (bw, bh) = (size.width as f32, size.height as f32);
        let (rw, rh) = (r.w() * s, r.h() * s);
        let k = (rw / bw).min(rh / bh);
        let (dw, dh) = (bw * k, bh * k);
        let x = (r.l * s + (rw - dw) / 2.0).round();
        let y = (r.t * s + (rh - dh) / 2.0).round();
        let dest = D2D_RECT_F {
            left: x,
            top: y,
            right: x + dw.round(),
            bottom: y + dh.round(),
        };
        if let Some(dc) = &t.dc {
            let mode = if k < 1.0 {
                D2D1_INTERPOLATION_MODE_HIGH_QUALITY_CUBIC
            } else {
                D2D1_INTERPOLATION_MODE_LINEAR
            };
            dc.DrawBitmap(bmp, Some(&dest), opacity, mode, None, None);
        } else {
            t.rt.DrawBitmap(
                bmp,
                Some(&dest),
                opacity,
                D2D1_BITMAP_INTERPOLATION_MODE_LINEAR,
                None,
            );
        }
    }
}

fn draw_simple(t: &Target, cmd: &Cmd, s: f32) {
    unsafe {
        match cmd {
            Cmd::Fill(r, c) => {
                t.brush.SetColor(&d2c(*c));
                t.rt.FillRectangle(&d2r(*r, s), &t.brush);
            }
            Cmd::Round(r, rad, c) => {
                t.brush.SetColor(&d2c(*c));
                let rr = D2D1_ROUNDED_RECT {
                    rect: d2r(*r, s),
                    radiusX: rad * s,
                    radiusY: rad * s,
                };
                t.rt.FillRoundedRectangle(&rr, &t.brush);
            }
            Cmd::Stroke(r, rad, width, c) => {
                t.brush.SetColor(&d2c(*c));
                // Inset by half the stroke so the outline stays inside the rect (crisp at 1px).
                let hw = (width * s).max(1.0) / 2.0;
                let mut rect = d2r(*r, s);
                rect.left = rect.left.round() + hw;
                rect.top = rect.top.round() + hw;
                rect.right = rect.right.round() - hw;
                rect.bottom = rect.bottom.round() - hw;
                let rr = D2D1_ROUNDED_RECT {
                    rect,
                    radiusX: rad * s,
                    radiusY: rad * s,
                };
                t.rt.DrawRoundedRectangle(&rr, &t.brush, (width * s).max(1.0), None);
            }
            Cmd::Line(x0, y0, x1, y1, w, c) => {
                t.brush.SetColor(&d2c(*c));
                let p0 = windows_numerics::Vector2 {
                    X: x0 * s,
                    Y: y0 * s,
                };
                let p1 = windows_numerics::Vector2 {
                    X: x1 * s,
                    Y: y1 * s,
                };
                t.rt.DrawLine(p0, p1, &t.brush, (w * s).max(1.0), None);
            }
            Cmd::PushClip(r) => {
                let mut rr = d2r(*r, s);
                rr.left = rr.left.round();
                rr.top = rr.top.round();
                rr.right = rr.right.round();
                rr.bottom = rr.bottom.round();
                t.rt.PushAxisAlignedClip(&rr, D2D1_ANTIALIAS_MODE_ALIASED);
            }
            Cmd::PopClip => t.rt.PopAxisAlignedClip(),
            _ => {}
        }
    }
}

impl Measure for Gfx {
    fn text_width(&self, s: &str, font: Font) -> f32 {
        if s.is_empty() {
            return 0.0;
        }
        let key = (s.to_string(), font);
        if let Some(w) = self.widths.borrow().get(&key) {
            return *w;
        }
        let Some(fmt) = self.format(font, Align::Left, false) else {
            return 0.0;
        };
        let w = unsafe {
            match self
                .dwrite
                .CreateTextLayout(&utf16(s), &fmt, 100_000.0, 1000.0)
            {
                Ok(layout) => {
                    let mut m = DWRITE_TEXT_METRICS::default();
                    let _ = layout.GetMetrics(&mut m);
                    m.widthIncludingTrailingWhitespace
                }
                Err(_) => 0.0,
            }
        };
        let mut cache = self.widths.borrow_mut();
        if cache.len() > 20_000 {
            cache.clear();
        }
        cache.insert(key, w);
        w
    }
}

/// Cascadia Mono (Windows 11 / Terminal) when installed, else Consolas.
fn mono_family(dwrite: &IDWriteFactory) -> PCWSTR {
    unsafe {
        let mut fonts = None;
        let (mut index, mut exists) = (0u32, BOOL(0));
        if dwrite.GetSystemFontCollection(&mut fonts, false).is_ok() {
            if let Some(fonts) = fonts {
                let _ = fonts.FindFamilyName(w!("Cascadia Mono"), &mut index, &mut exists);
            }
        }
        if exists.as_bool() {
            w!("Cascadia Mono")
        } else {
            w!("Consolas")
        }
    }
}
