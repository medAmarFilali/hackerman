//! Pictures of what's on the screens, for the canvas of this device or another.
//!
//! Nothing here runs unless someone is waiting for a picture. A device that wants another
//! one asks again, so one that goes away, or whose window closes, simply stops asking.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use base64::Engine as _;
use hk_core::layout::Monitor;
use hk_core::screens::{MAX_PICTURE_BYTES, MAX_PICTURE_SIDE};
use hk_ipc::{PictureAnswer, ScreenAsk};
use hk_net::{BlobChannel, DataWriter};
use hk_platform::Platform;
use hk_proto::{ScreenAnswer, ScreenRequest};
use image::ExtendedColorType;
use image::codecs::jpeg::JpegEncoder;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// How many pictures can be waiting to be taken, for all the devices asking together. Past
/// this they're told to ask again later, which limits what a device can make this one do.
pub const PICTURES_WAITING: usize = 8;

/// How long to wait for a picture from another device. It's asked for again after this.
const PICTURE_TIMEOUT: Duration = Duration::from_secs(5);

/// A picture no bigger than this is a small one on the canvas. Text on it can't be read in
/// any case, so it can look a little rougher in exchange for being much smaller to send.
const SMALL_PICTURE_SIDE: u32 = 800;
const SMALL_PICTURE_QUALITY: u8 = 50;
const LARGE_PICTURE_QUALITY: u8 = 65;

/// Keeps pictures from being taken all at once, however many are being asked for.
#[derive(Clone)]
pub struct Limits {
    /// Taken while waiting, and given back once the answer has gone.
    waiting: Arc<Semaphore>,
    /// One picture at a time. Each one keeps a processor core busy.
    taking: Arc<Semaphore>,
}

impl Limits {
    pub fn new() -> Self {
        Self {
            waiting: Arc::new(Semaphore::new(PICTURES_WAITING)),
            taking: Arc::new(Semaphore::new(1)),
        }
    }

    /// A place in the queue, unless it's full.
    pub fn join(&self) -> Option<OwnedSemaphorePermit> {
        self.waiting.clone().try_acquire_owned().ok()
    }
}

/// A picture ready to go, or the news that it hasn't changed.
pub enum Taken {
    Picture {
        width: u16,
        height: u16,
        fingerprint: u32,
        jpeg: Vec<u8>,
    },
    Unchanged,
}

/// Takes a picture of a screen here. This takes a moment, and it's done off to the side.
pub async fn take<P: Platform>(
    platform: P,
    monitor: Monitor,
    request: ScreenRequest,
    limits: &Limits,
) -> Result<Taken> {
    let _turn = limits.taking.clone().acquire_owned().await?;
    tokio::task::spawn_blocking(move || take_now(&platform, &monitor, &request)).await?
}

fn take_now<P: Platform>(
    platform: &P,
    monitor: &Monitor,
    request: &ScreenRequest,
) -> Result<Taken> {
    let image = platform.capture_screen(monitor, request.cursor)?;
    if !image.is_valid() {
        return Err(anyhow!("the picture of the screen came out wrong"));
    }
    let image = image.scaled_to_fit(
        u32::from(request.max_width).min(MAX_PICTURE_SIDE),
        u32::from(request.max_height).min(MAX_PICTURE_SIDE),
    );
    let fingerprint = image.fingerprint();
    if fingerprint == request.known {
        return Ok(Taken::Unchanged);
    }
    let quality = if image.width.max(image.height) <= SMALL_PICTURE_SIDE {
        SMALL_PICTURE_QUALITY
    } else {
        LARGE_PICTURE_QUALITY
    };
    let mut rgb = Vec::with_capacity(image.bgra.len() / 4 * 3);
    for pixel in image.bgra.as_chunks::<4>().0 {
        rgb.extend_from_slice(&[pixel[2], pixel[1], pixel[0]]);
    }
    let mut jpeg = Vec::new();
    JpegEncoder::new_with_quality(&mut jpeg, quality).encode(
        &rgb,
        image.width,
        image.height,
        ExtendedColorType::Rgb8,
    )?;
    Ok(Taken::Picture {
        width: image.width as u16,
        height: image.height as u16,
        fingerprint,
        jpeg,
    })
}

/// Answers another device's request for a picture, on the stream it came on.
pub async fn answer_peer<P: Platform>(
    platform: P,
    monitor: Monitor,
    request: ScreenRequest,
    mut writer: DataWriter,
    limits: Limits,
    place: OwnedSemaphorePermit,
) -> Result<()> {
    let taken = take(platform, monitor, request, &limits).await;
    let result = match taken {
        Ok(Taken::Picture {
            width,
            height,
            fingerprint,
            jpeg,
        }) => {
            write_answer(
                &mut writer,
                &ScreenAnswer::Picture {
                    width,
                    height,
                    fingerprint,
                },
            )
            .await?;
            writer.write(&jpeg).await.map_err(Into::into)
        }
        Ok(Taken::Unchanged) => write_answer(&mut writer, &ScreenAnswer::Unchanged).await,
        Err(error) => {
            tracing::debug!(%error, "no picture to send");
            write_answer(&mut writer, &ScreenAnswer::Unavailable).await
        }
    };
    drop(place);
    result?;
    writer.finish()?;
    Ok(())
}

/// Turns another device's request down.
pub async fn refuse(mut writer: DataWriter, answer: ScreenAnswer) -> Result<()> {
    write_answer(&mut writer, &answer).await?;
    writer.finish()?;
    Ok(())
}

async fn write_answer(writer: &mut DataWriter, answer: &ScreenAnswer) -> Result<()> {
    writer
        .write_message(&hk_proto::encode_screen_answer(answer)?)
        .await?;
    Ok(())
}

/// Takes a picture here for the app, without any need to ask anyone.
pub async fn answer_local<P: Platform>(
    platform: P,
    monitor: Monitor,
    ask: ScreenAsk,
    limits: Limits,
    place: OwnedSemaphorePermit,
) -> PictureAnswer {
    let taken = take(platform, monitor, request_for(ask), &limits).await;
    drop(place);
    match taken {
        Ok(Taken::Picture {
            width,
            height,
            fingerprint,
            jpeg,
        }) => PictureAnswer::Picture {
            url: picture_url(&jpeg),
            width,
            height,
            fingerprint,
        },
        Ok(Taken::Unchanged) => PictureAnswer::Unchanged,
        Err(error) => {
            tracing::debug!(%error, "no picture to show");
            PictureAnswer::Unavailable
        }
    }
}

/// Asks another device for a picture, for the app. Whatever goes wrong, there just isn't one
/// this time. It's the app's next request that gets another.
pub async fn ask_peer(blobs: BlobChannel, ask: ScreenAsk) -> PictureAnswer {
    let request = request_for(ask);
    match tokio::time::timeout(PICTURE_TIMEOUT, ask_peer_for(blobs, request)).await {
        Ok(Ok(answer)) => answer,
        Ok(Err(error)) => {
            tracing::debug!(%error, "no picture from another device");
            PictureAnswer::Unavailable
        }
        Err(_) => PictureAnswer::Unavailable,
    }
}

async fn ask_peer_for(blobs: BlobChannel, request: ScreenRequest) -> Result<PictureAnswer> {
    let bytes = hk_proto::encode_screen_request(&request)?;
    let mut reader = blobs.request(&bytes).await?;
    let answer = hk_proto::decode_screen_answer(&reader.read_message().await?)?;
    Ok(match answer {
        ScreenAnswer::Picture {
            width,
            height,
            fingerprint,
        } => {
            let mut jpeg = Vec::new();
            let mut piece = vec![0; 64 * 1024];
            loop {
                let count = reader.read(&mut piece).await?;
                if count == 0 {
                    break;
                }
                if jpeg.len() + count > MAX_PICTURE_BYTES {
                    return Err(anyhow!("that picture is far too large"));
                }
                jpeg.extend_from_slice(&piece[..count]);
            }
            // It goes into the page as a picture, so it had better say what it is.
            if !jpeg.starts_with(&[0xFF, 0xD8, 0xFF]) {
                return Err(anyhow!("that isn't a picture"));
            }
            PictureAnswer::Picture {
                url: picture_url(&jpeg),
                width,
                height,
                fingerprint,
            }
        }
        ScreenAnswer::Unchanged => PictureAnswer::Unchanged,
        ScreenAnswer::NotAllowed => PictureAnswer::NotAllowed,
        ScreenAnswer::Unavailable => PictureAnswer::Unavailable,
        ScreenAnswer::Busy => PictureAnswer::Busy,
    })
}

fn request_for(ask: ScreenAsk) -> ScreenRequest {
    ScreenRequest {
        monitor: ask.monitor,
        max_width: ask.max_width,
        max_height: ask.max_height,
        known: ask.known,
        cursor: ask.cursor,
    }
}

/// A picture, as the app's page can show it.
fn picture_url(jpeg: &[u8]) -> String {
    let encoded = base64::engine::general_purpose::STANDARD.encode(jpeg);
    format!("data:image/jpeg;base64,{encoded}")
}

#[cfg(test)]
mod tests {
    use hk_core::geometry::Rect;
    use hk_core::screens::ScreenImage;
    use hk_platform::fake::FakePlatform;

    use super::*;

    fn monitor() -> Monitor {
        Monitor {
            id: "m".into(),
            name: "m".into(),
            bounds: Rect::new(0.0, 0.0, 400.0, 200.0),
            scale: 1.0,
            primary: true,
        }
    }

    fn request(max_width: u16, known: u32) -> ScreenRequest {
        ScreenRequest {
            monitor: "m".into(),
            max_width,
            max_height: max_width,
            known,
            cursor: false,
        }
    }

    fn jpeg_of(taken: Taken) -> (u16, u16, Vec<u8>) {
        match taken {
            Taken::Picture {
                width,
                height,
                jpeg,
                ..
            } => (width, height, jpeg),
            Taken::Unchanged => panic!("wanted a picture"),
        }
    }

    /// Windows gives pixels in another order from the one a JPEG holds them in.
    #[test]
    fn colours_come_out_as_they_went_in() {
        let platform = FakePlatform::with_screens(0);
        // Blue, green and red bands, wide enough that blurring at the edges leaves them alone.
        let mut bgra = Vec::new();
        for _y in 0..200 {
            for x in 0..400 {
                bgra.extend_from_slice(match x {
                    0..=132 => &[220, 20, 20, 0],
                    133..=265 => &[20, 220, 20, 0],
                    _ => &[20, 20, 220, 0],
                });
            }
        }
        platform.set_screen_picture(
            "m",
            ScreenImage {
                width: 400,
                height: 200,
                bgra,
            },
        );
        // One of each kind: the small ones save space in a way that the large ones don't.
        for size in [400, 1000] {
            let (width, height, jpeg) =
                jpeg_of(take_now(&platform, &monitor(), &request(size, 0)).unwrap());
            assert_eq!((width, height), (400, 200));
            let decoded = image::load_from_memory_with_format(&jpeg, image::ImageFormat::Jpeg)
                .unwrap()
                .to_rgb8();
            let near = |x: u32, red: u8, green: u8, blue: u8| {
                let pixel = decoded.get_pixel(x, 100).0;
                let close = |a: u8, b: u8| a.abs_diff(b) < 24;
                assert!(
                    close(pixel[0], red) && close(pixel[1], green) && close(pixel[2], blue),
                    "at {x}: {pixel:?}"
                );
            };
            near(60, 20, 20, 220);
            near(200, 20, 220, 20);
            near(340, 220, 20, 20);
        }
    }

    #[test]
    fn a_picture_that_is_the_same_as_before_is_not_made_again() {
        let platform = FakePlatform::with_screens(0);
        let Taken::Picture { fingerprint, .. } =
            take_now(&platform, &monitor(), &request(200, 0)).unwrap()
        else {
            panic!("wanted a picture");
        };
        assert!(matches!(
            take_now(&platform, &monitor(), &request(200, fingerprint)).unwrap(),
            Taken::Unchanged
        ));
        // At another size, it's another picture.
        assert!(matches!(
            take_now(&platform, &monitor(), &request(100, fingerprint)).unwrap(),
            Taken::Picture { .. }
        ));
    }

    #[test]
    fn a_picture_with_pixels_missing_is_turned_away() {
        let platform = FakePlatform::with_screens(0);
        platform.set_screen_picture(
            "m",
            ScreenImage {
                width: 400,
                height: 200,
                bgra: vec![0; 10],
            },
        );
        assert!(take_now(&platform, &monitor(), &request(200, 0)).is_err());
    }
}
