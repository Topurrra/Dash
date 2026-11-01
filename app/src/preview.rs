//! The preview pane beside the results: what's inside the selected file
//! (code with line numbers, document text, images, GIFs) plus its details.
//! Loaded off the UI thread; video and music play (see `media`).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use gpui::SharedString;

const IMAGE_EXTS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "bmp", "svg", "ico", "tif", "tiff"];
const MEDIA_EXTS: &[&str] =
    &["mp4", "mkv", "webm", "mov", "avi", "wmv", "m4v", "mp3", "flac", "wav", "ogg", "m4a", "aac", "opus", "wma"];
/// Documents whose text is extracted (shown without line numbers).
const DOC_EXTS: &[&str] = &["pdf", "docx", "xlsx", "pptx", "odt", "ods", "odp", "rtf"];
const MAX_LINES: usize = 2000;

pub enum Body {
    Image(PathBuf),
    /// Video or music: played in the pane.
    Media(PathBuf),
    Text {
        lines: Vec<SharedString>,
        /// Code / plain text: show line numbers.
        code: bool,
        /// Line to scroll to and mark (Inside: the first match).
        hit: Option<usize>,
        truncated: bool,
        full: SharedString,
    },
    Note(SharedString),
    /// A folder: what's in it (folders first, then by name; at most
    /// `FOLDER_ITEMS`) and how many items it has in all.
    Folder {
        items: Vec<FolderItem>,
        total: usize,
    },
}

pub struct FolderItem {
    pub name: SharedString,
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
}

/// Entries listed in a folder's preview; the rest are counted.
const FOLDER_ITEMS: usize = 500;

pub struct Preview {
    pub path: String,
    pub name: SharedString,
    pub details: Vec<(&'static str, SharedString)>,
    pub body: Body,
    /// Music: its tags and cover, for the player.
    pub music: Option<Music>,
}

pub struct Music {
    pub title: Option<SharedString>,
    pub artist: Option<SharedString>,
    pub album: Option<SharedString>,
    pub cover: Option<Arc<gpui::Image>>,
    /// From the file, in 100 ns units: the player only knows once it plays.
    pub length: u64,
}

const AUDIO_EXTS: &[&str] = &["mp3", "flac", "wav", "ogg", "m4a", "aac", "opus", "wma"];

/// Tags and the first embedded picture; adds what's useful to `details`.
fn music(path: &Path, details: &mut Vec<(&'static str, SharedString)>) -> Option<Music> {
    use lofty::prelude::*;
    let file = lofty::read_from_path(path).ok()?;
    let props = file.properties();
    if let Some(kbps) = props.audio_bitrate() {
        details.push(("Bitrate", format!("{kbps} kbps").into()));
    }
    if let Some(hz) = props.sample_rate() {
        details.push(("Sample rate", format!("{:.1} kHz", hz as f32 / 1000.).into()));
    }
    let tag = file.primary_tag().or_else(|| file.first_tag());
    let text = |value: Option<std::borrow::Cow<str>>| {
        value.map(|v| SharedString::from(v.trim().to_string())).filter(|v| !v.is_empty())
    };
    let (title, artist, album) = match tag {
        Some(t) => (text(t.title()), text(t.artist()), text(t.album())),
        None => (None, None, None),
    };
    if let Some(year) = tag.and_then(|t| t.year()) {
        details.push(("Year", year.to_string().into()));
    }
    let cover = tag.and_then(|t| t.pictures().first()).and_then(|picture| {
        use lofty::picture::MimeType;
        let format = match picture.mime_type()? {
            MimeType::Png => gpui::ImageFormat::Png,
            MimeType::Jpeg => gpui::ImageFormat::Jpeg,
            MimeType::Gif => gpui::ImageFormat::Gif,
            MimeType::Bmp => gpui::ImageFormat::Bmp,
            MimeType::Tiff => gpui::ImageFormat::Tiff,
            _ => return None,
        };
        Some(Arc::new(gpui::Image::from_bytes(format, picture.data().to_vec())))
    });
    let length = u64::try_from(props.duration().as_nanos() / 100).unwrap_or(u64::MAX);
    Some(Music { title, artist, album, cover, length })
}

/// Reads `path` for the pane. `query` marks the first line that matches.
pub fn load(path: String, query: &str) -> Preview {
    let p = Path::new(&path);
    let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.clone());
    let ext = p.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    let meta = std::fs::metadata(p);
    let mut details: Vec<(&'static str, SharedString)> = Vec::new();
    if let Some(parent) = p.parent() {
        details.push(("Folder", parent.display().to_string().into()));
    }
    let body = match &meta {
        Err(e) => Body::Note(format!("Can't read it: {e}").into()),
        Ok(m) if m.is_dir() => {
            let (items, total) = folder_items(p);
            details.push((
                "Contains",
                if total == 1 { "1 item".into() } else { format!("{} items", thousands(total)).into() },
            ));
            Body::Folder { items, total }
        }
        Ok(_) if IMAGE_EXTS.contains(&ext.as_str()) => Body::Image(p.to_path_buf()),
        Ok(_) if MEDIA_EXTS.contains(&ext.as_str()) => Body::Media(p.to_path_buf()),
        Ok(_) => {
            match dash_search::search::read_file_preview(path.clone(), Some(PREVIEW_BYTES)).or_else(|| sniff_text(p)) {
                Some(text) if !text.trim().is_empty() => text_body(&text, !DOC_EXTS.contains(&ext.as_str()), query),
                _ => Body::Note("No preview for this kind of file.".into()),
            }
        }
    };
    if let Ok(m) = &meta {
        if m.is_file() {
            details.push(("Size", human_size(m.len()).into()));
        }
        if !ext.is_empty() {
            details.push(("Type", ext.to_uppercase().into()));
        }
        if let Ok(t) = m.modified() {
            details.push(("Modified", when(t).into()));
        }
        if let Ok(t) = m.created() {
            details.push(("Created", when(t).into()));
        }
    }
    let music = if AUDIO_EXTS.contains(&ext.as_str()) && matches!(body, Body::Media(_)) {
        music(p, &mut details)
    } else {
        None
    };
    Preview { path, name: name.into(), details, body, music }
}

/// A folder's entries for the pane, and how many it has (counting stops at
/// 100,000).
fn folder_items(folder: &Path) -> (Vec<FolderItem>, usize) {
    let Ok(read) = std::fs::read_dir(folder) else { return (Vec::new(), 0) };
    let mut all: Vec<FolderItem> = read
        .take(100_000)
        .filter_map(Result::ok)
        .map(|entry| {
            let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
            FolderItem {
                name: entry.file_name().to_string_lossy().into_owned().into(),
                path: entry.path().display().to_string(),
                is_dir,
                size: 0,
            }
        })
        .collect();
    let total = all.len();
    all.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
    all.truncate(FOLDER_ITEMS);
    for item in all.iter_mut().filter(|i| !i.is_dir) {
        item.size = std::fs::metadata(&item.path).map(|m| m.len()).unwrap_or(0);
    }
    (all, total)
}

const PREVIEW_BYTES: usize = 256 * 1024;

/// Any other file that turns out to be text (`.cs`, `Makefile`,
/// `.gitignore`…): UTF-8 with no zero bytes. Only read, never parsed.
fn sniff_text(path: &Path) -> Option<String> {
    use std::io::Read;
    let mut bytes = Vec::new();
    std::fs::File::open(path).ok()?.take(PREVIEW_BYTES as u64).read_to_end(&mut bytes).ok()?;
    looks_like_text(bytes)
}

fn looks_like_text(mut bytes: Vec<u8>) -> Option<String> {
    if bytes.contains(&0) {
        return None;
    }
    if bytes.starts_with(b"\xEF\xBB\xBF") {
        bytes.drain(..3);
    }
    match String::from_utf8(bytes) {
        Ok(text) => Some(text),
        // Cut mid-character at the read limit: keep what came before.
        Err(e) if e.utf8_error().error_len().is_none() => {
            let valid = e.utf8_error().valid_up_to();
            let mut bytes = e.into_bytes();
            bytes.truncate(valid);
            String::from_utf8(bytes).ok()
        }
        Err(_) => None,
    }
}

/// A clipboard entry's text, for the pane (no file behind it).
pub fn text_only(text: &str) -> Body {
    text_body(text, false, "")
}

fn text_body(text: &str, code: bool, query: &str) -> Body {
    let all: Vec<&str> = text.lines().collect();
    let truncated = all.len() > MAX_LINES;
    let lines: Vec<SharedString> =
        all.iter().take(MAX_LINES).map(|l| SharedString::from(l.replace('\t', "    "))).collect();
    let words: Vec<String> =
        query.split_whitespace().filter(|w| w.chars().count() >= 2).map(str::to_lowercase).collect();
    let hit = (!words.is_empty())
        .then(|| {
            lines.iter().position(|l| {
                let l = l.to_lowercase();
                words.iter().any(|w| l.contains(w))
            })
        })
        .flatten();
    Body::Text { lines, code, hit, truncated, full: text.to_string().into() }
}

/// 1234567 → "1,234,567".
pub fn thousands(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024. && unit < UNITS.len() - 1 {
        value /= 1024.;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn when(t: std::time::SystemTime) -> String {
    chrono::DateTime::<chrono::Local>::from(t).format("%Y-%m-%d %H:%M").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folders_list_their_contents() {
        let dir = std::env::temp_dir().join(format!("dash-folder-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("zeta")).unwrap();
        std::fs::write(dir.join("b.txt"), b"12345").unwrap();
        std::fs::write(dir.join("A.md"), b"").unwrap();
        let preview = load(dir.display().to_string(), "");
        let Body::Folder { items, total } = preview.body else { panic!("not a folder body") };
        assert_eq!(total, 3);
        let names: Vec<&str> = items.iter().map(|i| i.name.as_ref()).collect();
        assert_eq!(names, ["zeta", "A.md", "b.txt"], "folders first, then by name");
        assert_eq!(items[2].size, 5);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn text_is_told_from_binary() {
        assert_eq!(looks_like_text(b"class Invoice {}\n".to_vec()).as_deref(), Some("class Invoice {}\n"));
        assert_eq!(looks_like_text(b"\xEF\xBB\xBFusing System;".to_vec()).as_deref(), Some("using System;"));
        // Cut in the middle of "é" at the read limit.
        assert_eq!(looks_like_text(b"caf\xC3".to_vec()).as_deref(), Some("caf"));
        assert_eq!(looks_like_text(b"MZ\x90\x00\x03".to_vec()), None);
        assert_eq!(looks_like_text(b"\xFF\xFEa\xFF".to_vec()), None);
    }

    /// Writes tags and a cover into a copy of a Windows sound, then reads them back.
    #[cfg(windows)]
    #[test]
    fn music_tags_and_cover_are_read() {
        use lofty::config::WriteOptions;
        use lofty::picture::{MimeType, Picture, PictureType};
        use lofty::prelude::*;
        use lofty::tag::{Tag, TagType};

        let dir = std::env::temp_dir().join(format!("dash-music-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("song.wav");
        std::fs::copy(r"C:\Windows\Media\Alarm01.wav", &file).unwrap();

        let mut png = Vec::new();
        image::RgbaImage::from_pixel(4, 4, image::Rgba([200, 30, 30, 255]))
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let mut tag = Tag::new(TagType::Id3v2);
        tag.set_title("Test Title".into());
        tag.set_artist("Test Artist".into());
        tag.set_album("Test Album".into());
        tag.push_picture(Picture::new_unchecked(PictureType::CoverFront, Some(MimeType::Png), None, png));
        tag.save_to_path(&file, WriteOptions::default()).unwrap();

        let preview = load(file.display().to_string(), "");
        assert!(matches!(preview.body, Body::Media(_)));
        let music = preview.music.expect("music");
        assert_eq!(music.title.as_ref().map(|s| s.as_ref()), Some("Test Title"));
        assert_eq!(music.artist.as_ref().map(|s| s.as_ref()), Some("Test Artist"));
        assert_eq!(music.album.as_ref().map(|s| s.as_ref()), Some("Test Album"));
        assert!(music.cover.is_some(), "cover read");
        assert!(preview.details.iter().any(|(k, _)| *k == "Sample rate"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
