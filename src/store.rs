//! `thumbs26.db`: one small file per folder holding that folder's view settings and thumbnails.
//!
//! It never touches Windows' own `Thumbs.db` / `thumbcache_*.db`. The file is an append-only log
//! of CRC-checked records, so a crash or power loss can only lose the unfinished tail:
//!
//! ```text
//! header  : "THUMBS26" | version u16 | flags u16 | reserved u32 | created u64 | reserved u64   (32 bytes)
//! record  : kind u8 | key_len u16 | payload_len u32 | crc32 u32 | key | payload
//!   kind 1 FOLDER   key = folder path (UTF-8), lets a central cache map files back to folders
//!   kind 2 SETTINGS payload = "key=value\n" lines; the newest record wins
//!   kind 3 THUMB    key = file name; payload = size u64 | mtime u64 | max_dim u16 | w u16 | h u16 | fmt u8 | data
//!   kind 4 DELETE   key = file name (tombstone)
//! ```
//!
//! Replaced records become dead space; [`ThumbDb::compact_if_needed`] rewrites the file through a
//! temporary file and an atomic rename once dead bytes outweigh live ones.
//! Where the file lives (inside the folder or in the per-user cache) is decided by `thumbs.rs`.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

pub const FILE_NAME: &str = "thumbs26.db";
const MAGIC: &[u8; 8] = b"THUMBS26";
const VERSION: u16 = 1;
const HEADER_LEN: u64 = 32;
const REC_HEADER_LEN: u64 = 11;

const K_FOLDER: u8 = 1;
const K_SETTINGS: u8 = 2;
const K_THUMB: u8 = 3;
const K_DELETE: u8 = 4;

const THUMB_META_LEN: usize = 8 + 8 + 2 + 2 + 2 + 1;

/// Stored image encoding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThumbFmt {
    /// The shell has no thumbnail for this file (cached so we don't ask again).
    None = 0,
    Jpeg = 1,
    Png = 2,
}

impl ThumbFmt {
    fn from_u8(v: u8) -> Option<ThumbFmt> {
        match v {
            0 => Some(ThumbFmt::None),
            1 => Some(ThumbFmt::Jpeg),
            2 => Some(ThumbFmt::Png),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ThumbMeta {
    pub size: u64,
    pub mtime: u64,
    pub max_dim: u16,
    pub w: u16,
    pub h: u16,
    pub fmt: ThumbFmt,
    data_offset: u64,
    data_len: u32,
    record_len: u64,
}

pub struct ThumbDb {
    path: PathBuf,
    folder: String,
    file: Option<File>,
    index: HashMap<String, ThumbMeta>,
    settings: Option<String>,
    settings_len: u64,
    /// Offset where the next record goes (the end of the last valid record).
    end: u64,
    live: u64,
    dead: u64,
    /// The file exists but is not ours (bad magic or newer version): never write to it.
    foreign: bool,
    folder_written: bool,
    file_is_rw: bool,
    /// Mark a newly created file hidden+system (like Thumbs.db) when it lives inside the folder.
    hidden: bool,
}

fn key_of(name: &str) -> String {
    name.to_lowercase()
}

impl ThumbDb {
    /// Opens (or prepares to lazily create) the database at `path` for `folder`.
    /// Never fails: an unreadable or corrupt file behaves like an empty one.
    pub fn open(path: &Path, folder: &str) -> ThumbDb {
        let mut db = ThumbDb {
            path: path.to_path_buf(),
            folder: folder.to_string(),
            file: None,
            index: HashMap::new(),
            settings: None,
            settings_len: 0,
            end: 0,
            live: 0,
            dead: 0,
            foreign: false,
            folder_written: false,
            file_is_rw: false,
            hidden: false,
        };
        if let Ok(f) = File::open(path) {
            if db.scan(f).is_err() {
                // Unreadable header: leave the file alone.
                db.foreign = true;
            }
        }
        db
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn set_hidden(&mut self, hidden: bool) {
        self.hidden = hidden;
    }

    fn scan(&mut self, f: File) -> io::Result<()> {
        let len = f.metadata()?.len();
        let mut r = BufReader::with_capacity(64 * 1024, f);
        let mut header = [0u8; HEADER_LEN as usize];
        if r.read_exact(&mut header).is_err() {
            // Empty/truncated file: treat as new (it will be rewritten from scratch).
            self.end = 0;
            return Ok(());
        }
        if &header[0..8] != MAGIC {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "not a thumbs26 file"));
        }
        let version = u16::from_le_bytes([header[8], header[9]]);
        if version > VERSION {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "newer version"));
        }
        let mut pos = HEADER_LEN;
        let mut rec = [0u8; REC_HEADER_LEN as usize];
        let mut key = Vec::new();
        let mut buf = Vec::new();
        loop {
            if pos + REC_HEADER_LEN > len || r.read_exact(&mut rec).is_err() {
                break;
            }
            let kind = rec[0];
            let key_len = u16::from_le_bytes([rec[1], rec[2]]) as u64;
            let payload_len = u32::from_le_bytes([rec[3], rec[4], rec[5], rec[6]]) as u64;
            let crc = u32::from_le_bytes([rec[7], rec[8], rec[9], rec[10]]);
            let total = REC_HEADER_LEN + key_len + payload_len;
            if pos + total > len || !(K_FOLDER..=K_DELETE).contains(&kind) {
                break;
            }
            key.resize(key_len as usize, 0);
            if r.read_exact(&mut key).is_err() {
                break;
            }
            // Thumb payloads are only partially read during the scan (metadata); the CRC of the
            // image data is verified when it is read back.
            let meta_only = kind == K_THUMB && payload_len as usize >= THUMB_META_LEN;
            let read_len = if meta_only {
                THUMB_META_LEN
            } else {
                payload_len as usize
            };
            buf.resize(read_len, 0);
            if r.read_exact(&mut buf).is_err() {
                break;
            }
            if !meta_only && crc32(&[&key, &buf]) != crc {
                break;
            }
            if meta_only {
                let skip = payload_len as i64 - THUMB_META_LEN as i64;
                if r.seek_relative(skip).is_err() {
                    break;
                }
            }
            let Ok(key_str) = String::from_utf8(key.clone()) else {
                break;
            };
            match kind {
                K_FOLDER => {
                    self.folder_written = key_str.eq_ignore_ascii_case(&self.folder);
                    self.live += total;
                }
                K_SETTINGS => {
                    self.dead += self.settings_len;
                    self.settings_len = total;
                    self.live += total;
                    self.settings = String::from_utf8(buf.clone()).ok();
                }
                K_THUMB => {
                    if !meta_only {
                        break;
                    }
                    let m = &buf[..];
                    let Some(fmt) = ThumbFmt::from_u8(m[22]) else {
                        break;
                    };
                    let meta = ThumbMeta {
                        size: u64::from_le_bytes(m[0..8].try_into().unwrap()),
                        mtime: u64::from_le_bytes(m[8..16].try_into().unwrap()),
                        max_dim: u16::from_le_bytes([m[16], m[17]]),
                        w: u16::from_le_bytes([m[18], m[19]]),
                        h: u16::from_le_bytes([m[20], m[21]]),
                        fmt,
                        data_offset: pos + REC_HEADER_LEN + key_len + THUMB_META_LEN as u64,
                        data_len: (payload_len - THUMB_META_LEN as u64) as u32,
                        record_len: total,
                    };
                    self.live += total;
                    if let Some(old) = self.index.insert(key_of(&key_str), meta) {
                        self.live -= old.record_len;
                        self.dead += old.record_len;
                    }
                }
                K_DELETE => {
                    self.dead += total;
                    if let Some(old) = self.index.remove(&key_of(&key_str)) {
                        self.live -= old.record_len;
                        self.dead += old.record_len;
                    }
                }
                _ => break,
            }
            pos += total;
        }
        self.end = pos;
        Ok(())
    }

    pub fn settings(&self) -> Option<&str> {
        self.settings.as_deref()
    }

    pub fn len(&self) -> usize {
        self.index.len()
    }

    /// A stored thumbnail for `name` that is still valid for the file's size and mtime and at least
    /// `min_dim` pixels large (or a cached "no thumbnail" marker).
    pub fn lookup(&self, name: &str, size: u64, mtime: u64, min_dim: u16) -> Option<ThumbMeta> {
        let m = self.index.get(&key_of(name))?;
        (m.size == size && m.mtime == mtime && (m.fmt == ThumbFmt::None || m.max_dim >= min_dim))
            .then_some(*m)
    }

    /// Reads the encoded image bytes of a thumbnail found by [`lookup`](Self::lookup).
    pub fn read_blob(&mut self, meta: &ThumbMeta) -> io::Result<Vec<u8>> {
        let f = self.file_for_read()?;
        f.seek(SeekFrom::Start(meta.data_offset))?;
        let mut data = vec![0u8; meta.data_len as usize];
        f.read_exact(&mut data)?;
        Ok(data)
    }

    fn file_for_read(&mut self) -> io::Result<&mut File> {
        if self.file.is_none() {
            self.file = Some(File::open(&self.path)?);
            self.file_is_rw = false;
        }
        Ok(self.file.as_mut().unwrap())
    }

    /// Opens the file for writing, creating it (with header) if needed.
    fn writer(&mut self) -> io::Result<()> {
        if self.foreign {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "foreign file"));
        }
        if self.file_is_rw {
            return Ok(());
        }
        let created = !self.path.exists();
        let mut f = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&self.path)?;
        if self.end < HEADER_LEN {
            f.set_len(0)?;
            f.write_all(&header_bytes())?;
            self.reset_empty();
            self.end = HEADER_LEN;
        } else {
            // Drop any corrupt tail found during the scan.
            f.set_len(self.end)?;
        }
        self.file = Some(f);
        self.file_is_rw = true;
        if created && self.hidden {
            crate::platform::set_hidden_system(&self.path);
        }
        Ok(())
    }

    fn append(&mut self, kind: u8, key: &[u8], payload: &[&[u8]]) -> io::Result<u64> {
        self.writer()?;
        if kind != K_FOLDER && !self.folder_written {
            self.folder_written = true;
            let folder = self.folder.clone();
            self.write_record(K_FOLDER, folder.as_bytes(), &[])?;
        }
        self.write_record(kind, key, payload)
    }

    fn write_record(&mut self, kind: u8, key: &[u8], payload: &[&[u8]]) -> io::Result<u64> {
        let payload_len: usize = payload.iter().map(|p| p.len()).sum();
        let mut parts: Vec<&[u8]> = vec![key];
        parts.extend_from_slice(payload);
        let crc = crc32(&parts);
        let start = self.end;
        let f = self.file.as_mut().unwrap();
        f.seek(SeekFrom::Start(start))?;
        let mut w = BufWriter::new(f);
        w.write_all(&[kind])?;
        w.write_all(&(key.len() as u16).to_le_bytes())?;
        w.write_all(&(payload_len as u32).to_le_bytes())?;
        w.write_all(&crc.to_le_bytes())?;
        w.write_all(key)?;
        for p in payload {
            w.write_all(p)?;
        }
        w.flush()?;
        drop(w);
        let total = REC_HEADER_LEN + key.len() as u64 + payload_len as u64;
        self.end += total;
        self.live += total;
        Ok(start)
    }

    pub fn set_settings(&mut self, s: &str) -> io::Result<()> {
        if self.settings.as_deref() == Some(s) {
            return Ok(());
        }
        let start = self.append(K_SETTINGS, b"", &[s.as_bytes()])?;
        self.live -= self.settings_len;
        self.dead += self.settings_len;
        self.settings_len = self.end - start;
        self.settings = Some(s.to_string());
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn put(
        &mut self,
        name: &str,
        size: u64,
        mtime: u64,
        max_dim: u16,
        w: u16,
        h: u16,
        fmt: ThumbFmt,
        data: &[u8],
    ) -> io::Result<()> {
        if name.len() > u16::MAX as usize || data.len() > (u32::MAX as usize - 64) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "too large"));
        }
        let mut meta = [0u8; THUMB_META_LEN];
        meta[0..8].copy_from_slice(&size.to_le_bytes());
        meta[8..16].copy_from_slice(&mtime.to_le_bytes());
        meta[16..18].copy_from_slice(&max_dim.to_le_bytes());
        meta[18..20].copy_from_slice(&w.to_le_bytes());
        meta[20..22].copy_from_slice(&h.to_le_bytes());
        meta[22] = fmt as u8;
        let key = name.as_bytes();
        let start = self.append(K_THUMB, key, &[&meta, data])?;
        let record_len = self.end - start;
        let m = ThumbMeta {
            size,
            mtime,
            max_dim,
            w,
            h,
            fmt,
            data_offset: start + REC_HEADER_LEN + key.len() as u64 + THUMB_META_LEN as u64,
            data_len: data.len() as u32,
            record_len,
        };
        if let Some(old) = self.index.insert(key_of(name), m) {
            self.live -= old.record_len;
            self.dead += old.record_len;
        }
        Ok(())
    }

    /// Forgets thumbnails for files that no longer exist (`keep` gets lowercase names).
    pub fn prune(&mut self, keep: &dyn Fn(&str) -> bool) {
        let gone: Vec<String> = self.index.keys().filter(|k| !keep(k)).cloned().collect();
        for k in gone {
            if let Some(old) = self.index.remove(&k) {
                self.live -= old.record_len;
                self.dead += old.record_len;
            }
        }
    }

    /// Rewrites the file without dead records when they make up most of it.
    pub fn compact_if_needed(&mut self) -> io::Result<()> {
        if self.foreign || self.end == 0 || self.dead < 256 * 1024 || self.dead < self.live {
            return Ok(());
        }
        self.compact()
    }

    pub fn compact(&mut self) -> io::Result<()> {
        if self.foreign || self.end == 0 {
            return Ok(());
        }
        let tmp = self.path.with_extension("db.tmp");
        let mut entries: Vec<(String, ThumbMeta)> =
            self.index.iter().map(|(k, v)| (k.clone(), *v)).collect();
        entries.sort_by_key(|(_, m)| m.data_offset);
        let mut blobs = Vec::with_capacity(entries.len());
        for (k, m) in &entries {
            let data = if m.fmt == ThumbFmt::None {
                Vec::new()
            } else {
                self.read_blob(m)?
            };
            blobs.push((k.clone(), *m, data));
        }
        let settings = self.settings.clone();
        let mut fresh = ThumbDb {
            path: tmp.clone(),
            folder: self.folder.clone(),
            file: None,
            index: HashMap::new(),
            settings: None,
            settings_len: 0,
            end: 0,
            live: 0,
            dead: 0,
            foreign: false,
            folder_written: false,
            file_is_rw: false,
            hidden: false,
        };
        let _ = std::fs::remove_file(&tmp);
        if let Some(s) = &settings {
            fresh.set_settings(s)?;
        }
        for (k, m, data) in &blobs {
            fresh.put(k, m.size, m.mtime, m.max_dim, m.w, m.h, m.fmt, data)?;
        }
        if fresh.end == 0 {
            // Nothing left to keep.
            self.file = None;
            self.file_is_rw = false;
            crate::platform::clear_hidden_system(&self.path);
            let _ = std::fs::remove_file(&self.path);
            self.reset_empty();
            return Ok(());
        }
        fresh.file = None;
        self.file = None;
        self.file_is_rw = false;
        replace_file(&tmp, &self.path)?;
        let hidden = self.hidden;
        if hidden {
            crate::platform::set_hidden_system(&self.path);
        }
        *self = ThumbDb::open(&self.path.clone(), &self.folder.clone());
        self.hidden = hidden;
        Ok(())
    }

    fn reset_empty(&mut self) {
        self.index.clear();
        self.settings = None;
        self.settings_len = 0;
        self.end = 0;
        self.live = 0;
        self.dead = 0;
        self.folder_written = false;
    }
}

fn replace_file(from: &Path, to: &Path) -> io::Result<()> {
    // std::fs::rename uses MoveFileEx(MOVEFILE_REPLACE_EXISTING), which is atomic on NTFS, but
    // fails on hidden/system targets; clear those attributes on the target first.
    crate::platform::clear_hidden_system(to);
    std::fs::rename(from, to)
}

fn header_bytes() -> [u8; HEADER_LEN as usize] {
    let mut h = [0u8; HEADER_LEN as usize];
    h[0..8].copy_from_slice(MAGIC);
    h[8..10].copy_from_slice(&VERSION.to_le_bytes());
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    h[16..24].copy_from_slice(&now.to_le_bytes());
    h
}

/// CRC-32 (IEEE) over several byte slices.
pub fn crc32(parts: &[&[u8]]) -> u32 {
    static TABLE: std::sync::OnceLock<[u32; 256]> = std::sync::OnceLock::new();
    let table = TABLE.get_or_init(|| {
        let mut t = [0u32; 256];
        for (i, e) in t.iter_mut().enumerate() {
            let mut c = i as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            }
            *e = c;
        }
        t
    });
    let mut c = !0u32;
    for p in parts {
        for &b in *p {
            c = table[((c ^ b as u32) & 0xff) as usize] ^ (c >> 8);
        }
    }
    !c
}

/// Simple `key=value` lines used for settings payloads and the global config file.
pub fn parse_kv(s: &str) -> HashMap<String, String> {
    s.lines()
        .filter_map(|l| {
            let l = l.trim();
            if l.is_empty() || l.starts_with('#') || l.starts_with(';') || l.starts_with('[') {
                return None;
            }
            let (k, v) = l.split_once('=')?;
            Some((k.trim().to_string(), v.trim().to_string()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("e26test-{}-{}", name, std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let p = dir.join(FILE_NAME);
        let _ = std::fs::remove_file(&p);
        p
    }

    #[test]
    fn roundtrip_settings_and_thumbs() {
        let p = tmp("roundtrip");
        {
            let mut db = ThumbDb::open(&p, "C:\\x");
            assert!(db.settings().is_none());
            db.set_settings("view=details\nsort=name").unwrap();
            db.put("A.jpg", 10, 20, 256, 200, 100, ThumbFmt::Jpeg, b"jpegdata").unwrap();
            db.put("b.mp4", 11, 21, 256, 0, 0, ThumbFmt::None, b"").unwrap();
        }
        let mut db = ThumbDb::open(&p, "C:\\x");
        assert_eq!(db.settings(), Some("view=details\nsort=name"));
        let m = db.lookup("a.JPG", 10, 20, 96).unwrap();
        assert_eq!((m.w, m.h, m.fmt), (200, 100, ThumbFmt::Jpeg));
        assert_eq!(db.read_blob(&m).unwrap(), b"jpegdata");
        assert!(db.lookup("a.jpg", 10, 99, 96).is_none(), "stale mtime");
        assert!(db.lookup("a.jpg", 10, 20, 300).is_none(), "too small");
        assert_eq!(db.lookup("b.mp4", 11, 21, 256).unwrap().fmt, ThumbFmt::None);
    }

    #[test]
    fn replace_and_compact() {
        let p = tmp("compact");
        let mut db = ThumbDb::open(&p, "C:\\y");
        let big = vec![7u8; 100 * 1024];
        for i in 0..8 {
            db.put("same.png", i, i, 256, 1, 1, ThumbFmt::Png, &big).unwrap();
        }
        db.set_settings("view=large").unwrap();
        db.compact_if_needed().unwrap();
        let len = std::fs::metadata(&p).unwrap().len();
        assert!(len < 150 * 1024, "compacted to {len}");
        let mut db = ThumbDb::open(&p, "C:\\y");
        let m = db.lookup("same.png", 7, 7, 256).unwrap();
        assert_eq!(db.read_blob(&m).unwrap().len(), big.len());
        assert_eq!(db.settings(), Some("view=large"));
    }

    #[test]
    fn truncated_tail_is_ignored() {
        let p = tmp("trunc");
        {
            let mut db = ThumbDb::open(&p, "C:\\z");
            db.put("one.jpg", 1, 1, 256, 1, 1, ThumbFmt::Jpeg, b"1111").unwrap();
            db.put("two.jpg", 2, 2, 256, 1, 1, ThumbFmt::Jpeg, b"2222").unwrap();
        }
        let len = std::fs::metadata(&p).unwrap().len();
        let f = OpenOptions::new().write(true).open(&p).unwrap();
        f.set_len(len - 3).unwrap();
        drop(f);
        let mut db = ThumbDb::open(&p, "C:\\z");
        assert!(db.lookup("one.jpg", 1, 1, 1).is_some());
        assert!(db.lookup("two.jpg", 2, 2, 1).is_none());
        // Writing again truncates the broken tail and appends cleanly.
        db.put("three.jpg", 3, 3, 256, 1, 1, ThumbFmt::Jpeg, b"3333").unwrap();
        let mut db = ThumbDb::open(&p, "C:\\z");
        let m = db.lookup("three.jpg", 3, 3, 1).unwrap();
        assert_eq!(db.read_blob(&m).unwrap(), b"3333");
    }

    #[test]
    fn foreign_file_untouched() {
        let p = tmp("foreign");
        std::fs::write(&p, b"something else entirely, not ours").unwrap();
        let mut db = ThumbDb::open(&p, "C:\\w");
        assert!(db.put("a", 1, 1, 1, 1, 1, ThumbFmt::Jpeg, b"x").is_err());
        assert_eq!(std::fs::read(&p).unwrap(), b"something else entirely, not ours");
    }
}
