# Explorer App

A fast, native-looking file explorer for Windows 11, written from scratch in Rust: raw Win32,
Direct2D and the real Windows shell, with no UI framework.

## Features

* **Windows 11 look:** tabs in the title bar, Mica Alt, Fluent icons, Snap Layouts, light/dark/high
  contrast, per-monitor DPI, real shell icons and file type names.
* **Views:** Extra large / Large / Medium / Small icons, List, Details (sortable, resizable columns,
  double-click a column edge to autofit) and Tiles (drives with usage bars). Ctrl+mouse wheel changes size.
* **Thumbnails** for anything with a shell thumbnail provider (photos, videos, PDFs...), cached in
  `thumbs26.db` (never `Thumbs.db`).
* **Per-folder memory:** view mode, sort order and column widths are stored with the folder's thumbnails.
  Mostly-media folders default to Large icons.
* **Navigation:** back/forward/up, breadcrumbs with subfolder dropdowns, an editable address bar
  (`%VARS%`, relative paths), a navigation tree (Quick access, This PC, drives), live refresh on changes,
  scroll position restored on Back.
* **Files:** Explorer's full context menu (with extensions such as 7-Zip), New folder / Text document,
  rename inline, cut/copy/paste, delete to the Recycle Bin (Shift+Del permanently), Properties, Open in
  Terminal, drag & drop in both directions.
* **Search:** recursive search as you type, results streaming in, with a Folder column.
* **Selection:** click, Ctrl/Shift-click, rubber band, keyboard, type-ahead.

## Keyboard

| Keys | Action |
|---|---|
| Alt+←/→/↑, Backspace, mouse back/forward | Back / Forward / Up |
| Ctrl+L, Alt+D, F4 | Edit address |
| Ctrl+F, Ctrl+E, F3 | Search |
| Ctrl+T / Ctrl+W / Ctrl+Tab | New / close / switch tab |
| Ctrl+N / Ctrl+Shift+N | New window / new folder |
| F2, F5, Del, Shift+Del | Rename, refresh, recycle, delete permanently |
| Ctrl+A/C/X/V, Alt+Enter | Select all, copy, cut, paste, properties |
| Ctrl+Shift+1…7 | Extra large, large, medium, small, list, details, tiles |
| Menu key, Shift+F10 | Context menu |

## Build

Requires Rust (MSVC toolchain) and the Windows SDK (for `rc.exe`).

```
cargo build --release
cargo test
```

Run `target\release\explorer-app.exe [folder-or-file]`.

## Settings

* `%LOCALAPPDATA%\Explorer App\config.ini`: global settings (also available from the ··· menu).
* `thumbnail_store=central|folder`: whether `thumbs26.db` lives in
  `%LOCALAPPDATA%\Explorer App\folders` (default) or hidden inside each folder.

See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for the design, the `thumbs26.db` format and the
plan for web access.
