//! Pictures of what's on a screen, to show on the canvas of this device or another.

use std::fmt;

/// The longest side of a picture, whatever is asked for. A picture bigger than this would
/// take too long to prepare and send to look like anything but a slide show.
pub const MAX_PICTURE_SIDE: u32 = 2560;

/// A picture larger than this, from another device, is refused.
pub const MAX_PICTURE_BYTES: usize = 8 * 1024 * 1024;

/// What's on a screen, as it comes from the operating system.
#[derive(Clone, PartialEq, Eq)]
pub struct ScreenImage {
    pub width: u32,
    pub height: u32,
    /// Blue, green, red and an unused byte for each pixel, starting at the top left.
    pub bgra: Vec<u8>,
}

impl fmt::Debug for ScreenImage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ScreenImage({}x{})", self.width, self.height)
    }
}

/// The size of a picture this big once it fits inside this box. It keeps its shape, isn't
/// made any larger, and doesn't get smaller than a pixel in either direction.
pub fn fitted_size(width: u32, height: u32, max_width: u32, max_height: u32) -> (u32, u32) {
    let (width, height) = (width.max(1), height.max(1));
    let (max_width, max_height) = (max_width.max(1), max_height.max(1));
    if width <= max_width && height <= max_height {
        return (width, height);
    }
    let by_width = f64::from(max_width) / f64::from(width);
    let by_height = f64::from(max_height) / f64::from(height);
    let scale = by_width.min(by_height);
    let fitted = |side: u32, limit: u32| ((f64::from(side) * scale).round() as u32).clamp(1, limit);
    (fitted(width, max_width), fitted(height, max_height))
}

impl ScreenImage {
    /// Whether the pixels are all there.
    pub fn is_valid(&self) -> bool {
        self.width > 0 && self.height > 0 && {
            let pixels = u64::from(self.width) * u64::from(self.height);
            self.bgra.len() as u64 == pixels * 4
        }
    }

    /// This picture made small enough to fit in the box. Each new pixel is the average of the
    /// ones it covers, which keeps small text looking like text for longer than picking every
    /// few pixels would.
    pub fn scaled_to_fit(self, max_width: u32, max_height: u32) -> Self {
        let (width, height) = fitted_size(self.width, self.height, max_width, max_height);
        if (width, height) == (self.width, self.height) {
            return self;
        }
        let (from_width, from_height) = (self.width as usize, self.height as usize);
        let (to_width, to_height) = (width as usize, height as usize);
        let from_stride = from_width * 4;
        let columns: Vec<(usize, usize)> = (0..to_width)
            .map(|x| covered(x, from_width, to_width))
            .collect();

        let mut scaled = vec![0; to_width * to_height * 4];
        for (y, row) in scaled.chunks_exact_mut(to_width * 4).enumerate() {
            let (top, bottom) = covered(y, from_height, to_height);
            let rows = &self.bgra[top * from_stride..bottom * from_stride];
            for (pixel, &(left, right)) in row.as_chunks_mut::<4>().0.iter_mut().zip(&columns) {
                let mut sum = [0u64; 3];
                for source_row in rows.chunks_exact(from_stride) {
                    for source in source_row[left * 4..right * 4].as_chunks::<4>().0 {
                        sum[0] += u64::from(source[0]);
                        sum[1] += u64::from(source[1]);
                        sum[2] += u64::from(source[2]);
                    }
                }
                let count = ((bottom - top) * (right - left)) as u64;
                pixel[0] = (sum[0] / count) as u8;
                pixel[1] = (sum[1] / count) as u8;
                pixel[2] = (sum[2] / count) as u8;
            }
        }
        Self {
            width,
            height,
            bgra: scaled,
        }
    }

    /// A number that changes when the picture does, to tell whether it's worth sending again.
    /// Quick rather than secure. It's never zero, which stands for having no picture yet.
    pub fn fingerprint(&self) -> u32 {
        const MULTIPLIER: u64 = 0x517c_c1b7_2722_0a95;
        let mix = |hash: u64, word: u64| (hash.rotate_left(5) ^ word).wrapping_mul(MULTIPLIER);
        let mut hash = mix(u64::from(self.width), u64::from(self.height));
        let (words, rest) = self.bgra.as_chunks::<8>();
        for word in words {
            hash = mix(hash, u64::from_le_bytes(*word));
        }
        let mut last = [0; 8];
        last[..rest.len()].copy_from_slice(rest);
        hash = mix(hash, u64::from_le_bytes(last));
        ((hash >> 32) as u32).max(1)
    }
}

/// Which pixels in a row of `from` one of `to` new pixels covers. There's always at least one.
fn covered(index: usize, from: usize, to: usize) -> (usize, usize) {
    let start = index * from / to;
    let end = (index + 1) * from / to;
    (start, end.max(start + 1).min(from))
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn filled(width: u32, height: u32, pixel: [u8; 4]) -> ScreenImage {
        ScreenImage {
            width,
            height,
            bgra: pixel.repeat((width * height) as usize),
        }
    }

    #[test]
    fn a_picture_keeps_its_shape_and_is_never_made_larger() {
        assert_eq!(fitted_size(1920, 1080, 640, 640), (640, 360));
        assert_eq!(fitted_size(1080, 1920, 640, 640), (360, 640));
        assert_eq!(fitted_size(1920, 1080, 1920, 300), (533, 300));
        assert_eq!(fitted_size(800, 600, 1920, 1080), (800, 600));
        assert_eq!(fitted_size(1920, 1080, 0, 0), (1, 1));
        assert_eq!(fitted_size(10_000, 1, 100, 100), (100, 1));
    }

    #[test]
    fn each_new_pixel_is_the_average_of_what_it_covers() {
        // Two black pixels and two white ones, in two rows, become one black and one white.
        let black = [0, 0, 0, 0];
        let white = [200, 100, 50, 0];
        let row = [black, black, white, white].concat();
        let image = ScreenImage {
            width: 4,
            height: 2,
            bgra: [row.clone(), row].concat(),
        };
        let scaled = image.scaled_to_fit(2, 2);
        assert_eq!((scaled.width, scaled.height), (2, 1));
        assert_eq!(scaled.bgra, [black, white].concat());

        // And a checkerboard becomes the grey in between.
        let image = ScreenImage {
            width: 2,
            height: 2,
            bgra: [black, white, white, black].concat(),
        };
        assert_eq!(image.scaled_to_fit(1, 1).bgra, [100, 50, 25, 0]);
    }

    #[test]
    fn a_picture_that_already_fits_is_left_alone() {
        let image = filled(4, 3, [1, 2, 3, 0]);
        assert_eq!(image.clone().scaled_to_fit(4, 3), image);
        assert_eq!(image.clone().scaled_to_fit(400, 300), image);
    }

    #[test]
    fn the_fingerprint_notices_a_single_pixel() {
        let image = filled(100, 100, [10, 20, 30, 0]);
        let mut changed = image.clone();
        changed.bgra[5000 * 4 + 1] ^= 1;
        assert_eq!(image.fingerprint(), image.clone().fingerprint());
        assert_ne!(image.fingerprint(), changed.fingerprint());
        // The same pixels in another shape are another picture.
        assert_ne!(
            filled(4, 1, [1, 1, 1, 1]).fingerprint(),
            filled(2, 2, [1, 1, 1, 1]).fingerprint()
        );
        assert_ne!(filled(1, 1, [0, 0, 0, 0]).fingerprint(), 0);
    }

    #[test]
    fn the_last_few_bytes_count_towards_the_fingerprint_too() {
        let image = ScreenImage {
            width: 3,
            height: 1,
            bgra: vec![0; 12],
        };
        let mut changed = image.clone();
        changed.bgra[11] = 1;
        assert_ne!(image.fingerprint(), changed.fingerprint());
    }

    #[test]
    fn missing_pixels_are_noticed() {
        assert!(filled(3, 2, [0; 4]).is_valid());
        let mut short = filled(3, 2, [0; 4]);
        short.bgra.pop();
        assert!(!short.is_valid());
        assert!(!filled(0, 2, [0; 4]).is_valid());
    }

    proptest! {
        #[test]
        fn scaling_always_gives_a_whole_picture_of_the_right_size(
            width in 1u32..300,
            height in 1u32..300,
            max_width in 1u32..300,
            max_height in 1u32..300,
        ) {
            let scaled = filled(width, height, [50, 60, 70, 0]).scaled_to_fit(max_width, max_height);
            prop_assert!(scaled.is_valid());
            prop_assert!(scaled.width <= max_width.max(width.min(max_width)));
            prop_assert!(scaled.height <= max_height.max(height.min(max_height)));
            // Averaging the same colour gives that colour, whatever the sizes.
            prop_assert!(scaled.bgra.as_chunks::<4>().0.iter().all(|p| *p == [50, 60, 70, 0]));
        }
    }
}
