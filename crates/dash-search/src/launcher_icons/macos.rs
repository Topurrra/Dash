//! Icons on macOS: NSWorkspace's icon for a path (apps, files) or for a
//! file type (folders, extensions). It comes as a TIFF with one page per
//! size; the page nearest above `ICON_PX` is used.

use super::{png_of, ExtractMode, ICON_PX};
use objc2::rc::Retained;
use objc2_app_kit::{NSImage, NSWorkspace};
use objc2_foundation::NSString;
use std::io::Cursor;
use tiff::decoder::{Decoder, DecodingResult};
use tiff::ColorType;

pub(super) fn extract_icon_png(path: &str, mode: &ExtractMode) -> Option<Vec<u8>> {
    let workspace = NSWorkspace::sharedWorkspace();
    let icon = match mode {
        ExtractMode::RealPath => workspace.iconForFile(&NSString::from_str(path)),
        ExtractMode::Folder => type_icon(&workspace, "public.folder"),
        ExtractMode::Extension(extension) => type_icon(&workspace, extension),
        ExtractMode::Packaged => return None,
    };
    png_of(&best_page(&icon.TIFFRepresentation()?.to_vec())?)
}

#[expect(deprecated, reason = "iconForFileType still works; iconForContentType needs another framework for one call")]
fn type_icon(workspace: &NSWorkspace, file_type: &str) -> Retained<NSImage> {
    workspace.iconForFileType(&NSString::from_str(file_type))
}

/// The smallest page at least `ICON_PX` wide, else the largest.
fn best_page(tiff: &[u8]) -> Option<image::RgbaImage> {
    let mut decoder = Decoder::new(Cursor::new(tiff)).ok()?;
    let mut sizes = Vec::new();
    loop {
        sizes.push(decoder.dimensions().ok()?.0);
        if !decoder.more_images() {
            break;
        }
        decoder.next_image().ok()?;
    }
    let pick = (0..sizes.len())
        .filter(|&i| sizes[i] >= ICON_PX)
        .min_by_key(|&i| sizes[i])
        .or_else(|| (0..sizes.len()).max_by_key(|&i| sizes[i]))?;
    decoder.seek_to_image(pick).ok()?;
    let (width, height) = decoder.dimensions().ok()?;
    let color = decoder.colortype().ok()?;
    let DecodingResult::U8(pixels) = decoder.read_image().ok()? else {
        return None;
    };
    match color {
        ColorType::RGBA(8) => image::RgbaImage::from_raw(width, height, pixels),
        ColorType::RGB(8) => {
            image::RgbImage::from_raw(width, height, pixels).map(image::DynamicImage::ImageRgb8).map(|i| i.to_rgba8())
        }
        _ => None,
    }
}
