//! Pictures of what's on the screens, with GDI. That's slower than the graphics card's own
//! ways of doing this, but they're for a program that's watching all the time. This is only
//! for a look every so often, and it needs no setting up for whichever graphics card a screen
//! is on, which matters on a laptop with two of them.

use std::ptr;

use hk_core::layout::Monitor;
use hk_core::screens::ScreenImage;
use windows::Win32::Foundation::POINT;
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CreateCompatibleDC, CreateDIBSection,
    DIB_RGB_COLORS, DeleteDC, DeleteObject, GdiFlush, GetDC, HBITMAP, HDC, HGDIOBJ, ReleaseDC,
    SRCCOPY, SelectObject,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CURSOR_SHOWING, CURSORINFO, DI_NORMAL, DrawIconEx, GetCursorInfo, GetIconInfo, ICONINFO,
};

use super::session;
use crate::Error;

/// Takes the picture, if what's in front is the user's own desktop. The lock screen, an
/// administrator prompt and a screen saver are on other desktops, which Windows mostly refuses
/// to show anyway. This makes sure of it, as those can show things that aren't for sharing.
pub fn take(monitor: &Monitor, with_cursor: bool) -> Result<ScreenImage, Error> {
    if !session::screen_in_use() || !session::users_desktop_in_front() {
        return Err(Error::Screen("the screen is locked".to_string()));
    }
    let (left, top) = (monitor.bounds.x as i32, monitor.bounds.y as i32);
    let (width, height) = (monitor.bounds.width as i32, monitor.bounds.height as i32);
    if width <= 0 || height <= 0 {
        return Err(Error::Screen("that screen has no size".to_string()));
    }

    let screen = ScreenDc::get()?;
    let target = Target::new(&screen, width, height)?;
    // Not CAPTUREBLT, which also takes windows drawn in layers: it's known to make the cursor
    // flicker for someone using the computer, every time.
    // SAFETY: both device contexts are valid, and the area is the size of the target.
    unsafe {
        BitBlt(
            target.dc,
            0,
            0,
            width,
            height,
            Some(screen.0),
            left,
            top,
            SRCCOPY,
        )
    }
    .map_err(|error| Error::Screen(error.to_string()))?;
    if with_cursor {
        draw_cursor(target.dc, left, top);
    }
    Ok(ScreenImage {
        width: width as u32,
        height: height as u32,
        bgra: target.pixels(),
    })
}

/// Draws the cursor where it is, as taking the picture leaves it out. Going without is fine if
/// any of this fails, and it's hidden while it's parked or in use by another computer.
fn draw_cursor(target: HDC, left: i32, top: i32) {
    let mut info = CURSORINFO {
        cbSize: size_of::<CURSORINFO>() as u32,
        ..Default::default()
    };
    // SAFETY: the size is set as Windows asks, and it fills in the rest.
    if unsafe { GetCursorInfo(&raw mut info) }.is_err() || info.flags != CURSOR_SHOWING {
        return;
    }
    let mut icon = ICONINFO::default();
    // SAFETY: the handle comes from Windows, and the result is a plain structure to fill in.
    if unsafe { GetIconInfo(info.hCursor.into(), &raw mut icon) }.is_err() {
        return;
    }
    let at = POINT {
        x: info.ptScreenPos.x - left - icon.xHotspot as i32,
        y: info.ptScreenPos.y - top - icon.yHotspot as i32,
    };
    // SAFETY: the handles are valid. Windows made these two bitmaps for the caller to delete.
    unsafe {
        let _ = DrawIconEx(
            target,
            at.x,
            at.y,
            info.hCursor.into(),
            0,
            0,
            0,
            None,
            DI_NORMAL,
        );
        let _ = DeleteObject(icon.hbmMask.into());
        if !icon.hbmColor.is_invalid() {
            let _ = DeleteObject(icon.hbmColor.into());
        }
    }
}

/// The whole desktop, to read from.
struct ScreenDc(HDC);

impl ScreenDc {
    fn get() -> Result<Self, Error> {
        // SAFETY: no window means the whole desktop.
        let dc = unsafe { GetDC(None) };
        if dc.is_invalid() {
            return Err(Error::Screen("can't read the screen".to_string()));
        }
        Ok(Self(dc))
    }
}

impl Drop for ScreenDc {
    fn drop(&mut self) {
        // SAFETY: this came from GetDC for no window.
        unsafe { ReleaseDC(None, self.0) };
    }
}

/// Memory to draw the picture in.
struct Target {
    dc: HDC,
    bitmap: HBITMAP,
    previous: HGDIOBJ,
    bits: *const u8,
    length: usize,
}

impl Target {
    fn new(screen: &ScreenDc, width: i32, height: i32) -> Result<Self, Error> {
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                // Negative means the top row comes first.
                biHeight: -height,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        // SAFETY: the structure above describes the bitmap asked for, and Windows hands back
        // where its pixels are. Everything made here is released in Drop, or just below it.
        unsafe {
            let dc = CreateCompatibleDC(Some(screen.0));
            if dc.is_invalid() {
                return Err(Error::Screen("out of drawing resources".to_string()));
            }
            let mut bits = ptr::null_mut();
            let bitmap = match CreateDIBSection(
                Some(dc),
                &raw const info,
                DIB_RGB_COLORS,
                &raw mut bits,
                None,
                0,
            ) {
                Ok(bitmap) if !bits.is_null() => bitmap,
                _ => {
                    let _ = DeleteDC(dc);
                    return Err(Error::Screen("out of memory for the picture".to_string()));
                }
            };
            let previous = SelectObject(dc, bitmap.into());
            Ok(Self {
                dc,
                bitmap,
                previous,
                bits: bits.cast_const().cast(),
                length: width as usize * height as usize * 4,
            })
        }
    }

    fn pixels(&self) -> Vec<u8> {
        // SAFETY: Windows made the bitmap this size, with 32 bits for each pixel and no gaps
        // between rows. It stays until Drop, and flushing means it's finished drawing in it.
        unsafe {
            let _ = GdiFlush();
            std::slice::from_raw_parts(self.bits, self.length).to_vec()
        }
    }
}

impl Drop for Target {
    fn drop(&mut self) {
        // SAFETY: these were all made in `new`, and the bitmap is put back out of the device
        // context before it's deleted, as it has to be.
        unsafe {
            SelectObject(self.dc, self.previous);
            let _ = DeleteObject(self.bitmap.into());
            let _ = DeleteDC(self.dc);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use hk_core::screens::ScreenImage;
    use windows::Win32::Graphics::Gdi::GetPixel;

    use super::*;
    use crate::imp::displays;

    /// Reads the real screens, and only prints numbers: nothing of what's on them is kept or
    /// shown. Run on demand, with the screen unlocked:
    /// `cargo test -p hk-platform --release pictures_of_this -- --ignored --nocapture`
    #[test]
    #[ignore = "reads what's on this computer's screens"]
    fn pictures_of_this_computers_screens_match_what_windows_says_is_there() {
        // As the agent does, without which Windows hides how many pixels there really are.
        let _platform = crate::imp::native().unwrap();
        let monitors = displays::enumerate();
        assert!(!monitors.is_empty());
        for monitor in &monitors {
            let start = Instant::now();
            let image = take(monitor, false).unwrap();
            let took = start.elapsed();
            assert!(image.is_valid());
            assert_eq!(f64::from(image.width), monitor.bounds.width);
            assert_eq!(f64::from(image.height), monitor.bounds.height);

            let (matching, checked) = pixels_matching_the_screen(&image, monitor);
            let distinct = distinct_colours(&image);
            let start = Instant::now();
            let scaled = image.clone().scaled_to_fit(1920, 1200);
            let scaling = start.elapsed();
            println!(
                "{}: {}x{} in {took:?}, {distinct} colours among samples, \
                 {matching} of {checked} samples match, scaled to {}x{} in {scaling:?}",
                monitor.name, image.width, image.height, scaled.width, scaled.height,
            );
            let start = Instant::now();
            for _ in 0..10 {
                take(monitor, true).unwrap();
            }
            println!("10 more, with the cursor, in {:?}", start.elapsed());

            // Reading the samples takes seconds, and the screen can change in that time. But
            // most of them not matching would be a picture of something else, or one with its
            // colours or rows the wrong way round.
            assert!(matching * 2 >= checked, "{matching} of {checked}");
            assert!(distinct > 1, "a picture of one flat colour");
        }
    }

    fn pixels_matching_the_screen(image: &ScreenImage, monitor: &Monitor) -> (usize, usize) {
        let screen = ScreenDc::get().unwrap();
        let (mut matching, mut checked) = (0, 0);
        for row in 1..20 {
            for column in 1..20 {
                let (x, y) = (image.width * column / 20, image.height * row / 20);
                let at = ((y * image.width + x) * 4) as usize;
                let (blue, green, red) = (image.bgra[at], image.bgra[at + 1], image.bgra[at + 2]);
                // SAFETY: the device context is valid. This only reads one pixel.
                let color = unsafe {
                    GetPixel(
                        screen.0,
                        monitor.bounds.x as i32 + x as i32,
                        monitor.bounds.y as i32 + y as i32,
                    )
                };
                // 0x00BBGGRR, which is the other way round from the picture.
                let want = u32::from(red) | u32::from(green) << 8 | u32::from(blue) << 16;
                checked += 1;
                if color.0 == want {
                    matching += 1;
                }
            }
        }
        (matching, checked)
    }

    fn distinct_colours(image: &ScreenImage) -> usize {
        let mut seen: Vec<&[u8; 4]> = image.bgra.as_chunks::<4>().0.iter().step_by(997).collect();
        seen.sort_unstable();
        seen.dedup();
        seen.len()
    }
}
