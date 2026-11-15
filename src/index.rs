//! Everything-style instant search without admin rights.
//!
//! Each fixed drive gets an in-memory index of every file and folder name: a compact node table
//! (parent/child links, packed name buffers). Queries scan the lowercase name buffer in parallel,
//! so a million names take a few milliseconds.
//!
//! * **Build:** a multi-threaded crawl with `FindFirstFileEx` (large fetch). (Everything reads the
//!   NTFS MFT instead, which needs administrator rights.)
//! * **Persist:** saved to `%LOCALAPPDATA%\Explorer26\index\<serial>.e26idx`, so later launches are
//!   instant. A background re-crawl then picks up changes made while the app was closed.
//! * **Live:** a recursive `ReadDirectoryChangesW` on the drive root marks folders dirty, and they
//!   are rescanned in batches about once a second.
//!
//! Query syntax: space-separated terms must all match (case-insensitive); `*`/`?` make a term a
//! whole-name wildcard; a term containing `\` matches against the full path.

use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant};

use windows::core::PCWSTR;
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FindClose, FindExInfoBasic, FindExSearchNameMatch, FindFirstFileExW,
    FindNextFileW, GetDriveTypeW, GetLogicalDrives, GetVolumeInformationW, ReadDirectoryChangesW,
    FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_LIST_DIRECTORY, FILE_NOTIFY_CHANGE_DIR_NAME, FILE_NOTIFY_CHANGE_FILE_NAME,
    FILE_NOTIFY_CHANGE_LAST_WRITE, FILE_NOTIFY_CHANGE_SIZE, FILE_NOTIFY_INFORMATION,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FIND_FIRST_EX_LARGE_FETCH,
    OPEN_EXISTING, WIN32_FIND_DATAW,
};

use crate::fs::Entry;
use crate::platform::{long_path, wide};

const NONE: u32 = u32::MAX;
/// Attribute bit Windows never sets; marks removed nodes.
const DELETED: u32 = 0x8000_0000;
const MAGIC: &[u8; 8] = b"E26INDEX";
const VERSION: u32 = 2;
/// Results beyond this are dropped (the status bar says so).
pub const MAX_RESULTS: usize = 200_000;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Node {
    parent: u32,
    first_child: u32,
    next_sibling: u32,
    name_off: u32,
    name_len: u16,
    _pad: u16,
    attrs: u32,
    size: u64,
    mtime: u64,
}

#[derive(Default)]
struct Volume {
    /// "C:\"
    root: String,
    serial: u32,
    nodes: Vec<Node>,
    names: String,
    deleted: usize,
}

fn lower_name(name: &str) -> String {
    if name.is_ascii() {
        name.to_ascii_lowercase()
    } else {
        name.to_lowercase()
    }
}

impl Volume {
    fn new(root: &str, serial: u32) -> Volume {
        let mut v = Volume {
            root: root.to_string(),
            serial,
            ..Default::default()
        };
        v.nodes.push(Node {
            parent: NONE,
            first_child: NONE,
            next_sibling: NONE,
            attrs: FILE_ATTRIBUTE_DIRECTORY.0,
            ..Default::default()
        });
        v
    }

    fn name(&self, id: u32) -> &str {
        let n = &self.nodes[id as usize];
        &self.names[n.name_off as usize..n.name_off as usize + n.name_len as usize]
    }

    fn add(&mut self, parent: u32, f: &Found) -> u32 {
        let id = self.nodes.len() as u32;
        let node = Node {
            parent,
            first_child: NONE,
            next_sibling: self.nodes[parent as usize].first_child,
            name_off: self.names.len() as u32,
            name_len: f.name.len().min(u16::MAX as usize) as u16,
            attrs: f.attrs,
            _pad: (!f.name.is_ascii()) as u16,
            size: f.size,
            mtime: f.mtime,
        };
        self.names.push_str(&f.name);
        self.nodes[parent as usize].first_child = id;
        self.nodes.push(node);
        id
    }

    fn children(&self, id: u32) -> Vec<u32> {
        let mut out = Vec::new();
        let mut c = self.nodes[id as usize].first_child;
        while c != NONE {
            if self.nodes[c as usize].attrs & DELETED == 0 {
                out.push(c);
            }
            c = self.nodes[c as usize].next_sibling;
        }
        out
    }

    fn is_dir(&self, id: u32) -> bool {
        self.nodes[id as usize].attrs & FILE_ATTRIBUTE_DIRECTORY.0 != 0
    }

    /// Full path of a node ("C:\Users\x").
    fn path(&self, id: u32) -> PathBuf {
        let mut parts = Vec::new();
        let mut c = id;
        while c != 0 && c != NONE {
            parts.push(self.name(c));
            c = self.nodes[c as usize].parent;
        }
        let mut s = self.root.clone();
        for (i, p) in parts.iter().rev().enumerate() {
            if i > 0 {
                s.push('\\');
            }
            s.push_str(p);
        }
        PathBuf::from(s)
    }

    fn lower_path(&self, id: u32) -> String {
        self.path(id).to_string_lossy().to_lowercase()
    }

    /// Node for a path on this volume.
    fn resolve(&self, path: &Path) -> Option<u32> {
        let s = path.to_string_lossy();
        let rest = s.get(self.root.len()..)?;
        if !s[..self.root.len().min(s.len())].eq_ignore_ascii_case(&self.root) {
            return None;
        }
        let mut id = 0u32;
        for part in rest.split('\\').filter(|p| !p.is_empty()) {
            let want = lower_name(part);
            id = self.children(id).into_iter().find(|&c| lower_name(self.name(c)) == want)?;
        }
        Some(id)
    }

    fn remove(&mut self, id: u32) {
        // Unlink from the parent's child list, then mark the whole subtree deleted.
        let parent = self.nodes[id as usize].parent;
        if parent != NONE {
            let mut prev = NONE;
            let mut c = self.nodes[parent as usize].first_child;
            while c != NONE && c != id {
                prev = c;
                c = self.nodes[c as usize].next_sibling;
            }
            if c == id {
                let next = self.nodes[id as usize].next_sibling;
                if prev == NONE {
                    self.nodes[parent as usize].first_child = next;
                } else {
                    self.nodes[prev as usize].next_sibling = next;
                }
            }
        }
        let mut stack = vec![id];
        while let Some(n) = stack.pop() {
            let mut c = self.nodes[n as usize].first_child;
            while c != NONE {
                stack.push(c);
                c = self.nodes[c as usize].next_sibling;
            }
            if self.nodes[n as usize].attrs & DELETED == 0 {
                self.nodes[n as usize].attrs |= DELETED;
                self.deleted += 1;
            }
        }
    }

    fn is_under(&self, mut id: u32, ancestor: u32) -> bool {
        while id != NONE {
            if id == ancestor {
                return true;
            }
            id = self.nodes[id as usize].parent;
        }
        false
    }

    fn live(&self) -> usize {
        self.nodes.len() - 1 - self.deleted
    }
}

// ---- crawling -----------------------------------------------------------------------------------

struct Found {
    name: String,
    attrs: u32,
    size: u64,
    mtime: u64,
}

fn list_raw(dir: &Path) -> Option<Vec<Found>> {
    let pattern = long_path(&dir.join("*"));
    let mut out = Vec::new();
    unsafe {
        let mut d = WIN32_FIND_DATAW::default();
        let h = FindFirstFileExW(
            PCWSTR(pattern.as_ptr()),
            FindExInfoBasic,
            &mut d as *mut _ as _,
            FindExSearchNameMatch,
            None,
            FIND_FIRST_EX_LARGE_FETCH,
        )
        .ok()?;
        loop {
            let len = d.cFileName.iter().position(|&c| c == 0).unwrap_or(260);
            let name = &d.cFileName[..len];
            if !(name == [46] || name == [46, 46]) {
                out.push(Found {
                    name: String::from_utf16_lossy(name),
                    attrs: d.dwFileAttributes,
                    size: (d.nFileSizeHigh as u64) << 32 | d.nFileSizeLow as u64,
                    mtime: (d.ftLastWriteTime.dwHighDateTime as u64) << 32
                        | d.ftLastWriteTime.dwLowDateTime as u64,
                });
            }
            if FindNextFileW(h, &mut d).is_err() {
                break;
            }
        }
        let _ = FindClose(h);
    }
    Some(out)
}

/// Junctions and symlinks would duplicate (or loop) the tree.
fn descend(f: &Found) -> bool {
    f.attrs & FILE_ATTRIBUTE_DIRECTORY.0 != 0 && f.attrs & FILE_ATTRIBUTE_REPARSE_POINT.0 == 0
}

/// Crawls a whole drive with several threads.
fn crawl(root: &str, serial: u32, progress: &AtomicUsize) -> Volume {
    let vol = Mutex::new(Volume::new(root, serial));
    let queue = Mutex::new((vec![(0u32, PathBuf::from(root))], 0usize));
    let cv = Condvar::new();
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).clamp(2, 8);
    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| loop {
                let (id, path) = {
                    let mut q = queue.lock().unwrap();
                    loop {
                        if let Some(job) = q.0.pop() {
                            q.1 += 1;
                            break job;
                        }
                        if q.1 == 0 {
                            cv.notify_all();
                            return;
                        }
                        q = cv.wait(q).unwrap();
                    }
                };
                let found = list_raw(&path).unwrap_or_default();
                let mut dirs = Vec::new();
                {
                    let mut v = vol.lock().unwrap();
                    for f in &found {
                        let child = v.add(id, f);
                        if descend(f) {
                            dirs.push((child, path.join(&f.name)));
                        }
                    }
                }
                progress.fetch_add(found.len(), Ordering::Relaxed);
                let mut q = queue.lock().unwrap();
                q.0.extend(dirs);
                q.1 -= 1;
                cv.notify_all();
            });
        }
    });
    vol.into_inner().unwrap()
}

/// Brings one folder's children in line with the disk (live updates).
fn rescan(v: &mut Volume, id: u32, path: &Path) {
    let Some(found) = list_raw(path) else {
        if id != 0 {
            v.remove(id);
        }
        return;
    };
    let mut existing: HashMap<String, u32> =
        v.children(id).into_iter().map(|c| (lower_name(v.name(c)), c)).collect();
    for f in &found {
        let key = lower_name(&f.name);
        match existing.remove(&key) {
            Some(c) if v.is_dir(c) == (f.attrs & FILE_ATTRIBUTE_DIRECTORY.0 != 0) => {
                let n = &mut v.nodes[c as usize];
                n.attrs = f.attrs;
                n.size = f.size;
                n.mtime = f.mtime;
                // Renamed with different case: keep the node, refresh its display name.
                if v.name(c) != f.name {
                    let off = v.names.len() as u32;
                    v.names.push_str(&f.name);
                    let n = &mut v.nodes[c as usize];
                    n.name_off = off;
                    n.name_len = f.name.len().min(u16::MAX as usize) as u16;
                }
            }
            other => {
                if let Some(c) = other {
                    v.remove(c);
                }
                let c = v.add(id, f);
                if descend(f) {
                    add_subtree(v, c, &path.join(&f.name));
                }
            }
        }
    }
    for (_, c) in existing {
        v.remove(c);
    }
}

fn add_subtree(v: &mut Volume, id: u32, path: &Path) {
    let mut stack = vec![(id, path.to_path_buf())];
    while let Some((id, p)) = stack.pop() {
        for f in list_raw(&p).unwrap_or_default() {
            let c = v.add(id, &f);
            if descend(&f) {
                stack.push((c, p.join(&f.name)));
            }
        }
    }
}

// ---- persistence --------------------------------------------------------------------------------

fn cache_path(serial: u32) -> PathBuf {
    crate::platform::app_data_dir()
        .join("index")
        .join(format!("{serial:08x}.e26idx"))
}

fn save(v: &Volume) -> std::io::Result<()> {
    let p = cache_path(v.serial);
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d)?;
    }
    let tmp = p.with_extension("tmp");
    {
        let mut f = std::io::BufWriter::new(std::fs::File::create(&tmp)?);
        f.write_all(MAGIC)?;
        for x in [
            VERSION,
            v.serial,
            v.nodes.len() as u32,
            v.names.len() as u32,
            0,
            v.deleted as u32,
            v.root.len() as u32,
        ] {
            f.write_all(&x.to_le_bytes())?;
        }
        f.write_all(v.root.as_bytes())?;
        let bytes = unsafe {
            std::slice::from_raw_parts(
                v.nodes.as_ptr() as *const u8,
                v.nodes.len() * std::mem::size_of::<Node>(),
            )
        };
        f.write_all(bytes)?;
        f.write_all(v.names.as_bytes())?;
        f.flush()?;
    }
    std::fs::rename(&tmp, &p)
}

fn load(serial: u32) -> Option<Volume> {
    let mut f = std::io::BufReader::new(std::fs::File::open(cache_path(serial)).ok()?);
    let mut magic = [0u8; 8];
    f.read_exact(&mut magic).ok()?;
    if &magic != MAGIC {
        return None;
    }
    let mut hdr = [0u32; 7];
    for h in &mut hdr {
        let mut b = [0u8; 4];
        f.read_exact(&mut b).ok()?;
        *h = u32::from_le_bytes(b);
    }
    let [version, ser, n, names_len, _, deleted, root_len] = hdr;
    if version != VERSION || ser != serial || n == 0 {
        return None;
    }
    let mut root = vec![0u8; root_len as usize];
    f.read_exact(&mut root).ok()?;
    let mut nodes = vec![Node::default(); n as usize];
    let bytes = unsafe {
        std::slice::from_raw_parts_mut(
            nodes.as_mut_ptr() as *mut u8,
            nodes.len() * std::mem::size_of::<Node>(),
        )
    };
    f.read_exact(bytes).ok()?;
    let mut names = vec![0u8; names_len as usize];
    f.read_exact(&mut names).ok()?;
    let v = Volume {
        root: String::from_utf8(root).ok()?,
        serial,
        nodes,
        names: String::from_utf8(names).ok()?,
        deleted: deleted as usize,
    };
    // Reject anything inconsistent rather than index out of bounds later.
    let ok = v.nodes.iter().all(|n| {
        (n.name_off as usize + n.name_len as usize) <= v.names.len()
            && v.names.is_char_boundary(n.name_off as usize)
            && v.names.is_char_boundary(n.name_off as usize + n.name_len as usize)
            && (n.parent == NONE || (n.parent as usize) < v.nodes.len())
    });
    ok.then_some(v)
}

// ---- the indexer --------------------------------------------------------------------------------

struct Drive {
    root: String,
    volume: RwLock<Option<Volume>>,
    /// Items found by the first crawl so far (status bar).
    progress: AtomicUsize,
    /// A full crawl is running (first build or refresh).
    crawling: AtomicBool,
    pending: Mutex<HashSet<PathBuf>>,
    overflow: AtomicBool,
    dirty: AtomicBool,
}

struct Indexer {
    drives: Vec<Arc<Drive>>,
}

static INDEXER: OnceLock<Indexer> = OnceLock::new();

fn fixed_drives() -> Vec<(String, u32)> {
    let mask = unsafe { GetLogicalDrives() };
    (0..26u32)
        .filter(|i| mask & (1 << i) != 0)
        .filter_map(|i| {
            let root = format!("{}:\\", (b'A' + i as u8) as char);
            let w = wide(&root);
            // DRIVE_FIXED = 3
            if unsafe { GetDriveTypeW(PCWSTR(w.as_ptr())) } != 3 {
                return None;
            }
            let mut serial = 0u32;
            unsafe {
                GetVolumeInformationW(PCWSTR(w.as_ptr()), None, Some(&mut serial), None, None, None)
                    .ok()?;
            }
            Some((root, serial))
        })
        .collect()
}

/// Starts indexing every fixed drive in the background (once per process).
pub fn start() {
    INDEXER.get_or_init(|| {
        let drives: Vec<Arc<Drive>> = fixed_drives()
            .into_iter()
            .map(|(root, serial)| {
                let d = Arc::new(Drive {
                    root: root.clone(),
                    volume: RwLock::new(None),
                    progress: AtomicUsize::new(0),
                    crawling: AtomicBool::new(true),
                    pending: Mutex::new(HashSet::new()),
                    overflow: AtomicBool::new(false),
                    dirty: AtomicBool::new(false),
                });
                let d2 = d.clone();
                std::thread::spawn(move || run_drive(d2, serial));
                d
            })
            .collect();
        Indexer { drives }
    });
}

fn run_drive(d: Arc<Drive>, serial: u32) {
    lower_priority();
    // Watch first, so nothing that changes during the crawl is missed.
    let d2 = d.clone();
    std::thread::spawn(move || watch_drive(d2));
    if let Some(v) = load(serial) {
        d.progress.store(v.live(), Ordering::Relaxed);
        *d.volume.write().unwrap() = Some(v);
    }
    loop {
        // Full (re)crawl: initially, after a watcher overflow, or to catch up with a cached index.
        d.crawling.store(true, Ordering::Relaxed);
        d.progress.store(0, Ordering::Relaxed);
        let fresh = crawl(&d.root, serial, &d.progress);
        d.overflow.store(false, Ordering::Relaxed);
        let _ = save(&fresh);
        *d.volume.write().unwrap() = Some(fresh);
        d.crawling.store(false, Ordering::Relaxed);
        let mut last_save = Instant::now();
        // Apply live changes until a full rescan is needed.
        loop {
            std::thread::sleep(Duration::from_millis(1000));
            if d.overflow.load(Ordering::Relaxed) {
                break;
            }
            let dirs: Vec<PathBuf> = std::mem::take(&mut *d.pending.lock().unwrap()).into_iter().collect();
            if !dirs.is_empty() {
                apply(&d, dirs);
                d.dirty.store(true, Ordering::Relaxed);
            }
            if d.dirty.load(Ordering::Relaxed) && last_save.elapsed() > Duration::from_secs(300) {
                if let Some(v) = d.volume.read().unwrap().as_ref() {
                    let _ = save(v);
                }
                d.dirty.store(false, Ordering::Relaxed);
                last_save = Instant::now();
            }
        }
    }
}

fn lower_priority() {
    use windows::Win32::System::Threading::{
        GetCurrentThread, SetThreadPriority, THREAD_MODE_BACKGROUND_BEGIN,
    };
    unsafe {
        // Background mode also lowers I/O priority, so indexing never makes the PC sluggish.
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_BEGIN);
    }
}

fn apply(d: &Drive, dirs: Vec<PathBuf>) {
    // List outside the write lock where possible: resolve under a read lock first.
    let mut guard = d.volume.write().unwrap();
    let Some(v) = guard.as_mut() else { return };
    let mut done = HashSet::new();
    for dir in dirs {
        // The folder itself may be gone or new: rescan the nearest ancestor that we know.
        let mut p = dir.clone();
        let id = loop {
            if let Some(id) = v.resolve(&p) {
                break id;
            }
            match p.parent() {
                Some(par) => p = par.to_path_buf(),
                None => break 0,
            }
        };
        if done.insert(id) {
            rescan(v, id, &p);
        }
    }
    // Rebuild from scratch when too much is dead weight.
    if v.deleted > 100_000 && v.deleted > v.nodes.len() / 3 {
        d.overflow.store(true, Ordering::Relaxed);
    }
}

fn watch_drive(d: Arc<Drive>) {
    lower_priority();
    unsafe {
        let Ok(h) = CreateFileW(
            PCWSTR(wide(&d.root).as_ptr()),
            FILE_LIST_DIRECTORY.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            None,
        ) else {
            return;
        };
        let mut buf = vec![0u32; 64 * 1024];
        let filter = FILE_NOTIFY_CHANGE_FILE_NAME
            | FILE_NOTIFY_CHANGE_DIR_NAME
            | FILE_NOTIFY_CHANGE_SIZE
            | FILE_NOTIFY_CHANGE_LAST_WRITE;
        let own = crate::platform::app_data_dir().to_string_lossy().to_lowercase();
        loop {
            let mut n = 0u32;
            if ReadDirectoryChangesW(
                h,
                buf.as_mut_ptr() as _,
                (buf.len() * 4) as u32,
                true,
                filter,
                Some(&mut n),
                None,
                None,
            )
            .is_err()
            {
                break;
            }
            if n == 0 {
                d.overflow.store(true, Ordering::Relaxed);
                continue;
            }
            let base = buf.as_ptr() as *const u8;
            let mut off = 0usize;
            let mut pending = d.pending.lock().unwrap();
            loop {
                let info = &*(base.add(off) as *const FILE_NOTIFY_INFORMATION);
                let len = info.FileNameLength as usize / 2;
                let rel = String::from_utf16_lossy(std::slice::from_raw_parts(info.FileName.as_ptr(), len));
                let full = PathBuf::from(&d.root).join(&rel);
                // Our own cache/index writes would otherwise keep the indexer busy.
                if !full.to_string_lossy().to_lowercase().starts_with(&own) {
                    if let Some(parent) = full.parent() {
                        pending.insert(parent.to_path_buf());
                    }
                }
                if info.NextEntryOffset == 0 {
                    break;
                }
                off += info.NextEntryOffset as usize;
            }
        }
        let _ = CloseHandle(h);
    }
}

// ---- querying -----------------------------------------------------------------------------------

enum Term {
    /// Lowercase needle and its case-fold mask (0x20 for ASCII letters).
    Sub(String, Vec<u8>),
    Wild(String),
    Path(String),
}

fn parse(query: &str) -> Vec<Term> {
    query
        .split_whitespace()
        .map(|t| {
            let t = t.to_lowercase();
            if t.contains('\\') {
                Term::Path(t)
            } else if t.contains('*') || t.contains('?') {
                Term::Wild(t)
            } else {
                let mask = t.bytes().map(|b| if b.is_ascii_lowercase() { 0x20 } else { 0 }).collect();
                Term::Sub(t, mask)
            }
        })
        .collect()
}

fn matches(v: &Volume, id: u32, terms: &[Term]) -> bool {
    let name = v.name(id);
    let ascii = v.nodes[id as usize]._pad == 0;
    for t in terms {
        let ok = match t {
            Term::Sub(s, mask) if ascii => contains_fold(name.as_bytes(), s.as_bytes(), mask),
            Term::Sub(s, _) => name.to_lowercase().contains(s.as_str()),
            Term::Wild(w) if ascii => wild(w.as_bytes(), name.as_bytes()),
            Term::Wild(w) => wild(w.as_bytes(), name.to_lowercase().as_bytes()),
            Term::Path(_) => true,
        };
        if !ok {
            return false;
        }
    }
    let paths: Vec<&String> = terms
        .iter()
        .filter_map(|t| match t {
            Term::Path(p) => Some(p),
            _ => None,
        })
        .collect();
    if paths.is_empty() {
        return true;
    }
    let full = v.lower_path(id);
    paths.iter().all(|p| full.contains(p.as_str()))
}

fn search_volume(v: &Volume, scope: Option<u32>, terms: &[Term], cancel: &AtomicBool) -> Vec<u32> {
    let n = v.nodes.len();
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).clamp(1, 8);
    let chunk = n.div_ceil(threads).max(1);
    let mut out: Vec<u32> = Vec::new();
    std::thread::scope(|s| {
        let handles: Vec<_> = (0..threads)
            .map(|t| {
                s.spawn(move || {
                    let mut hits = Vec::new();
                    let start = (t * chunk).max(1);
                    let end = ((t + 1) * chunk).min(n);
                    for id in start..end {
                        if id % 65536 == 0 && cancel.load(Ordering::Relaxed) {
                            break;
                        }
                        let node = &v.nodes[id];
                        if node.attrs & DELETED != 0 {
                            continue;
                        }
                        if matches(v, id as u32, terms)
                            && scope.is_none_or(|sc| v.is_under(id as u32, sc) && id as u32 != sc)
                        {
                            hits.push(id as u32);
                            if hits.len() >= MAX_RESULTS {
                                break;
                            }
                        }
                    }
                    hits
                })
            })
            .collect();
        for h in handles {
            out.extend(h.join().unwrap_or_default());
        }
    });
    out.truncate(MAX_RESULTS);
    out
}

/// Searches the index. `scope` empty = all drives. Returns false if the scope isn't indexed yet
/// (the caller should fall back to walking folders).
pub fn search(scope: &Path, query: &str, cancel: &AtomicBool, emit: &mut dyn FnMut(Vec<Entry>)) -> bool {
    let Some(ix) = INDEXER.get() else {
        return false;
    };
    let terms = parse(query);
    if terms.is_empty() {
        return true;
    }
    let all = scope.as_os_str().is_empty();
    let scope_s = scope.to_string_lossy().to_lowercase();
    let drives: Vec<&Arc<Drive>> = ix
        .drives
        .iter()
        .filter(|d| all || scope_s.starts_with(&d.root.to_lowercase()))
        .collect();
    if drives.is_empty() {
        return false;
    }
    // Every drive in scope must have an index (a cached one counts while it refreshes).
    if drives.iter().any(|d| d.volume.read().unwrap().is_none()) {
        return false;
    }
    let mut total = 0usize;
    for d in drives {
        let guard = d.volume.read().unwrap();
        let Some(v) = guard.as_ref() else { continue };
        let scope_id = if all {
            None
        } else {
            match v.resolve(scope) {
                Some(id) => Some(id),
                // Folder not (yet) in the index, e.g. just created: walk it instead.
                None => return false,
            }
        };
        let ids = search_volume(v, scope_id, &terms, cancel);
        // Build entries in batches so results stream into the view.
        for chunk in ids.chunks(5000) {
            if cancel.load(Ordering::Relaxed) || total >= MAX_RESULTS {
                return true;
            }
            let entries: Vec<Entry> = chunk
                .iter()
                .map(|&id| {
                    let n = &v.nodes[id as usize];
                    crate::fs::entry_from_parts(v.path(id), v.name(id).to_string(), n.attrs, n.size, n.mtime)
                })
                .collect();
            total += entries.len();
            emit(entries);
        }
    }
    true
}

/// "Indexing C:\ (123,456 items)" while a first crawl runs, for the status bar.
pub fn status() -> Option<String> {
    let ix = INDEXER.get()?;
    let busy: Vec<String> = ix
        .drives
        .iter()
        .filter(|d| d.crawling.load(Ordering::Relaxed) && d.volume.read().unwrap().is_none())
        .map(|d| format!("{} ({} items)", d.root, group(d.progress.load(Ordering::Relaxed))))
        .collect();
    (!busy.is_empty()).then(|| format!("Indexing {}", busy.join(", ")))
}

fn group(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn found(name: &str, dir: bool) -> Found {
        Found {
            name: name.into(),
            attrs: if dir { FILE_ATTRIBUTE_DIRECTORY.0 } else { 0 },
            size: 1,
            mtime: 1,
        }
    }

    #[test]
    fn tree_query_and_remove() {
        let mut v = Volume::new("C:\\", 1);
        let users = v.add(0, &found("Users", true));
        let sid = v.add(users, &found("Sid", true));
        let a = v.add(sid, &found("Report Final.PDF", false));
        let _b = v.add(sid, &found("notes.txt", false));
        assert_eq!(v.path(a), PathBuf::from("C:\\Users\\Sid\\Report Final.PDF"));
        assert_eq!(v.resolve(Path::new("c:\\users\\sid")), Some(sid));
        let no = AtomicBool::new(false);
        let q = |v: &Volume, s: &str, scope| search_volume(v, scope, &parse(s), &no);
        assert_eq!(q(&v, "report pdf", None), vec![a]);
        assert_eq!(q(&v, "*.pdf", None), vec![a]);
        assert_eq!(q(&v, "users\\ final", None), vec![a]);
        assert_eq!(q(&v, "final", Some(users)), vec![a]);
        assert!(q(&v, "missing", None).is_empty());
        v.remove(sid);
        assert!(q(&v, "report", None).is_empty());
        assert_eq!(v.resolve(Path::new("C:\\Users\\Sid")), None);
    }

    /// `cargo test --release -- --ignored --nocapture bench_c`
    #[test]
    #[ignore]
    fn bench_c() {
        let t = Instant::now();
        let p = AtomicUsize::new(0);
        let v = crawl("C:\\", 0, &p);
        println!("crawl: {} items in {:?}", v.live(), t.elapsed());
        let no = AtomicBool::new(false);
        for q in ["note", "*.dll", r"system32\ drivers", "zzzqqq"] {
            let t = Instant::now();
            let hits = search_volume(&v, None, &parse(q), &no);
            println!("query {q:>22}: {:>7} hits in {:?}", hits.len(), t.elapsed());
        }
        let t = Instant::now();
        save(&Volume { serial: 0xBEEF_0000, ..v }).unwrap();
        println!("save: {:?}", t.elapsed());
        let t = Instant::now();
        let l = load(0xBEEF_0000).unwrap();
        println!("load: {} nodes in {:?}", l.nodes.len(), t.elapsed());
        let _ = std::fs::remove_file(cache_path(0xBEEF_0000));
    }

    #[test]
    fn persist_roundtrip() {
        let mut v = Volume::new("C:\\", 0xDEAD_0001);
        let d = v.add(0, &found("Ünïcode dir", true));
        v.add(d, &found("file.rs", false));
        save(&v).unwrap();
        let l = load(0xDEAD_0001).unwrap();
        assert_eq!(l.nodes.len(), 3);
        assert_eq!(l.path(2), PathBuf::from("C:\\Ünïcode dir\\file.rs"));
        let _ = std::fs::remove_file(cache_path(0xDEAD_0001));
    }
}

/// Allocation-free `*`/`?` match over UTF-8 bytes (`?` = one character), with fast paths for
/// the common `*.ext` and `prefix*` shapes.
fn wild(p: &[u8], s: &[u8]) -> bool {
    let stars = p.iter().filter(|&&c| c == b'*').count();
    if stars == 1 && !p.contains(&b'?') {
        if p[0] == b'*' {
            let t = &p[1..];
            return s.len() >= t.len() && s[s.len() - t.len()..].eq_ignore_ascii_case(t);
        }
        if p[p.len() - 1] == b'*' {
            let t = &p[..p.len() - 1];
            return s.len() >= t.len() && s[..t.len()].eq_ignore_ascii_case(t);
        }
    }
    let char_len = |i: usize| -> usize {
        let mut j = i + 1;
        while j < s.len() && s[j] & 0xC0 == 0x80 {
            j += 1;
        }
        j - i
    };
    let (mut pi, mut si) = (0usize, 0usize);
    let (mut star, mut mark) = (usize::MAX, 0usize);
    while si < s.len() {
        if pi < p.len() && p[pi] == b'?' {
            pi += 1;
            si += char_len(si);
        } else if pi < p.len() && p[pi] != b'*' && p[pi] == s[si].to_ascii_lowercase() {
            pi += 1;
            si += 1;
        } else if pi < p.len() && p[pi] == b'*' {
            star = pi;
            mark = si;
            pi += 1;
        } else if star != usize::MAX {
            pi = star + 1;
            mark += char_len(mark);
            si = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }
    pi == p.len()
}

#[cfg(test)]
mod wild_tests {
    use super::wild;

    #[test]
    fn wildcards() {
        assert!(wild(b"*.dll", b"kernel32.dll"));
        assert!(!wild(b"*.dll", b"kernel32.dll.mui"));
        assert!(wild(b"ker*", b"kernel32.dll"));
        assert!(wild(b"k?rnel*.d?l", b"kernel32.dll"));
        assert!(wild("?ber.txt".as_bytes(), "über.txt".as_bytes()));
        assert!(!wild(b"a*b*c", b"aXbY"));
        assert!(wild(b"*", b""));
    }

    #[test]
    fn fold() {
        let m = |s: &str| s.bytes().map(|b| if b.is_ascii_lowercase() { 0x20 } else { 0 }).collect::<Vec<u8>>();
        assert!(super::contains_fold(b"Report-Final.PDF", b"final.pdf", &m("final.pdf")));
        assert!(!super::contains_fold(b"Report[Final", b"report{", &m("report{")));
        assert!(super::contains_fold(b"A_B", b"a_b", &m("a_b")));
    }
}

/// Case-insensitive substring test against a lowercase needle, without allocating for ASCII
/// names (nearly all of them).
#[allow(dead_code)]
fn contains_ci(hay: &str, needle: &str) -> bool {
    let (h, n) = (hay.as_bytes(), needle.as_bytes());
    if n.is_empty() {
        return true;
    }
    if !hay.is_ascii() {
        return hay.to_lowercase().contains(needle);
    }
    if n.len() > h.len() {
        return false;
    }
    let (first, first_up) = (n[0], n[0].to_ascii_uppercase());
    (0..=h.len() - n.len()).any(|i| {
        let c = h[i];
        (c == first || c == first_up)
            && h[i + 1..i + n.len()]
                .iter()
                .zip(&n[1..])
                .all(|(a, b)| a.to_ascii_lowercase() == *b)
    })
}

/// ASCII case-insensitive substring search: `(byte | mask) == needle` folds letters only.
#[inline]
fn contains_fold(h: &[u8], n: &[u8], mask: &[u8]) -> bool {
    if n.len() > h.len() {
        return false;
    }
    let (n0, m0) = (n[0], mask[0]);
    let last = h.len() - n.len();
    let mut i = 0;
    while i <= last {
        if h[i] | m0 == n0 {
            let mut k = 1;
            while k < n.len() && h[i + k] | mask[k] == n[k] {
                k += 1;
            }
            if k == n.len() {
                return true;
            }
        }
        i += 1;
    }
    false
}
