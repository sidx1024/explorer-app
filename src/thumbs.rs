//! Thumbnail workers.
//!
//! Each request is served from the folder's `thumbs26.db` when it holds a fresh entry; otherwise the
//! shell extracts one (`IShellItemImageFactory`, so every installed thumbnail provider works: photos,
//! videos via Media Foundation, PDFs, ...), it is stored as JPEG (PNG when it has transparency) at
//! 256 px, and decoded at the size the view needs. Requests are replaced wholesale whenever the
//! visible range changes, so scrolling never queues stale work.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use windows::core::{Interface, GUID, PCWSTR};
use windows::Win32::Foundation::SIZE;
use windows::Win32::Graphics::Gdi::{DeleteObject, HGDIOBJ};
use windows::Win32::Graphics::Imaging::*;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, IStream, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
    STATFLAG_NONAME, STATSTG, STREAM_SEEK_SET,
};
use windows::Win32::UI::Shell::{
    IShellItemImageFactory, SHCreateItemFromParsingName, SHCreateMemStream, SIIGBF_THUMBNAILONLY,
};

use crate::config::ThumbLocation;
use crate::icons::{hbitmap_pixels, premultiply};
use crate::msg::{Msg, Notifier};
use crate::platform::{folder_key, fnv1a64, wide};
use crate::scene::{Bitmap, ImageKey, ImageStore};
use crate::store::{ThumbDb, ThumbFmt, FILE_NAME};

/// Thumbnails are stored at this size; views up to Extra large icons are served from it.
pub const STORE_DIM: u32 = 256;

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

pub struct Req {
    pub tab: u64,
    pub gen: u64,
    pub idx: u32,
    pub path: PathBuf,
    pub name: String,
    pub size: u64,
    pub mtime: u64,
    pub px: u32,
    pub db: Arc<Mutex<ThumbDb>>,
}

struct Queue {
    jobs: VecDeque<Req>,
    quit: bool,
}

pub struct Pool {
    q: Arc<(Mutex<Queue>, Condvar)>,
}

impl Pool {
    pub fn new(threads: usize, notifier: Notifier, images: Arc<ImageStore>) -> Pool {
        let q = Arc::new((
            Mutex::new(Queue {
                jobs: VecDeque::new(),
                quit: false,
            }),
            Condvar::new(),
        ));
        for _ in 0..threads {
            let q = q.clone();
            let notifier = notifier.clone();
            let images = images.clone();
            std::thread::spawn(move || worker(q, notifier, images));
        }
        Pool { q }
    }

    /// Replaces all pending requests (first = most urgent).
    pub fn set(&self, jobs: Vec<Req>) {
        let (m, cv) = &*self.q;
        let mut q = m.lock().unwrap();
        q.jobs = jobs.into();
        cv.notify_all();
    }

    pub fn pending(&self) -> usize {
        self.q.0.lock().unwrap().jobs.len()
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        let (m, cv) = &*self.q;
        m.lock().unwrap().quit = true;
        cv.notify_all();
    }
}

fn worker(q: Arc<(Mutex<Queue>, Condvar)>, notifier: Notifier, images: Arc<ImageStore>) {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let Ok(wic) = (unsafe {
        CoCreateInstance::<_, IWICImagingFactory>(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)
    }) else {
        return;
    };
    loop {
        let job = {
            let (m, cv) = &*q;
            let mut g = m.lock().unwrap();
            loop {
                if g.quit {
                    return;
                }
                if let Some(j) = g.jobs.pop_front() {
                    break j;
                }
                g = cv.wait(g).unwrap();
            }
        };
        let bmp = thumbnail(&wic, &job);
        let key = bmp.map(|b| {
            let key = ImageKey::Thumb(NEXT_ID.fetch_add(1, Ordering::Relaxed));
            images.put(key, b);
            key
        });
        notifier.send(Msg::Thumb {
            tab: job.tab,
            gen: job.gen,
            idx: job.idx,
            key,
            px: job.px,
        });
    }
}

fn thumbnail(wic: &IWICImagingFactory, job: &Req) -> Option<Bitmap> {
    let want = job.px.min(STORE_DIM) as u16;
    // 1. Our cache.
    let cached = {
        let mut db = job.db.lock().unwrap();
        match db.lookup(&job.name, job.size, job.mtime, want) {
            Some(m) if m.fmt == ThumbFmt::None => return None,
            Some(m) => db.read_blob(&m).ok(),
            None => None,
        }
    };
    if let Some(data) = cached {
        if let Some(b) = decode(wic, &data, job.px) {
            return Some(b);
        }
    }
    // 2. The shell.
    let Some((w, h, px)) = extract(&job.path, STORE_DIM) else {
        let _ = job
            .db
            .lock()
            .unwrap()
            .put(&job.name, job.size, job.mtime, STORE_DIM as u16, 0, 0, ThumbFmt::None, &[]);
        return None;
    };
    let opaque = px.chunks_exact(4).all(|p| p[3] == 255);
    let fmt = if opaque { ThumbFmt::Jpeg } else { ThumbFmt::Png };
    if let Some(data) = encode(wic, w, h, &px, fmt) {
        let _ = job.db.lock().unwrap().put(
            &job.name,
            job.size,
            job.mtime,
            STORE_DIM as u16,
            w as u16,
            h as u16,
            fmt,
            &data,
        );
    }
    scale(wic, w, h, &px, job.px)
}

/// A fresh id for an image put into the ImageStore.
pub fn next_image_id() -> u64 {
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

/// A shell thumbnail as a bitmap (preview pane), any size up to `dim`.
pub fn extract_any(path: &Path, dim: u32) -> Option<Bitmap> {
    extract(path, dim).map(|(w, h, data)| Bitmap { w, h, data })
}

/// Asks the shell for a thumbnail no larger than `dim` (premultiplied BGRA).
pub fn extract(path: &Path, dim: u32) -> Option<(u32, u32, Vec<u8>)> {
    unsafe {
        let w = wide(path);
        let factory: IShellItemImageFactory =
            SHCreateItemFromParsingName(PCWSTR(w.as_ptr()), None).ok()?;
        let hbm = factory
            .GetImage(
                SIZE {
                    cx: dim as i32,
                    cy: dim as i32,
                },
                SIIGBF_THUMBNAILONLY,
            )
            .ok()?;
        let r = hbitmap_pixels(hbm);
        let _ = DeleteObject(HGDIOBJ(hbm.0));
        let (w, h, mut px) = r?;
        if px.chunks_exact(4).all(|p| p[3] == 0) {
            // No alpha channel at all (typical for photos): opaque.
            px.chunks_exact_mut(4).for_each(|p| p[3] = 255);
        } else {
            // Keep it valid premultiplied data whatever the provider handed back.
            for p in px.chunks_exact_mut(4) {
                let a = p[3];
                p[0] = p[0].min(a);
                p[1] = p[1].min(a);
                p[2] = p[2].min(a);
            }
        }
        Some((w, h, px))
    }
}

fn fit(w: u32, h: u32, max: u32) -> (u32, u32) {
    if w <= max && h <= max {
        return (w.max(1), h.max(1));
    }
    let k = max as f32 / w.max(h) as f32;
    (
        ((w as f32 * k).round() as u32).max(1),
        ((h as f32 * k).round() as u32).max(1),
    )
}

fn copy_pixels(src: &IWICBitmapSource, w: u32, h: u32) -> Option<Bitmap> {
    unsafe {
        let mut data = vec![0u8; (w * h * 4) as usize];
        src.CopyPixels(std::ptr::null(), w * 4, &mut data).ok()?;
        Some(Bitmap { w, h, data })
    }
}

fn scale(wic: &IWICImagingFactory, w: u32, h: u32, px: &[u8], max: u32) -> Option<Bitmap> {
    let (nw, nh) = fit(w, h, max);
    if (nw, nh) == (w, h) {
        return Some(Bitmap {
            w,
            h,
            data: px.to_vec(),
        });
    }
    unsafe {
        let bmp = wic
            .CreateBitmapFromMemory(w, h, &GUID_WICPixelFormat32bppPBGRA, w * 4, px)
            .ok()?;
        let scaler = wic.CreateBitmapScaler().ok()?;
        scaler
            .Initialize(&bmp, nw, nh, WICBitmapInterpolationModeHighQualityCubic)
            .ok()?;
        copy_pixels(&scaler.cast().ok()?, nw, nh)
    }
}

fn decode(wic: &IWICImagingFactory, data: &[u8], max: u32) -> Option<Bitmap> {
    unsafe {
        let stream = SHCreateMemStream(Some(data))?;
        let dec = wic
            .CreateDecoderFromStream(&stream, std::ptr::null(), WICDecodeMetadataCacheOnDemand)
            .ok()?;
        let frame = dec.GetFrame(0).ok()?;
        let (mut w, mut h) = (0, 0);
        frame.GetSize(&mut w, &mut h).ok()?;
        let (nw, nh) = fit(w, h, max);
        let source: IWICBitmapSource = if (nw, nh) != (w, h) {
            let scaler = wic.CreateBitmapScaler().ok()?;
            scaler
                .Initialize(&frame, nw, nh, WICBitmapInterpolationModeHighQualityCubic)
                .ok()?;
            scaler.cast().ok()?
        } else {
            frame.cast().ok()?
        };
        let conv = wic.CreateFormatConverter().ok()?;
        conv.Initialize(
            &source,
            &GUID_WICPixelFormat32bppPBGRA,
            WICBitmapDitherTypeNone,
            None,
            0.0,
            WICBitmapPaletteTypeCustom,
        )
        .ok()?;
        copy_pixels(&conv.cast().ok()?, nw, nh)
    }
}

fn encode(wic: &IWICImagingFactory, w: u32, h: u32, px: &[u8], fmt: ThumbFmt) -> Option<Vec<u8>> {
    unsafe {
        let container: GUID = match fmt {
            ThumbFmt::Jpeg => GUID_ContainerFormatJpeg,
            _ => GUID_ContainerFormatPng,
        };
        let stream: IStream = SHCreateMemStream(None)?;
        let enc = wic.CreateEncoder(&container, std::ptr::null()).ok()?;
        enc.Initialize(&stream, WICBitmapEncoderNoCache).ok()?;
        let mut frame = None;
        let mut props = None;
        enc.CreateNewFrame(&mut frame, &mut props).ok()?;
        let frame = frame?;
        frame.Initialize(props.as_ref()).ok()?;
        frame.SetSize(w, h).ok()?;
        let src = wic
            .CreateBitmapFromMemory(w, h, &GUID_WICPixelFormat32bppPBGRA, w * 4, px)
            .ok()?;
        // JPEG has no alpha; PNG stores straight (non-premultiplied) alpha.
        let target = match fmt {
            ThumbFmt::Jpeg => GUID_WICPixelFormat24bppBGR,
            _ => GUID_WICPixelFormat32bppBGRA,
        };
        let conv = wic.CreateFormatConverter().ok()?;
        conv.Initialize(
            &src,
            &target,
            WICBitmapDitherTypeNone,
            None,
            0.0,
            WICBitmapPaletteTypeCustom,
        )
        .ok()?;
        let mut pf = target;
        frame.SetPixelFormat(&mut pf).ok()?;
        frame.WriteSource(&conv, std::ptr::null()).ok()?;
        frame.Commit().ok()?;
        enc.Commit().ok()?;
        let mut stat = STATSTG::default();
        stream.Stat(&mut stat, STATFLAG_NONAME).ok()?;
        let len = stat.cbSize as usize;
        stream.Seek(0, STREAM_SEEK_SET, None).ok()?;
        let mut out = vec![0u8; len];
        let mut read = 0u32;
        stream
            .Read(out.as_mut_ptr() as _, len as u32, Some(&mut read))
            .ok()
            .ok()?;
        out.truncate(read as usize);
        Some(out)
    }
}

/// Where a folder's `thumbs26.db` lives, and opening it.
pub fn open_db(folder: &Path, loc: ThumbLocation) -> ThumbDb {
    let key = folder_key(folder);
    let central = || {
        let dir = crate::platform::app_data_dir().join("folders");
        let _ = std::fs::create_dir_all(&dir);
        dir.join(format!("{:016x}.{FILE_NAME}", fnv1a64(key.as_bytes())))
    };
    if loc == ThumbLocation::InFolder && folder_is_local_writable(folder) {
        let p = folder.join(FILE_NAME);
        let mut db = ThumbDb::open(&p, &folder.to_string_lossy());
        db.set_hidden(true);
        return db;
    }
    // Central store; an in-folder db written earlier is still read if present.
    let in_folder = folder.join(FILE_NAME);
    if loc == ThumbLocation::Central && in_folder.exists() {
        let db = ThumbDb::open(&in_folder, &folder.to_string_lossy());
        if db.settings().is_some() || db.len() > 0 {
            let mut db = db;
            db.set_hidden(true);
            return db;
        }
    }
    ThumbDb::open(&central(), &folder.to_string_lossy())
}

fn folder_is_local_writable(folder: &Path) -> bool {
    use windows::Win32::Storage::FileSystem::{GetDriveTypeW, FILE_ATTRIBUTE_READONLY};
    let s = folder.to_string_lossy();
    if s.starts_with(r"\\") {
        return false;
    }
    let root: String = s.chars().take(3).collect();
    let t = unsafe { GetDriveTypeW(PCWSTR(wide(&root).as_ptr())) };
    // DRIVE_FIXED = 3, DRIVE_REMOVABLE = 2 (USB sticks: keep their folders clean too).
    if t != 3 {
        return false;
    }
    crate::platform::file_attributes(folder)
        .map(|a| a & FILE_ATTRIBUTE_READONLY.0 == 0 || true)
        .unwrap_or(false)
}

/// Decodes an image file straight with WIC, scaled during decode to fit `max_w`×`max_h`
/// (never enlarged) and rotated per its EXIF orientation. Much faster than asking the shell for a
/// large thumbnail, which decodes the full image out of process and writes it to its cache.
pub fn decode_file(path: &Path, max_w: u32, max_h: u32) -> Option<Bitmap> {
    use windows::Win32::Foundation::GENERIC_READ;
    unsafe {
        let wic: IWICImagingFactory =
            CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER).ok()?;
        let dec = wic
            .CreateDecoderFromFilename(
                PCWSTR(wide(path).as_ptr()),
                None,
                GENERIC_READ,
                WICDecodeMetadataCacheOnDemand,
            )
            .ok()?;
        let frame = dec.GetFrame(0).ok()?;
        let (mut w, mut h) = (0, 0);
        frame.GetSize(&mut w, &mut h).ok()?;
        let orientation = exif_orientation(&frame);
        // Orientations 5..8 swap width and height on screen.
        let swapped = (5..=8).contains(&orientation);
        let (sw, sh) = if swapped { (h, w) } else { (w, h) };
        let k = (max_w.max(1) as f32 / sw as f32).min(max_h.max(1) as f32 / sh as f32).min(1.0);
        let (nw, nh) = (
            ((w as f32 * k).round() as u32).max(1),
            ((h as f32 * k).round() as u32).max(1),
        );
        let mut source: IWICBitmapSource = frame.cast().ok()?;
        if (nw, nh) != (w, h) {
            // The scaler lets the JPEG decoder downscale in the DCT domain: most of the speed-up.
            let scaler = wic.CreateBitmapScaler().ok()?;
            scaler.Initialize(&source, nw, nh, WICBitmapInterpolationModeFant).ok()?;
            source = scaler.cast().ok()?;
        }
        let rot = match orientation {
            2 => WICBitmapTransformFlipHorizontal,
            3 => WICBitmapTransformRotate180,
            4 => WICBitmapTransformFlipVertical,
            5 => WICBitmapTransformOptions(WICBitmapTransformRotate90.0 | WICBitmapTransformFlipHorizontal.0),
            6 => WICBitmapTransformRotate90,
            7 => WICBitmapTransformOptions(WICBitmapTransformRotate270.0 | WICBitmapTransformFlipHorizontal.0),
            8 => WICBitmapTransformRotate270,
            _ => WICBitmapTransformRotate0,
        };
        if rot != WICBitmapTransformRotate0 {
            let r = wic.CreateBitmapFlipRotator().ok()?;
            r.Initialize(&source, rot).ok()?;
            source = r.cast().ok()?;
        }
        let conv = wic.CreateFormatConverter().ok()?;
        conv.Initialize(
            &source,
            &GUID_WICPixelFormat32bppPBGRA,
            WICBitmapDitherTypeNone,
            None,
            0.0,
            WICBitmapPaletteTypeCustom,
        )
        .ok()?;
        let (ow, oh) = if swapped { (nh, nw) } else { (nw, nh) };
        copy_pixels(&conv.cast().ok()?, ow, oh)
    }
}

/// EXIF orientation (1..8) for JPEG/TIFF/HEIF frames; 1 when absent.
fn exif_orientation(frame: &IWICBitmapFrameDecode) -> u16 {
    unsafe {
        let Ok(reader) = frame.GetMetadataQueryReader() else {
            return 1;
        };
        for q in ["/app1/ifd/{ushort=274}", "/ifd/{ushort=274}"] {
            let mut v = windows::Win32::System::Com::StructuredStorage::PROPVARIANT::default();
            if reader.GetMetadataByName(PCWSTR(wide(q).as_ptr()), &mut v).is_ok() {
                let inner = &v.Anonymous.Anonymous;
                // VT_UI2: no allocation to free.
                if inner.vt.0 == 18 {
                    let o = inner.Anonymous.uiVal;
                    if (1..=8).contains(&o) {
                        return o;
                    }
                }
            }
        }
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `cargo test --release -- --ignored --nocapture bench_preview` (needs E26_BENCH_IMG).
    #[test]
    #[ignore]
    fn bench_preview() {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        let a = PathBuf::from(std::env::var("E26_BENCH_IMG").unwrap());
        let b = PathBuf::from(std::env::var("E26_BENCH_IMG2").unwrap());
        let t = std::time::Instant::now();
        let s = extract_any(&a, 1300).map(|b| (b.w, b.h));
        println!("shell thumbnail 1300px: {:?} in {:?}", s, t.elapsed());
        let t = std::time::Instant::now();
        let w = decode_file(&b, 900, 1300).map(|b| (b.w, b.h));
        println!("WIC decode to 900x1300: {:?} in {:?}", w, t.elapsed());
    }
}
