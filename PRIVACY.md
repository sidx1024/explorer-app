# Privacy policy

*Last updated: November 15, 2026*

Explorer App is a file explorer that runs entirely on your PC.

- **No data collection.** Explorer App doesn't collect, transmit or share any personal information, usage data, analytics or crash reports.
- **No network access of its own.** Explorer App never contacts any server. It shows network locations only when you open them.
- **No accounts, no AI features, no cloud.**
- **Data stored on your PC only**, in `%LOCALAPPDATA%\Explorer App`:
  - `config.ini`: your preferences (theme, view options, window position, pane sizes).
  - `folders\*.thumbs26.db`: thumbnails and per-folder view settings (view mode, sort order, column widths) for folders you've opened. If you turn on *Store thumbnails inside folders*, a hidden `thumbs26.db` is written into those folders instead.
  - `index\*.e26idx`: the instant-search index, containing the names, sizes and dates of the files and folders on your fixed drives. You can turn it off with *Search options > Instant search index* (or `index_drives=false` in `config.ini`).

  The Microsoft Store version removes this folder on uninstall. The installer and portable versions leave it; delete it to remove everything.
- **Windows and installed shell extensions.** Thumbnails, previews and context menu entries come from Windows and from the shell extensions installed on your PC (for example a PDF reader's preview handler). Those components follow their own vendors' privacy policies.
- **Files you act on.** Copying, moving, renaming and deleting files only happen when you choose to. Deleting moves items to the Recycle Bin unless you use Shift+Delete.

Questions: open an issue at https://github.com/sidx1024/explorer-app/issues.
