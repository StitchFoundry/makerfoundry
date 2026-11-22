//! The picture a shared link to the home page shows (`og:image`): 1200x630, drawn in code from the
//! site's mark colours (an original drawing, so it needs no row in `ASSETS.md`). It has no text;
//! the link's title and description carry the words.

use std::io::Cursor;
use std::sync::OnceLock;

use image::{ImageFormat, Rgb, RgbImage};

/// The card's size, the 1.91:1 shape Facebook and Pinterest crop to.
pub const WIDTH: u32 = 1200;
pub const HEIGHT: u32 = 630;

const TERRACOTTA: Rgb<u8> = Rgb([0xb4, 0x46, 0x2a]);
const CREAM: Rgb<u8> = Rgb([0xfb, 0xfa, 0xf7]);
const GOLD: Rgb<u8> = Rgb([0xff, 0xd1, 0x66]);

/// Fills the rectangle at (`x`, `y`), clipped to the picture.
fn fill(img: &mut RgbImage, x: u32, y: u32, w: u32, h: u32, colour: Rgb<u8>) {
    for py in y..y.saturating_add(h).min(HEIGHT) {
        for px in x..x.saturating_add(w).min(WIDTH) {
            if let Some(p) = img.get_pixel_mut_checked(px, py) {
                *p = colour;
            }
        }
    }
}

/// The three crafts on the site's colour: a dashed thread (embroidery), a beam over a cut
/// (laser), and a stack of printed layers (3D printing).
fn draw() -> RgbImage {
    let mut img = RgbImage::from_pixel(WIDTH, HEIGHT, TERRACOTTA);
    // thread: dashes along a line, left third
    let mut x = 90;
    while x < 400 {
        fill(&mut img, x, 300, 44, 16, GOLD);
        x += 64;
    }
    // laser head and beam, middle
    fill(&mut img, 520, 110, 160, 100, CREAM);
    fill(&mut img, 580, 210, 40, 40, CREAM);
    fill(&mut img, 592, 250, 16, 230, GOLD);
    fill(&mut img, 480, 480, 240, 16, CREAM);
    // printed layers, right third
    for (i, w) in [(0u32, 260u32), (1, 220), (2, 180), (3, 140)] {
        fill(&mut img, 820 + (260 - w) / 2, 460 - i * 56, w, 44, CREAM);
    }
    img
}

/// The card as PNG bytes, drawn once. `None` only if encoding failed.
pub fn png() -> Option<&'static [u8]> {
    static CARD: OnceLock<Option<Vec<u8>>> = OnceLock::new();
    CARD.get_or_init(|| {
        let mut out = Cursor::new(Vec::new());
        draw().write_to(&mut out, ImageFormat::Png).ok()?;
        Some(out.into_inner())
    })
    .as_deref()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_card_is_a_1200_by_630_png() {
        let bytes = png().expect("it encodes");
        let img = image::load_from_memory_with_format(bytes, ImageFormat::Png).expect("it decodes");
        assert_eq!((img.width(), img.height()), (1200, 630));
    }
}
