# Explorer26 architecture

Explorer26 is a native Win32 + Direct2D file explorer written in Rust, modelled on Windows 11 File
Explorer. It follows the same approach as [images-app](https://github.com/sidx1024/images-app):
no UI framework, everything custom-drawn, and real shell integration wherever Windows provides it.

```
 ┌───────────── window.rs (Win32 host) ─────────────┐
 │ WndProc · custom frame · DPI · timers · effects  │
 └──────┬──────────────────────────────▲────────────┘
  Input │ (DIPs, abstract)      Effect │ (menus, shell ops, timers…)
 ┌──────▼──────────────────────────────┴────────────┐
 │ ui/  App: tabs, nav, selection, layout, input    │──paint──▶ Scene (display list)
 └──────┬───────────────────────────────────────────┘             │
        │ Msg (channel + coalesced WM_APP)                        ▼
 ┌──────┴─────────────────────────────┐                 render.rs (Direct2D)
 │ workers: listing · search · icons  │                 future: web canvas client
 │ thumbs pool · watcher · file ops   │
 └──────┬─────────────────────────────┘
        ▼
  fs.rs · icons.rs · thumbs.rs · store.rs (thumbs26.db) · shell.rs · watch.rs
```

## Modules

| File | Role |
|---|---|
| `scene.rs` | Renderer-agnostic display list (`Cmd`), `ImageKey`, `ImageStore`, `Measure` trait |
| `render.rs` | Direct2D/DirectWrite backend that replays a `Scene` |
| `ui/` | Platform-neutral UI core: `App`, tabs, nav tree, grid layout, painting, input, text fields |
| `window.rs` | Win32 host: window, custom title bar (Snap Layouts), input translation, effect execution |
| `fs.rs` | `FindFirstFileEx` (large fetch) listing, This PC, recursive search, `StrCmpLogicalW` sorting |
| `icons.rs` | Shell icons/type names via `SHGetFileInfo` + system image lists |
| `thumbs.rs` | Thumbnail worker pool: `IShellItemImageFactory` → WIC encode → `thumbs26.db` → decode at view size |
| `store.rs` | The `thumbs26.db` format |
| `shell.rs` | Shell context menus, `IFileOperation`, clipboard, properties, OLE drag & drop |
| `watch.rs` | `ReadDirectoryChangesW` live refresh |
| `config.rs` | Global `config.ini` |
| `theme.rs` | Light/dark/high contrast, accent, Mica Alt |

## Why it is fast

* Listing uses `FindFirstFileExW(FindExInfoBasic, FIND_FIRST_EX_LARGE_FETCH)` on a background thread.
  Type names and icons are looked up once per extension. Per-file icons (exe, lnk, customized
  folders) come in a second pass, so a folder appears before they resolve.
* Only visible items are painted and hit-tested (`ui/layout.rs::Grid` virtualizes every view mode).
* Thumbnail requests are replaced wholesale on every scroll, visible items first, so no stale work queues up.
* Thumbnails decode at the exact display size; GPU bitmaps are cached and LRU-evicted.
* Worker results are coalesced into a single posted message.

## Native look

* Real shell icons (system image list, including overlays) and real type names.
* The full classic shell context menu (`IContextMenu` with `IContextMenu3` forwarding, so the
  owner-drawn "Open with", "Send to" and "New" submenus work), themed dark/light like Explorer's.
* The background menu is the shell's own folder menu (New ▸ with every registered type, Paste,
  Open in Terminal, Properties) with View/Sort submenus added on top.
* `IFileOperation` gives Explorer's progress, conflict and elevation dialogs, plus undo-able recycling.
* Drag & drop: dragging out uses `SHDoDragDrop` (shell drag images). Our `IDropTarget` forwards every
  drop to the shell's own drop target for the folder or file under the cursor, so dropping onto
  folders, drives, `.exe` or `.zip` behaves exactly as in Explorer.
* Mica Alt backdrop, Segoe Fluent Icons, Fluent colors, Snap Layouts on the maximize button.

## thumbs26.db: thumbnails and per-folder settings

Each folder gets one `thumbs26.db`, which stores **both** its view settings (view mode, sort,
column widths) and its thumbnails. It never touches `Thumbs.db` or the system `thumbcache_*.db`.

Format (see `store.rs`): a 32-byte header (`THUMBS26`, version) followed by an append-only log of
CRC-32-checked records:

| kind | key | payload |
|---|---|---|
| 1 FOLDER | folder path | — (lets a central cache map back to folders) |
| 2 SETTINGS | — | `key=value` lines; newest wins |
| 3 THUMB | file name | size, mtime, max_dim, w, h, fmt (JPEG / PNG / "no thumbnail") + bytes |
| 4 DELETE | file name | tombstone |

* **Crash-safe:** a torn tail fails its CRC and is truncated on the next write.
* **Validity:** an entry is used only if the file's size and mtime still match.
* **Negative caching:** "this file has no thumbnail" is stored, so the shell isn't asked again.
* **Compaction:** replaced and pruned records become dead space. When it outweighs live data, the
  file is rewritten through a temp file and an atomic rename. Pruning of deleted files runs when you leave a folder.
* **Never clobbers:** a file with a foreign magic or newer version is left untouched.
* **Location** (`config.ini` → `thumbnail_store`):
  * `central` (default): `%LOCALAPPDATA%\Explorer26\folders\<fnv64(path)>.thumbs26.db`. This keeps
    folders clean (git repos, OneDrive, read-only media, network shares).
  * `folder`: a hidden+system `thumbs26.db` inside each folder on fixed local drives (the settings
    travel with the folder), falling back to central elsewhere. An existing in-folder db is still
    read in central mode.
* The file is created lazily, only when there is something to store.

Global settings (theme, Mica, hidden files, extensions, compact view, window placement, pinned
folders, nav width) live in the human-editable `%LOCALAPPDATA%\Explorer26\config.ini`.

## Instant search (index.rs)

Everything-style search without administrator rights:

* One in-memory index per fixed drive: 40-byte nodes (parent, first child, next sibling, name
  offset, attributes, size, mtime) plus one packed UTF-8 name buffer. Queries scan all nodes in
  parallel with ASCII case-folding and need no per-name allocation.
* Measured on a 1.7M-item C: drive: first crawl ≈15 s (multi-threaded `FindFirstFileEx`, at
  background CPU/IO priority); queries 13–65 ms; the saved index (161 MB) loads in ≈0.15 s.
* Saved to `%LOCALAPPDATA%\Explorer26\index\<volume serial>.e26idx`; a background re-crawl at
  launch catches changes made while the app was closed.
* Live updates: a recursive `ReadDirectoryChangesW` per drive collects changed folders, which
  are rescanned in one batch per second. A buffer overflow (or too many dead nodes) triggers a
  full re-crawl.
* Syntax: space-separated terms are ANDed; `*`/`?` match whole names; a term containing `\`
  matches the full path. Searching from This PC covers all drives; searching from a folder
  covers its subtree. Until a drive is indexed, search falls back to walking folders.
* Everything itself reads the NTFS MFT (≈2 s for a full drive), which needs admin rights. That
  could be added later as an optional fast path for elevated runs or a helper service.

## Web access: streaming the display list (planned)

The goal is to use Explorer26 from a browser without rebuilding the UI in the DOM. Because the UI
core already paints into a `Scene` and consumes abstract `Input`, the web version is a second host,
not a second UI.

**Approach: remote display-list rendering (not video).**

1. The server (Explorer26 headless, or a `--serve` mode) runs one `App` per browser session.
2. Each frame, the `Scene` is serialized into a compact binary stream (`Fill`/`Round`/`Text`/`Image`/clip
   commands, in f32 DIPs) and **diffed** against the previous frame. Most frames (hover, caret
   blink) are a few dozen bytes.
3. `ImageKey`s are content-addressed resources: icons and thumbnails go to the client once (as
   PNG/WebP) and are cached in the browser by key.
4. The browser client is a single `<canvas>` (Canvas2D first, WebGL/WebGPU later), about 300 lines
   of JS replaying commands. Text uses the same font names; ship Segoe UI Variable and Fluent Icons
   as web fonts for non-Windows clients.
5. Input (`pointer*`, `wheel`, `keydown`, `beforeinput`/IME text) maps 1:1 to `ui::Input` and goes
   back over the same WebSocket.
6. Text measurement stays on the server (DirectWrite through `Measure`). The client just draws runs
   at given positions, so layout is identical on both sides.

Why not pixel streaming (WebRTC/H.264)? It needs a GPU encoder, adds latency, makes text blurry at
non-native DPI, and costs constant bandwidth. A display list is crisp at any client DPI and nearly
free when idle. Pixel streaming stays available as a fallback, since the same `Scene` can be
rasterized server-side.

**What has to change for remote sessions:**

* Native popups (`TrackPopupMenu`, shell context menus) must become scene-drawn menus. Shell menu
  items can still come from `IContextMenu`: query into an `HMENU`, read the items with
  `GetMenuItemInfo`, draw them ourselves, and `InvokeCommand` the chosen id.
* Shell dialogs (progress, conflicts, properties) need scene-drawn equivalents, or must run
  "server-side only".
* Drag & drop maps to browser upload/download (`DataTransfer`).
* Auth/TLS on the WebSocket endpoint and a per-session sandbox root.

## Microsoft Store (not started)

This will follow images-app's packaging: an MSIX with `runFullTrust`, plus an MSI via WiX. Per-user
data lives under `%LOCALAPPDATA%`, which MSIX virtualizes per package, so it already works packaged.
