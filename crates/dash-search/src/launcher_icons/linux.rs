//! Icons on Linux, the freedesktop way: an app's `.desktop` file names its
//! icon; a file's type comes from the shared MIME database (`mime/globs2`)
//! and names an icon (`text-x-python`, then the generic `text-x-generic`).
//! Names are looked up in the icon themes under the XDG data folders and in
//! `pixmaps`; SVG icons are drawn with resvg (which GPUI already uses).

use super::{png_of, ExtractMode, ICON_PX};
use resvg::{tiny_skia, usvg};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// `ICON_PX` for drawing.
const ICON_PX_F32: f32 = 64.0;

/// Themes tried first, in order; any others after them.
const PREFERRED_THEMES: [&str; 7] = ["Adwaita", "AdwaitaLegacy", "breeze", "Yaru", "Papirus", "elementary", "hicolor"];

pub(super) fn extract_icon_png(path: &str, mode: &ExtractMode) -> Option<Vec<u8>> {
    let names = match mode {
        ExtractMode::Folder => vec!["folder".to_string()],
        ExtractMode::Extension(extension) => names_for_extension(extension),
        ExtractMode::RealPath if path.ends_with(".desktop") => {
            let icon = desktop_icon(&std::fs::read_to_string(path).ok()?)?;
            if Path::new(&icon).is_absolute() {
                return draw(Path::new(&icon));
            }
            vec![icon]
        }
        ExtractMode::RealPath if Path::new(path).is_dir() => vec!["folder".to_string()],
        ExtractMode::RealPath => {
            let extension = Path::new(path).extension().and_then(|e| e.to_str()).unwrap_or_default();
            let mut names = names_for_extension(&extension.to_lowercase());
            names.push("application-x-executable".to_string());
            names
        }
        ExtractMode::Packaged => return None,
    };
    names.iter().find_map(|name| find_icon(name)).and_then(|file| draw(&file))
}

/// `$XDG_DATA_HOME` and `$XDG_DATA_DIRS`, with their defaults.
fn data_dirs() -> Vec<PathBuf> {
    let home = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")));
    let system = std::env::var("XDG_DATA_DIRS").unwrap_or_default();
    let system = if system.is_empty() { "/usr/local/share:/usr/share".to_string() } else { system };
    home.into_iter().chain(system.split(':').filter(|d| !d.is_empty()).map(PathBuf::from)).collect()
}

/// The `Icon=` of a `.desktop` file's `[Desktop Entry]` group.
fn desktop_icon(desktop: &str) -> Option<String> {
    let mut in_entry = false;
    for line in desktop.lines().map(str::trim) {
        if line.starts_with('[') {
            in_entry = line == "[Desktop Entry]";
        } else if in_entry {
            if let Some(icon) = line.strip_prefix("Icon=") {
                return Some(icon.trim().to_string()).filter(|icon| !icon.is_empty());
            }
        }
    }
    None
}

/// Icon names for a file extension: the type's own, its generic one, and
/// the plain-file icon.
fn names_for_extension(extension: &str) -> Vec<String> {
    let mut names = Vec::new();
    let dirs = data_dirs();
    let read = |file: &str| {
        dirs.iter().filter_map(|d| std::fs::read_to_string(d.join("mime").join(file)).ok()).collect::<Vec<_>>()
    };
    if let Some(mime) = read("globs2").iter().find_map(|globs| mime_for_extension(globs, extension)) {
        names.push(mime.replace('/', "-"));
        let generic = read("generic-icons").iter().find_map(|icons| generic_icon(icons, &mime));
        names.push(generic.unwrap_or_else(|| format!("{}-x-generic", mime.split('/').next().unwrap_or("text"))));
    }
    names.push("text-x-generic".to_string());
    names
}

/// The MIME type `globs2` gives `*.extension` (highest weight wins).
fn mime_for_extension(globs2: &str, extension: &str) -> Option<String> {
    let glob = format!("*.{extension}");
    globs2
        .lines()
        .filter(|line| !line.starts_with('#'))
        .filter_map(|line| {
            let mut parts = line.split(':');
            let weight: u32 = parts.next()?.parse().ok()?;
            let mime = parts.next()?;
            let pattern = parts.next()?;
            let case_sensitive = parts.next().is_some_and(|flags| flags.contains("cs"));
            let hit = if case_sensitive { pattern == glob } else { pattern.eq_ignore_ascii_case(&glob) };
            hit.then(|| (weight, mime.to_string()))
        })
        .max_by_key(|(weight, _)| *weight)
        .map(|(_, mime)| mime)
}

/// The icon `generic-icons` gives `mime` (lines of `type/name:icon-name`).
fn generic_icon(generic_icons: &str, mime: &str) -> Option<String> {
    generic_icons.lines().find_map(|line| line.strip_prefix(mime)?.strip_prefix(':').map(str::to_string))
}

/// One icon file: its theme's rank and its size (`u32::MAX` for SVG).
struct Found {
    path: PathBuf,
    theme_rank: usize,
    size: u32,
}

/// Every icon file in every theme and `pixmaps`, by name. Built once.
fn icon_index() -> &'static HashMap<String, Vec<Found>> {
    static INDEX: OnceLock<HashMap<String, Vec<Found>>> = OnceLock::new();
    INDEX.get_or_init(|| {
        let mut index: HashMap<String, Vec<Found>> = HashMap::new();
        for dir in data_dirs() {
            for theme in std::fs::read_dir(dir.join("icons")).into_iter().flatten().flatten() {
                let name = theme.file_name().to_string_lossy().into_owned();
                let rank = PREFERRED_THEMES.iter().position(|t| *t == name).unwrap_or(PREFERRED_THEMES.len());
                for entry in walkdir::WalkDir::new(theme.path()).max_depth(4).into_iter().flatten() {
                    add_icon(&mut index, entry.path(), rank);
                }
            }
            for entry in std::fs::read_dir(dir.join("pixmaps")).into_iter().flatten().flatten() {
                add_icon(&mut index, &entry.path(), PREFERRED_THEMES.len() + 1);
            }
        }
        index
    })
}

fn add_icon(index: &mut HashMap<String, Vec<Found>>, path: &Path, theme_rank: usize) {
    let (Some(stem), Some(extension)) = (path.file_stem().and_then(|s| s.to_str()), path.extension()) else {
        return;
    };
    let size = match extension.to_str() {
        Some("svg") => u32::MAX,
        Some("png") => path.components().find_map(|c| size_of_dir(&c.as_os_str().to_string_lossy())).unwrap_or(0),
        _ => return,
    };
    // Symbolic icons are one-colour outlines meant for toolbars.
    if stem.ends_with("-symbolic") {
        return;
    }
    index.entry(stem.to_string()).or_default().push(Found { path: path.to_path_buf(), theme_rank, size });
}

/// A theme folder's icon size: `48x48`, `256x256@2` (doubled), `scalable`.
fn size_of_dir(component: &str) -> Option<u32> {
    if component == "scalable" {
        return Some(u32::MAX);
    }
    let (size, scale) = component.split_once('@').unwrap_or((component, "1"));
    let (width, height) = size.split_once('x')?;
    let width: u32 = width.parse().ok()?;
    (height.parse::<u32>().ok()? == width).then(|| width.saturating_mul(scale.parse().unwrap_or(1)))
}

/// The best file for `name`: the first theme that has it, then the
/// smallest size at least `ICON_PX`, else the largest.
fn find_icon(name: &str) -> Option<PathBuf> {
    let found = icon_index().get(name)?;
    let theme = found.iter().map(|f| f.theme_rank).min()?;
    let in_theme = || found.iter().filter(move |f| f.theme_rank == theme);
    in_theme()
        .filter(|f| f.size >= ICON_PX)
        .min_by_key(|f| f.size)
        .or_else(|| in_theme().max_by_key(|f| f.size))
        .map(|f| f.path.clone())
}

/// A PNG of the icon file at `ICON_PX`.
fn draw(file: &Path) -> Option<Vec<u8>> {
    let data = std::fs::read(file).ok()?;
    if file.extension().is_some_and(|e| e == "svg") {
        return png_of(&draw_svg(&data)?);
    }
    png_of(&image::load_from_memory(&data).ok()?.to_rgba8())
}

fn draw_svg(data: &[u8]) -> Option<image::RgbaImage> {
    let tree = usvg::Tree::from_data(data, &usvg::Options::default()).ok()?;
    let size = tree.size();
    let scale = ICON_PX_F32 / size.width().max(size.height());
    let mut pixmap = tiny_skia::Pixmap::new(ICON_PX, ICON_PX)?;
    resvg::render(&tree, tiny_skia::Transform::from_scale(scale, scale), &mut pixmap.as_mut());
    // tiny-skia keeps premultiplied alpha; PNG wants it straight.
    let pixels = pixmap.pixels().iter().flat_map(|p| {
        let c = p.demultiply();
        [c.red(), c.green(), c.blue(), c.alpha()]
    });
    image::RgbaImage::from_raw(ICON_PX, ICON_PX, pixels.collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desktop_files_name_their_icon() {
        let desktop = "[Desktop Action new]\nIcon=wrong\n[Desktop Entry]\nName=Files\nIcon=org.gnome.Nautilus\n";
        assert_eq!(desktop_icon(desktop).as_deref(), Some("org.gnome.Nautilus"), "the entry group's Icon");
        assert_eq!(desktop_icon("[Desktop Entry]\nName=x\n"), None, "no Icon line");
    }

    #[test]
    fn extensions_map_to_mime_types() {
        let globs = "# comment\n50:text/x-python:*.py\n50:text/x-csrc:*.c\n60:text/x-chdr:*.C:cs\n55:image/png:*.PNG\n";
        assert_eq!(mime_for_extension(globs, "py").as_deref(), Some("text/x-python"), "plain glob");
        assert_eq!(mime_for_extension(globs, "png").as_deref(), Some("image/png"), "case ignored");
        assert_eq!(mime_for_extension(globs, "c").as_deref(), Some("text/x-csrc"), "case-sensitive glob skipped");
        assert_eq!(mime_for_extension(globs, "zzz"), None, "unknown");
        let generic = "text/x-python:text-x-script\nimage/png:image-x-generic\n";
        assert_eq!(generic_icon(generic, "text/x-python").as_deref(), Some("text-x-script"), "generic icon");
        assert_eq!(generic_icon(generic, "text/x-py"), None, "no prefix match");
    }

    #[test]
    fn theme_folder_sizes() {
        assert_eq!(size_of_dir("48x48"), Some(48), "plain");
        assert_eq!(size_of_dir("32x32@2"), Some(64), "doubled");
        assert_eq!(size_of_dir("scalable"), Some(u32::MAX), "SVG");
        assert_eq!(size_of_dir("apps"), None, "not a size");
        assert_eq!(size_of_dir("16x24"), None, "not square");
    }

    #[test]
    fn svg_icons_are_drawn() {
        let svg = br##"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16"><rect width="16" height="16" fill="#4f8cff"/></svg>"##;
        let image = draw_svg(svg).expect("drawn");
        assert_eq!(image.dimensions(), (ICON_PX, ICON_PX), "drawn at the icon size");
        assert_eq!(image.get_pixel(32, 32).0, [0x4f, 0x8c, 0xff, 0xff], "filled");
    }
}
