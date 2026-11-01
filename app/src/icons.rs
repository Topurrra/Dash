//! Line icons (24×24, 2px strokes, after the Lucide set — ISC licence),
//! built into the binary and drawn in the current text colour.

use std::borrow::Cow;

use gpui::{prelude::*, px, svg, AssetSource, Hsla, Result, SharedString, Svg};

#[derive(Clone, Copy)]
pub enum Icon {
    Search,
    Clipboard,
    Settings,
    Folder,
    File,
    Music,
    Play,
    Pause,
    Back,
    Forward,
    Volume,
    Muted,
    Wrap,
    Copy,
    Pin,
    Up,
    Down,
    Panel,
    Close,
    Plus,
    Power,
    Table,
    List,
    Grid,
    Rows,
    Image,
}

impl Icon {
    fn name(self) -> &'static str {
        match self {
            Icon::Search => "search",
            Icon::Clipboard => "clipboard",
            Icon::Settings => "settings",
            Icon::Folder => "folder",
            Icon::File => "file",
            Icon::Music => "music",
            Icon::Play => "play",
            Icon::Pause => "pause",
            Icon::Back => "back",
            Icon::Forward => "forward",
            Icon::Volume => "volume",
            Icon::Muted => "muted",
            Icon::Wrap => "wrap",
            Icon::Copy => "copy",
            Icon::Pin => "pin",
            Icon::Up => "up",
            Icon::Down => "down",
            Icon::Panel => "panel",
            Icon::Close => "close",
            Icon::Plus => "plus",
            Icon::Power => "power",
            Icon::Table => "table",
            Icon::List => "list",
            Icon::Grid => "grid",
            Icon::Rows => "rows",
            Icon::Image => "image",
        }
    }

    fn body(name: &str) -> Option<&'static str> {
        Some(match name {
            "search" => r#"<circle cx="11" cy="11" r="7"/><path d="m20 20-3.5-3.5"/>"#,
            "clipboard" => r#"<rect width="8" height="4" x="8" y="2" rx="1"/><path d="M16 4h2a2 2 0 0 1 2 2v14a2 2 0 0 1-2 2H6a2 2 0 0 1-2-2V6a2 2 0 0 1 2-2h2"/>"#,
            "settings" => r#"<path d="M20 7h-9M14 17H5"/><circle cx="17" cy="17" r="3"/><circle cx="7" cy="7" r="3"/>"#,
            "folder" => r#"<path d="M20 20a2 2 0 0 0 2-2V8a2 2 0 0 0-2-2h-7.9a2 2 0 0 1-1.69-.9L9.6 3.9A2 2 0 0 0 7.93 3H4a2 2 0 0 0-2 2v13a2 2 0 0 0 2 2Z"/>"#,
            "file" => r#"<path d="M15 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V7Z"/><path d="M14 2v4a2 2 0 0 0 2 2h4"/>"#,
            "music" => r#"<path d="M9 18V5l12-2v13"/><circle cx="6" cy="18" r="3"/><circle cx="18" cy="16" r="3"/>"#,
            "play" => r#"<path d="M7 4.5v15a1 1 0 0 0 1.5.86l12.5-7.5a1 1 0 0 0 0-1.72L8.5 3.64A1 1 0 0 0 7 4.5Z" fill="black"/>"#,
            "pause" => r#"<rect x="6" y="4" width="4" height="16" rx="1" fill="black"/><rect x="14" y="4" width="4" height="16" rx="1" fill="black"/>"#,
            "back" => r#"<path d="M3 12a9 9 0 1 0 9-9 9.75 9.75 0 0 0-6.74 2.74L3 8"/><path d="M3 3v5h5"/>"#,
            "forward" => r#"<path d="M21 12a9 9 0 1 1-9-9c2.52 0 4.93 1 6.74 2.74L21 8"/><path d="M21 3v5h-5"/>"#,
            "volume" => r#"<path d="M11 5 6 9H2v6h4l5 4V5Z"/><path d="M15.5 8.5a5 5 0 0 1 0 7"/><path d="M19 5a10 10 0 0 1 0 14"/>"#,
            "muted" => r#"<path d="M11 5 6 9H2v6h4l5 4V5Z"/><path d="m22 9-6 6M16 9l6 6"/>"#,
            "wrap" => r#"<path d="M3 6h18M3 18h7"/><path d="M3 12h15a3 3 0 1 1 0 6h-4"/><path d="m16 16-2 2 2 2"/>"#,
            "copy" => r#"<rect width="14" height="14" x="8" y="8" rx="2"/><path d="M4 16a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h10a2 2 0 0 1 2 2"/>"#,
            "pin" => r#"<path d="M12 17v5"/><path d="M5 17h14v-1.8a2 2 0 0 0-1.1-1.8l-1.8-.9a2 2 0 0 1-1.1-1.8V6h1a2 2 0 0 0 0-4H8a2 2 0 0 0 0 4h1v4.7a2 2 0 0 1-1.1 1.8l-1.8.9A2 2 0 0 0 5 15.2Z"/>"#,
            "up" => r#"<path d="m18 15-6-6-6 6"/>"#,
            "down" => r#"<path d="m6 9 6 6 6-6"/>"#,
            "panel" => r#"<rect width="18" height="18" x="3" y="3" rx="2"/><path d="M15 3v18"/>"#,
            "close" => r#"<path d="M18 6 6 18M6 6l12 12"/>"#,
            "plus" => r#"<path d="M12 5v14M5 12h14"/>"#,
            "table" => r#"<rect width="18" height="18" x="3" y="3" rx="2"/><path d="M3 9h18M3 15h18M9 9v12"/>"#,
            "list" => r#"<path d="M9 6h12M9 12h12M9 18h12M4 6h.01M4 12h.01M4 18h.01"/>"#,
            "grid" => r#"<rect width="7" height="7" x="3" y="3" rx="1.5"/><rect width="7" height="7" x="14" y="3" rx="1.5"/><rect width="7" height="7" x="14" y="14" rx="1.5"/><rect width="7" height="7" x="3" y="14" rx="1.5"/>"#,
            "rows" => r#"<path d="M3 5h18M3 9.67h18M3 14.33h18M3 19h18"/>"#,
            "image" => r#"<rect width="18" height="18" x="3" y="3" rx="2"/><circle cx="9" cy="9" r="2"/><path d="m21 15-3.1-3.1a2 2 0 0 0-2.8 0L6 21"/>"#,
            "power" => r#"<path d="M12 2v10"/><path d="M18.4 6.6a9 9 0 1 1-12.77.04"/>"#,
            _ => return None,
        })
    }
}

/// The icon at `size` px, in `color`.
pub fn icon(icon: Icon, size: f32, color: impl Into<Hsla>) -> Svg {
    svg().path(SharedString::from(format!("icons/{}.svg", icon.name()))).size(px(size)).flex_none().text_color(color.into())
}

/// Serves `icons/<name>.svg`.
pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        let Some(body) = path.strip_prefix("icons/").and_then(|p| p.strip_suffix(".svg")).and_then(Icon::body) else {
            return Ok(None);
        };
        let svg = format!(
            r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="#000" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">{body}</svg>"##
        );
        Ok(Some(Cow::Owned(svg.into_bytes())))
    }

    fn list(&self, _path: &str) -> Result<Vec<SharedString>> {
        Ok(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_icon_loads() {
        for icon in [Icon::Search, Icon::Clipboard, Icon::Settings, Icon::Folder, Icon::File, Icon::Music, Icon::Play, Icon::Pause, Icon::Back, Icon::Forward, Icon::Volume, Icon::Muted, Icon::Wrap, Icon::Copy, Icon::Pin, Icon::Up, Icon::Down, Icon::Panel, Icon::Close, Icon::Plus, Icon::Power, Icon::Table, Icon::List, Icon::Grid, Icon::Rows, Icon::Image] {
            let bytes = Assets.load(&format!("icons/{}.svg", icon.name())).unwrap().unwrap();
            assert!(std::str::from_utf8(&bytes).unwrap().ends_with("</svg>"));
        }
        assert!(Assets.load("icons/nope.svg").unwrap().is_none());
    }
}
