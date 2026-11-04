# Dash

Instant file search and clipboard history in one keyboard overlay.
Native (Rust + [GPUI](https://www.gpui.rs)), local-only, lives in the tray.

| | |
|---|---|
| **Search** | `Ctrl+Alt+F` — find any file by name in milliseconds, or search inside documents |
| **Clipboard** | `Ctrl+Alt+C` — everything you copied: text, images, animated GIFs |
| **Settings** | `Ctrl+,` — indexing, clipboard and hotkeys, all inside the overlay |

## Features

**Search**
- Every file and folder name on your drives, matched as you type — anywhere in the name, any case
- **Inside** tab: the text of documents, code, PDFs and Office files; OCR for images and scans; search by meaning
- Query language: `*.pdf`, `"exact phrase"`, `a OR b`, `-tmp`, `( … )`, `regex:`, `case:`, `path:`, `ext:pdf;docx`, `size:1mb..5mb`, `dm:lastweek`, `parent:`, `depth:`, saved filters `@name`
- Results table: sort by name, path, size, date or type; no result limit
- Multi-select, open, show in folder, copy paths, rename, delete to Recycle Bin, move, drag out
- Preview: code with line numbers, documents, images, GIFs, video and music with cover art

**Clipboard**
- Text and images (GIFs stay animated), source app and age
- Search, pin, delete; Enter pastes into the app you came from
- Skips password managers; history encrypted at rest (Windows)

**Everything else**
- Index stays fresh with a file watcher; quiet / balanced / fast indexing speed
- Change hotkeys, start with the system, all from the overlay
- Nothing leaves your computer

## Benchmarks

Release build, Windows 11, Intel Core i9-14900KF, 64 GB RAM.

**Name search — 1,000,000 files**

| Query | Matches | Best match | By name | By size |
|---|---:|---:|---:|---:|
| `budget 42` (rare) | 2,966 | 3.1 ms | 2.4 ms | 1.9 ms |
| `report` (common) | 100,000 | 4.0 ms | 2.2 ms | 1.9 ms |
| `e` (almost everything) | 500,000 | 7.1 ms | 2.2 ms | 1.9 ms |
| `zzz` (nothing) | 0 | 1.8 ms | 2.9 ms | 2.2 ms |

Times include parsing the query, scanning every name and returning the first 100 rows.

| Operation | Time |
|---|---:|
| Load 1M names into memory | 276 ms |
| Prepare the other sort orders (background) | 142 ms |
| Scan 1M names (parallel) | 1.5 ms |
| Next page of a sorted result | 12 µs |
| Jump to row 500,000 of an unfiltered list | 1.5 µs |

**Real index — 330,627 files and folders**

| | |
|---|---:|
| Load into memory | 392 ms |
| Query + first page, any sort | 2.6 – 5.7 ms |
| Memory | 70 – 90 bytes per file |

## Install

From [Releases](https://github.com/Topurrra/Dash/releases):

- **Windows:** `Dash_<version>_x64-setup.exe`. Per-user install, no admin rights.
- **macOS:** `Dash_<version>_aarch64.dmg`; drag Dash to Applications. It has no Apple Developer ID, so the first start is blocked once: open System Settings › Privacy & Security and choose Open Anyway. Pasting into other apps needs Accessibility permission (macOS asks once).
- **Linux:** `Dash_<version>_x86_64.AppImage`; make it executable and run it. Pasting into other apps works on X11; on Wayland the text is put on the clipboard for you to paste. Clipboard history is encrypted with a key kept in the Secret Service (GNOME Keyring, KWallet).

macOS and Linux are built and tested in CI but haven't been tried by hand yet, and they don't play video. Each package has a `.sha256` beside it; Dash updates itself from the same files.

## Build

```bash
cargo build --release -p dash
```

Windows installer: `installer/build.ps1` (needs NSIS, plus the runtime files listed in `installer/dash.nsi`: PDFium, Tesseract, the EmbeddingGemma 2 model).
macOS `.dmg`: `packaging/macos/make_dmg.sh`. Linux AppImage: `packaging/linux/make_appimage.sh`. CI builds both, and the Release workflow attaches them to each published release.

| Crate | What |
|---|---|
| `app` | The overlay (GPUI), tray and hotkeys |
| `dash-search` | Name table, query language, filename + content indexes, text extraction, OCR, meaning search |
| `dash-clipboard` | Clipboard capture, history, pins, paste |
| `dash-core` | Shared storage, encryption, process link |

## License

[GPL-3.0-or-later](LICENSE). You can use, change and share Dash; anything you
distribute that's built from it must be open source under the same license.
Bundled components and the meaning model keep their own licenses — see
[THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md).
