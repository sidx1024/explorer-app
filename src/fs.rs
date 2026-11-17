//! Folder listing, This PC, recursive search and Explorer-compatible sorting.

use std::cmp::Ordering as CmpOrdering;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{GetLastError, ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND};
use windows::Win32::Storage::FileSystem::{
    FindClose, FindExInfoBasic, FindExSearchNameMatch, FindFirstFileExW, FindNextFileW,
    GetDiskFreeSpaceExW, GetLogicalDrives, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_HIDDEN,
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_ATTRIBUTE_SYSTEM, FIND_FIRST_EX_LARGE_FETCH,
    WIN32_FIND_DATAW,
};
use windows::Win32::UI::Shell::StrCmpLogicalW;

use crate::icons;
use crate::platform::{from_wide, long_path, wide};
use crate::store;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Thumb {
    /// Not asked for yet.
    Unknown,
    Pending(u32),
    Ready(u64, u32),
    /// The shell has no thumbnail for this file.
    None,
}

#[derive(Clone)]
pub struct Entry {
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
    pub size: u64,
    pub modified: u64,
    pub attrs: u32,
    pub type_name: Arc<str>,
    /// System image list index (overlay in bits 24..28), -1 while unknown.
    pub icon: i32,
    pub thumb: Thumb,
    /// Drives in This PC: (total, free) bytes.
    pub drive: Option<(u64, u64)>,
    /// Lowercase extension including the dot ("" for folders).
    pub ext: String,
}

impl Entry {
    pub fn hidden(&self) -> bool {
        self.attrs & FILE_ATTRIBUTE_HIDDEN.0 != 0
    }
    /// Hidden + system: "protected operating system files" (desktop.ini, Thumbs.db...).
    pub fn protected(&self) -> bool {
        let m = FILE_ATTRIBUTE_HIDDEN.0 | FILE_ATTRIBUTE_SYSTEM.0;
        self.attrs & m == m
    }
    /// Name as displayed, honoring "File name extensions".
    pub fn display_name(&self, show_ext: bool) -> &str {
        if show_ext || self.is_dir || self.drive.is_some() || self.ext.is_empty() {
            return &self.name;
        }
        let stem_len = self.name.len() - self.ext.len();
        if stem_len == 0 {
            &self.name
        } else {
            &self.name[..stem_len]
        }
    }
    pub fn is_media(&self) -> bool {
        is_image_ext(&self.ext) || is_video_ext(&self.ext)
    }
}

pub fn is_image_ext(e: &str) -> bool {
    matches!(
        e,
        ".jpg" | ".jpeg" | ".png" | ".gif" | ".bmp" | ".webp" | ".heic" | ".heif" | ".avif"
            | ".tif" | ".tiff" | ".ico" | ".jxr" | ".dng" | ".cr2" | ".cr3" | ".nef" | ".arw"
            | ".raf" | ".orf" | ".rw2" | ".svg" | ".jfif"
    )
}

pub fn is_video_ext(e: &str) -> bool {
    matches!(
        e,
        ".mp4" | ".mkv" | ".mov" | ".avi" | ".wmv" | ".webm" | ".m4v" | ".mpg" | ".mpeg"
            | ".3gp" | ".ts" | ".m2ts" | ".mts" | ".flv"
    )
}

fn ext_of(name: &str, is_dir: bool) -> String {
    if is_dir {
        return String::new();
    }
    match name.rfind('.') {
        Some(i) if i > 0 || name.len() > 1 => name[i..].to_lowercase(),
        _ => String::new(),
    }
}

fn filetime(lo: u32, hi: u32) -> u64 {
    (hi as u64) << 32 | lo as u64
}

pub fn entry_from_find(dir: &Path, d: &WIN32_FIND_DATAW) -> Option<Entry> {
    let name = from_wide(&d.cFileName);
    if name == "." || name == ".." {
        return None;
    }
    let attrs = d.dwFileAttributes;
    let is_dir = attrs & FILE_ATTRIBUTE_DIRECTORY.0 != 0;
    let ext = ext_of(&name, is_dir);
    let (type_name, icon) = if is_dir {
        (icons::folder_type_name(), icons::folder_icon())
    } else {
        (icons::type_name(&ext), icons::ext_icon(&ext))
    };
    Some(Entry {
        path: dir.join(&name),
        name,
        is_dir,
        size: if is_dir {
            0
        } else {
            (d.nFileSizeHigh as u64) << 32 | d.nFileSizeLow as u64
        },
        modified: filetime(d.ftLastWriteTime.dwLowDateTime, d.ftLastWriteTime.dwHighDateTime),
        attrs,
        type_name,
        icon,
        thumb: Thumb::Unknown,
        drive: None,
        ext,
    })
}

/// Enumerates a folder with FindFirstFileEx (large fetch, no short names): fast even for 100k items.
pub fn list_dir(dir: &Path, cancel: &AtomicBool) -> Result<Vec<Entry>, String> {
    let pattern = long_path(&dir.join("*"));
    let mut out = Vec::new();
    unsafe {
        let mut d = WIN32_FIND_DATAW::default();
        let h = match FindFirstFileExW(
            PCWSTR(pattern.as_ptr()),
            FindExInfoBasic,
            &mut d as *mut _ as _,
            FindExSearchNameMatch,
            None,
            FIND_FIRST_EX_LARGE_FETCH,
        ) {
            Ok(h) => h,
            Err(e) => {
                let code = GetLastError();
                return if code == ERROR_FILE_NOT_FOUND {
                    Ok(out)
                } else if code == ERROR_ACCESS_DENIED {
                    Err("You don't currently have permission to access this folder.".into())
                } else if code == ERROR_PATH_NOT_FOUND {
                    Err(format!("{} is unavailable.", dir.display()))
                } else {
                    Err(e.message())
                };
            }
        };
        loop {
            if let Some(e) = entry_from_find(dir, &d) {
                out.push(e);
            }
            if cancel.load(Ordering::Relaxed) {
                break;
            }
            if FindNextFileW(h, &mut d).is_err() {
                break;
            }
        }
        let _ = FindClose(h);
    }
    Ok(out)
}

/// Drives for This PC, with display names ("Local Disk (C:)") and capacity.
pub fn list_this_pc() -> Vec<Entry> {
    let mut out = Vec::new();
    let mask = unsafe { GetLogicalDrives() };
    for i in 0..26u32 {
        if mask & (1 << i) == 0 {
            continue;
        }
        let root = format!("{}:\\", (b'A' + i as u8) as char);
        let (name, icon, type_name) = icons::drive_info(&root);
        let (mut total, mut free) = (0u64, 0u64);
        unsafe {
            let _ = GetDiskFreeSpaceExW(
                PCWSTR(wide(&root).as_ptr()),
                None,
                Some(&mut total),
                Some(&mut free),
            );
        }
        out.push(Entry {
            name,
            path: PathBuf::from(&root),
            is_dir: true,
            size: total,
            modified: 0,
            attrs: FILE_ATTRIBUTE_DIRECTORY.0,
            type_name: type_name.into(),
            icon,
            thumb: Thumb::Unknown,
            drive: Some((total, free)),
            ext: String::new(),
        });
    }
    out
}

/// Recursive name search under `root`, streaming batches to `emit`. Skips reparse points
/// (junctions/symlinks) to avoid cycles. Matching is case-insensitive substring or `*`/`?` wildcards.
pub fn search(
    root: &Path,
    query: &str,
    recursive: bool,
    cancel: &AtomicBool,
    emit: &mut dyn FnMut(Vec<Entry>),
) {
    let q = query.to_lowercase();
    let wildcard = q.contains('*') || q.contains('?');
    let mut stack = vec![root.to_path_buf()];
    let mut batch = Vec::new();
    let mut last = std::time::Instant::now();
    while let Some(dir) = stack.pop() {
        if cancel.load(Ordering::Relaxed) {
            return;
        }
        let Ok(entries) = list_dir(&dir, cancel) else {
            continue;
        };
        for e in entries {
            if recursive && e.is_dir && e.attrs & FILE_ATTRIBUTE_REPARSE_POINT.0 == 0 {
                stack.push(e.path.clone());
            }
            let n = e.name.to_lowercase();
            let hit = if wildcard {
                wild_match(&q, &n)
            } else {
                n.contains(&q)
            };
            if hit && !e.name.eq_ignore_ascii_case(store::FILE_NAME) {
                batch.push(e);
            }
        }
        if !batch.is_empty() && (batch.len() >= 500 || last.elapsed().as_millis() > 150) {
            emit(std::mem::take(&mut batch));
            last = std::time::Instant::now();
        }
    }
    if !batch.is_empty() {
        emit(batch);
    }
}

pub fn wild_match(pat: &str, s: &str) -> bool {
    let (p, t): (Vec<char>, Vec<char>) = (pat.chars().collect(), s.chars().collect());
    let (mut pi, mut ti, mut star, mut mark) = (0usize, 0usize, usize::MAX, 0usize);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = pi;
            mark = ti;
            pi += 1;
        } else if star != usize::MAX {
            pi = star + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Column {
    Name,
    Modified,
    Type,
    Size,
    /// Containing folder (search results).
    Folder,
    /// Drive capacity (This PC).
    Total,
    Free,
}

impl Column {
    pub fn title(self) -> &'static str {
        match self {
            Column::Name => "Name",
            Column::Modified => "Date modified",
            Column::Type => "Type",
            Column::Size => "Size",
            Column::Folder => "Folder",
            Column::Total => "Total size",
            Column::Free => "Free space",
        }
    }
    pub fn id(self) -> &'static str {
        match self {
            Column::Name => "name",
            Column::Modified => "modified",
            Column::Type => "type",
            Column::Size => "size",
            Column::Folder => "folder",
            Column::Total => "total",
            Column::Free => "free",
        }
    }
    pub fn from_id(s: &str) -> Option<Column> {
        Some(match s {
            "name" => Column::Name,
            "modified" => Column::Modified,
            "type" => Column::Type,
            "size" => Column::Size,
            "folder" => Column::Folder,
            "total" => Column::Total,
            "free" => Column::Free,
            _ => return None,
        })
    }
    pub fn right_aligned(self) -> bool {
        matches!(self, Column::Size | Column::Total | Column::Free)
    }
    pub fn default_width(self) -> f32 {
        match self {
            Column::Name => 280.0,
            Column::Modified => 150.0,
            Column::Type => 130.0,
            Column::Size => 90.0,
            Column::Folder => 300.0,
            Column::Total => 100.0,
            Column::Free => 100.0,
        }
    }
}

/// Explorer's "logical" name order: digits compare numerically ("file2" < "file10").
pub fn cmp_names(a: &str, b: &str) -> CmpOrdering {
    let (wa, wb) = (wide(a), wide(b));
    let r = unsafe { StrCmpLogicalW(PCWSTR(wa.as_ptr()), PCWSTR(wb.as_ptr())) };
    r.cmp(&0)
}

/// Sorts `order` (indices into `entries`). Folders always come first, like Explorer.
pub fn sort(entries: &[Entry], order: &mut [u32], key: Column, asc: bool) {
    // Precompute UTF-16 names once; StrCmpLogicalW is the expensive part for big folders.
    let names: Vec<Vec<u16>> = entries.iter().map(|e| wide(&e.name)).collect();
    let by_name = |a: usize, b: usize| -> CmpOrdering {
        unsafe { StrCmpLogicalW(PCWSTR(names[a].as_ptr()), PCWSTR(names[b].as_ptr())) }.cmp(&0)
    };
    order.sort_by(|&ia, &ib| {
        let (a, b) = (&entries[ia as usize], &entries[ib as usize]);
        let dirs = b.is_dir.cmp(&a.is_dir);
        if dirs != CmpOrdering::Equal {
            return dirs;
        }
        let primary = match key {
            // Drives sort by letter, as in File Explorer, not by label ("WD2TB (D:)" < "Windows (C:)").
            Column::Name if a.drive.is_some() && b.drive.is_some() => a.path.cmp(&b.path),
            Column::Name => by_name(ia as usize, ib as usize),
            Column::Modified => a.modified.cmp(&b.modified),
            Column::Type => a
                .type_name
                .to_lowercase()
                .cmp(&b.type_name.to_lowercase()),
            Column::Size => a.size.cmp(&b.size),
            Column::Folder => a
                .path
                .parent()
                .cmp(&b.path.parent()),
            Column::Total => a.drive.map(|d| d.0).cmp(&b.drive.map(|d| d.0)),
            Column::Free => a.drive.map(|d| d.1).cmp(&b.drive.map(|d| d.1)),
        };
        let primary = if asc { primary } else { primary.reverse() };
        primary.then_with(|| by_name(ia as usize, ib as usize))
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drives_sort_by_letter() {
        let drive = |name: &str, root: &str| Entry {
            name: name.into(),
            path: PathBuf::from(root),
            is_dir: true,
            size: 0,
            modified: 0,
            attrs: FILE_ATTRIBUTE_DIRECTORY.0,
            type_name: "Local Disk".into(),
            icon: -1,
            thumb: Thumb::Unknown,
            drive: Some((0, 0)),
            ext: String::new(),
        };
        let e = [drive("WD2TB (D:)", "D:\\"), drive("XBOX (E:)", "E:\\"), drive("Windows (C:)", "C:\\")];
        let mut order = [0u32, 1, 2];
        sort(&e, &mut order, Column::Name, true);
        assert_eq!(order, [2, 0, 1]);
    }

    #[test]
    fn wildcard() {
        assert!(wild_match("*.jpg", "photo.jpg"));
        assert!(wild_match("ph?to*", "photo.jpg"));
        assert!(!wild_match("*.png", "photo.jpg"));
        assert!(wild_match("*", ""));
    }

    #[test]
    fn extensions() {
        assert_eq!(ext_of("a.JPG", false), ".jpg");
        assert_eq!(ext_of("archive.tar.gz", false), ".gz");
        assert_eq!(ext_of("noext", false), "");
        assert_eq!(ext_of(".gitignore", false), ".gitignore");
        assert_eq!(ext_of("dir.d", true), "");
    }
}

/// An entry built from indexed data (search results).
pub fn entry_from_parts(path: PathBuf, name: String, attrs: u32, size: u64, modified: u64) -> Entry {
    let is_dir = attrs & FILE_ATTRIBUTE_DIRECTORY.0 != 0;
    let ext = ext_of(&name, is_dir);
    let (type_name, icon) = if is_dir {
        (icons::folder_type_name(), icons::folder_icon())
    } else {
        (icons::type_name(&ext), icons::ext_icon(&ext))
    };
    Entry {
        path,
        name,
        is_dir,
        size: if is_dir { 0 } else { size },
        modified,
        attrs,
        type_name,
        icon,
        thumb: Thumb::Unknown,
        drive: None,
        ext,
    }
}

// ---- search options (Explorer's "Search options" menu) ------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DateFilter {
    Today,
    Yesterday,
    ThisWeek,
    LastWeek,
    ThisMonth,
    LastMonth,
    ThisYear,
    LastYear,
}

impl DateFilter {
    pub const ALL: [DateFilter; 8] = [
        DateFilter::Today,
        DateFilter::Yesterday,
        DateFilter::ThisWeek,
        DateFilter::LastWeek,
        DateFilter::ThisMonth,
        DateFilter::LastMonth,
        DateFilter::ThisYear,
        DateFilter::LastYear,
    ];
    pub fn label(self) -> &'static str {
        match self {
            DateFilter::Today => "Today",
            DateFilter::Yesterday => "Yesterday",
            DateFilter::ThisWeek => "This week",
            DateFilter::LastWeek => "Last week",
            DateFilter::ThisMonth => "This month",
            DateFilter::LastMonth => "Last month",
            DateFilter::ThisYear => "This year",
            DateFilter::LastYear => "Last year",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KindFilter {
    Folder,
    Document,
    Picture,
    Video,
    Music,
    Program,
    Compressed,
}

impl KindFilter {
    pub const ALL: [KindFilter; 7] = [
        KindFilter::Folder,
        KindFilter::Document,
        KindFilter::Picture,
        KindFilter::Video,
        KindFilter::Music,
        KindFilter::Program,
        KindFilter::Compressed,
    ];
    pub fn label(self) -> &'static str {
        match self {
            KindFilter::Folder => "Folder",
            KindFilter::Document => "Document",
            KindFilter::Picture => "Picture",
            KindFilter::Video => "Movie",
            KindFilter::Music => "Music",
            KindFilter::Program => "Program",
            KindFilter::Compressed => "Compressed folder",
        }
    }
    fn accepts(self, e: &Entry) -> bool {
        let x = e.ext.as_str();
        match self {
            KindFilter::Folder => e.is_dir,
            _ if e.is_dir => false,
            KindFilter::Document => matches!(
                x,
                ".doc" | ".docx" | ".pdf" | ".txt" | ".md" | ".rtf" | ".odt" | ".xls" | ".xlsx"
                    | ".ppt" | ".pptx" | ".csv" | ".ods" | ".odp" | ".epub" | ".xps"
            ),
            KindFilter::Picture => is_image_ext(x),
            KindFilter::Video => is_video_ext(x),
            KindFilter::Music => matches!(
                x,
                ".mp3" | ".flac" | ".wav" | ".m4a" | ".aac" | ".ogg" | ".wma" | ".opus" | ".aiff"
            ),
            KindFilter::Program => matches!(
                x,
                ".exe" | ".msi" | ".bat" | ".cmd" | ".ps1" | ".com" | ".msix" | ".appx" | ".scr"
            ),
            KindFilter::Compressed => matches!(
                x,
                ".zip" | ".7z" | ".rar" | ".tar" | ".gz" | ".tgz" | ".xz" | ".bz2" | ".cab" | ".iso"
                    | ".zst"
            ),
        }
    }
}

/// Explorer's size buckets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SizeFilter {
    Empty,
    Tiny,
    Small,
    Medium,
    Large,
    Huge,
    Gigantic,
}

impl SizeFilter {
    pub const ALL: [SizeFilter; 7] = [
        SizeFilter::Empty,
        SizeFilter::Tiny,
        SizeFilter::Small,
        SizeFilter::Medium,
        SizeFilter::Large,
        SizeFilter::Huge,
        SizeFilter::Gigantic,
    ];
    pub fn label(self) -> &'static str {
        match self {
            SizeFilter::Empty => "Empty (0 KB)",
            SizeFilter::Tiny => "Tiny (0 \u{2013} 16 KB)",
            SizeFilter::Small => "Small (16 KB \u{2013} 1 MB)",
            SizeFilter::Medium => "Medium (1 \u{2013} 128 MB)",
            SizeFilter::Large => "Large (128 MB \u{2013} 1 GB)",
            SizeFilter::Huge => "Huge (1 \u{2013} 4 GB)",
            SizeFilter::Gigantic => "Gigantic (> 4 GB)",
        }
    }
    fn accepts(self, size: u64) -> bool {
        const K: u64 = 1024;
        const M: u64 = K * K;
        const G: u64 = M * K;
        match self {
            SizeFilter::Empty => size == 0,
            SizeFilter::Tiny => size > 0 && size <= 16 * K,
            SizeFilter::Small => size > 16 * K && size <= M,
            SizeFilter::Medium => size > M && size <= 128 * M,
            SizeFilter::Large => size > 128 * M && size <= G,
            SizeFilter::Huge => size > G && size <= 4 * G,
            SizeFilter::Gigantic => size > 4 * G,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SearchOpts {
    /// Search the whole subtree (true) or only the folder itself.
    pub subfolders: bool,
    pub date: Option<DateFilter>,
    pub kind: Option<KindFilter>,
    pub size: Option<SizeFilter>,
    /// Include system files (system attribute, the Windows folder, Recycle Bin...).
    pub system: bool,
}

impl Default for SearchOpts {
    fn default() -> Self {
        SearchOpts {
            subfolders: true,
            date: None,
            kind: None,
            size: None,
            system: false,
        }
    }
}

/// A compiled filter: the date range is resolved once when the search starts.
pub struct SearchFilter {
    opts: SearchOpts,
    root: PathBuf,
    range: Option<(u64, u64)>,
    windir: String,
}

impl SearchOpts {
    pub fn compile(&self, root: &Path) -> SearchFilter {
        let windir = std::env::var("SystemRoot")
            .unwrap_or_else(|_| r"C:\Windows".into())
            .to_lowercase();
        SearchFilter {
            opts: *self,
            root: root.to_path_buf(),
            range: self.date.map(date_range),
            windir: format!("{}\\", windir.trim_end_matches('\\')),
        }
    }
}

impl SearchFilter {
    pub fn accepts(&self, e: &Entry) -> bool {
        let o = &self.opts;
        if !o.subfolders
            && !self.root.as_os_str().is_empty()
            && e.path.parent() != Some(self.root.as_path())
        {
            return false;
        }
        if !o.system && is_system(e, &self.windir) {
            return false;
        }
        if let Some(k) = o.kind {
            if !k.accepts(e) {
                return false;
            }
        }
        if let Some(s) = o.size {
            if e.is_dir || !s.accepts(e.size) {
                return false;
            }
        }
        if let Some((a, b)) = self.range {
            if e.modified < a || e.modified >= b {
                return false;
            }
        }
        true
    }
}

/// System files: the system attribute, anything under the Windows folder, and the system folders
/// at drive roots (Recycle Bin, System Volume Information...).
fn is_system(e: &Entry, windir: &str) -> bool {
    if e.attrs & FILE_ATTRIBUTE_SYSTEM.0 != 0 {
        return true;
    }
    let lower = e.path.to_string_lossy().to_lowercase();
    if lower.starts_with(windir) {
        return true;
    }
    let rest = lower.get(3..).unwrap_or("");
    [
        "$recycle.bin",
        "system volume information",
        "$windows.~bt",
        "$winreagent",
        "recovery\\",
    ]
    .iter()
    .any(|s| rest.starts_with(s))
}

/// [start, end) FILETIMEs (UTC) for a date filter, in local calendar terms (weeks start Monday).
fn date_range(f: DateFilter) -> (u64, u64) {
    use windows::Win32::Foundation::{FILETIME, SYSTEMTIME};
    use windows::Win32::System::SystemInformation::GetLocalTime;
    use windows::Win32::System::Time::{SystemTimeToFileTime, TzSpecificLocalTimeToSystemTime};
    const DAY: u64 = 864_000_000_000;
    let midnight = |y: u16, m: u16, d: u16| -> u64 {
        let local = SYSTEMTIME {
            wYear: y,
            wMonth: m,
            wDay: d,
            ..Default::default()
        };
        let mut utc = SYSTEMTIME::default();
        let mut ft = FILETIME::default();
        unsafe {
            if TzSpecificLocalTimeToSystemTime(None, &local, &mut utc).is_err() {
                utc = local;
            }
            let _ = SystemTimeToFileTime(&utc, &mut ft);
        }
        (ft.dwHighDateTime as u64) << 32 | ft.dwLowDateTime as u64
    };
    let now = unsafe { GetLocalTime() };
    let today = midnight(now.wYear, now.wMonth, now.wDay);
    let since_monday = (now.wDayOfWeek as u64 + 6) % 7;
    let week = today - since_monday * DAY;
    let month = midnight(now.wYear, now.wMonth, 1);
    let (py, pm) = if now.wMonth == 1 {
        (now.wYear - 1, 12)
    } else {
        (now.wYear, now.wMonth - 1)
    };
    let year = midnight(now.wYear, 1, 1);
    match f {
        DateFilter::Today => (today, u64::MAX),
        DateFilter::Yesterday => (today - DAY, today),
        DateFilter::ThisWeek => (week, u64::MAX),
        DateFilter::LastWeek => (week - 7 * DAY, week),
        DateFilter::ThisMonth => (month, u64::MAX),
        DateFilter::LastMonth => (midnight(py, pm, 1), month),
        DateFilter::ThisYear => (year, u64::MAX),
        DateFilter::LastYear => (midnight(now.wYear - 1, 1, 1), year),
    }
}

#[cfg(test)]
mod search_filter_tests {
    use super::*;

    fn e(path: &str, attrs: u32, size: u64) -> Entry {
        let p = PathBuf::from(path);
        let name = p.file_name().unwrap().to_string_lossy().into_owned();
        entry_from_parts(p, name, attrs, size, 1)
    }

    #[test]
    fn filters() {
        let root = Path::new(r"C:\Users\me");
        let default = SearchOpts::default().compile(Path::new(""));
        assert!(default.accepts(&e(r"C:\Users\me\a.txt", 0, 10)));
        assert!(!default.accepts(&e(r"C:\Windows\System32\a.dll", 0, 10)), "Windows folder");
        assert!(!default.accepts(&e(r"C:\Users\me\desktop.ini", FILE_ATTRIBUTE_SYSTEM.0, 10)));
        assert!(!default.accepts(&e(r"C:\$Recycle.Bin\x", 0, 10)));
        let system = SearchOpts { system: true, ..Default::default() }.compile(Path::new(""));
        assert!(system.accepts(&e(r"C:\Windows\System32\a.dll", 0, 10)));

        let here = SearchOpts { subfolders: false, ..Default::default() }.compile(root);
        assert!(here.accepts(&e(r"C:\Users\me\a.txt", 0, 1)));
        assert!(!here.accepts(&e(r"C:\Users\me\sub\a.txt", 0, 1)));

        let small = SearchOpts { size: Some(SizeFilter::Small), ..Default::default() }.compile(root);
        assert!(small.accepts(&e(r"C:\Users\me\a.bin", 0, 500 * 1024)));
        assert!(!small.accepts(&e(r"C:\Users\me\a.bin", 0, 2 * 1024 * 1024)));

        let pics = SearchOpts { kind: Some(KindFilter::Picture), ..Default::default() }.compile(root);
        assert!(pics.accepts(&e(r"C:\Users\me\p.JPG", 0, 1)));
        assert!(!pics.accepts(&e(r"C:\Users\me\p.txt", 0, 1)));

        let today = SearchOpts { date: Some(DateFilter::Today), ..Default::default() }.compile(root);
        assert!(!today.accepts(&e(r"C:\Users\me\old.txt", 0, 1)), "modified in 1601");
    }
}
