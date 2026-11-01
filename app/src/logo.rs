//! Dash's logo, drawn in code at any size: a black rounded square with a
//! white magnifier and two speed lines. The tray uses it directly;
//! `installer/dash.ico` is written from it (the ignored test below).

/// The logo as RGBA pixels, `size` × `size`.
pub fn rgba(size: u32) -> Vec<u8> {
    // Each pixel averages 4×4 samples, for smooth edges at small sizes.
    const SUB: u32 = 4;
    let mut out = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let (mut tile, mut mark) = (0f32, 0f32);
            for sy in 0..SUB {
                for sx in 0..SUB {
                    let px = (x as f32 + (sx as f32 + 0.5) / SUB as f32) / size as f32;
                    let py = (y as f32 + (sy as f32 + 0.5) / SUB as f32) / size as f32;
                    if rounded_square(px, py) <= 0. {
                        tile += 1.;
                        if in_mark(px, py) {
                            mark += 1.;
                        }
                    }
                }
            }
            let samples = (SUB * SUB) as f32;
            if tile == 0. {
                out.extend_from_slice(&[0, 0, 0, 0]);
                continue;
            }
            // White mark on black.
            let white = (255. * mark / tile).round() as u8;
            out.extend_from_slice(&[white, white, white, (255. * tile / samples).round() as u8]);
        }
    }
    out
}

/// Signed distance to the tile (negative inside).
fn rounded_square(x: f32, y: f32) -> f32 {
    const HALF: f32 = 0.47;
    const RADIUS: f32 = 0.22;
    let (qx, qy) = ((x - 0.5).abs() - (HALF - RADIUS), (y - 0.5).abs() - (HALF - RADIUS));
    let outside = (qx.max(0.).powi(2) + qy.max(0.).powi(2)).sqrt();
    outside + qx.max(qy).min(0.) - RADIUS
}

fn in_mark(x: f32, y: f32) -> bool {
    let lens = ((x - 0.56).powi(2) + (y - 0.44).powi(2)).sqrt();
    let ring = (lens - 0.185).abs() <= 0.045;
    let handle = segment(x, y, (0.695, 0.575), (0.79, 0.67)) <= 0.05;
    let fast = segment(x, y, (0.17, 0.37), (0.27, 0.37)) <= 0.032 || segment(x, y, (0.13, 0.51), (0.27, 0.51)) <= 0.032;
    ring || handle || fast
}

/// Distance from (x, y) to the segment a–b.
fn segment(x: f32, y: f32, a: (f32, f32), b: (f32, f32)) -> f32 {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let t = (((x - a.0) * dx + (y - a.1) * dy) / (dx * dx + dy * dy)).clamp(0., 1.);
    ((x - a.0 - t * dx).powi(2) + (y - a.1 - t * dy).powi(2)).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corners_are_clear_and_middle_is_drawn() {
        let n = 64;
        let px = |x: u32, y: u32| {
            let i = ((y * n + x) * 4) as usize;
            rgba(n)[i..i + 4].to_vec()
        };
        assert_eq!(px(0, 0)[3], 0, "corner outside the rounded tile");
        assert_eq!(px(32, 58)[3], 255, "tile is opaque");
        // On the lens ring: white.
        assert!(px((0.56 * 64.) as u32, ((0.44 - 0.185) * 64.) as u32)[..3].iter().all(|c| *c > 200));
    }

    /// Writes `installer/dash.ico` (16–256 px). Run after changing the logo:
    /// `cargo test -p dash write_installer_icon -- --ignored`.
    #[test]
    #[ignore]
    fn write_installer_icon() {
        use image::codecs::ico::{IcoEncoder, IcoFrame};
        let frames: Vec<IcoFrame> = [16, 20, 24, 32, 40, 48, 64, 256]
            .into_iter()
            .map(|n| IcoFrame::as_png(&rgba(n), n, n, image::ExtendedColorType::Rgba8).unwrap())
            .collect();
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../installer/dash.ico");
        IcoEncoder::new(std::fs::File::create(path).unwrap()).encode_images(&frames).unwrap();
    }
}
