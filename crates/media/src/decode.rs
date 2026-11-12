//! Decoding PNG (incl. APNG), GIF and WebP (static and animated) into BGRA frames.
//!
//! Decoding is CPU-bound and synchronous; [`crate::ImageCache`] runs it on tokio's blocking
//! pool behind a semaphore, never on the async workers. Call [`decode`] directly only from a
//! thread that may block.
//!
//! # Pixel format
//!
//! Every frame is **BGRA8 with straight (non-premultiplied) alpha**, rows tightly packed
//! (stride = `width * 4`), top-left origin, all frames the full canvas size. That is exactly
//! what `gpui-pre 0.3.12`'s `RenderImage` holds: its own decoders take `image`'s straight RGBA
//! and only swap R and B (`platform.rs`, `decode_static_image_from_decoder` /
//! `decode_animated_frames`), and its SVG path un-premultiplies before handing pixels over
//! (`color.rs`, `swap_rgba_pa_to_bgra`). So a [`DecodedFrame`]'s buffer can be moved into an
//! `image::RgbaImage` / `image::Frame` and on into `RenderImage::new` without touching a pixel.

use std::borrow::Cow;
use std::io::Cursor;
use std::time::Duration;

use image::codecs::gif::GifDecoder;
use image::codecs::png::PngDecoder;
use image::codecs::webp::WebPDecoder;
use image::{AnimationDecoder, DynamicImage, ImageDecoder, ImageFormat};

use crate::error::DecodeError;

/// Frame delays below this are treated as "unspecified", like browsers do for GIF
/// (a delay of 0 or 10 ms would otherwise spin at 100 fps or faster).
pub const MIN_FRAME_DELAY: Duration = Duration::from_millis(20);
/// What a delay below [`MIN_FRAME_DELAY`] is replaced with.
pub const CLAMPED_FRAME_DELAY: Duration = Duration::from_millis(100);

/// Bounds that protect against decompression bombs and absurd inputs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecodeLimits {
    /// Larger canvases are rejected with [`DecodeError::TooLarge`].
    pub max_width: u32,
    /// Larger canvases are rejected with [`DecodeError::TooLarge`].
    pub max_height: u32,
    /// Animations are truncated after this many frames.
    pub max_frames: usize,
    /// Decoded pixel bytes over all frames; animations are truncated before exceeding it, and a
    /// single frame that alone exceeds it is rejected.
    pub max_bytes: usize,
}

impl Default for DecodeLimits {
    fn default() -> Self {
        Self {
            max_width: 2048,
            max_height: 2048,
            max_frames: 1024,
            max_bytes: 64 * 1024 * 1024,
        }
    }
}

/// The container format an image was decoded from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SourceFormat {
    Png,
    Gif,
    WebP,
}

/// One frame: BGRA8, straight alpha, `width * height * 4` bytes (see the module docs).
#[derive(Clone, PartialEq, Eq)]
pub struct DecodedFrame {
    pixels: Vec<u8>,
    delay: Duration,
}

impl DecodedFrame {
    /// BGRA8 pixels, straight alpha, tightly packed rows.
    pub fn pixels(&self) -> &[u8] {
        &self.pixels
    }

    /// Takes the pixel buffer without copying (for building a GPUI `RenderImage`).
    pub fn into_pixels(self) -> Vec<u8> {
        self.pixels
    }

    /// How long this frame is shown. Already clamped (see [`MIN_FRAME_DELAY`]). Zero for
    /// still images.
    pub fn delay(&self) -> Duration {
        self.delay
    }
}

impl std::fmt::Debug for DecodedFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DecodedFrame")
            .field("bytes", &self.pixels.len())
            .field("delay", &self.delay)
            .finish()
    }
}

/// Frame timing of an animation, for driving one global animation clock: every image asked
/// with the same `elapsed` shows the frame that a browser started at time zero would show.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Timeline {
    /// End time of each frame since the start of a loop; strictly increasing.
    ends: Vec<Duration>,
}

impl Timeline {
    /// Builds a timeline from per-frame delays (already clamped).
    pub fn new(delays: impl IntoIterator<Item = Duration>) -> Self {
        let mut total = Duration::ZERO;
        let ends = delays
            .into_iter()
            .map(|d| {
                total += d;
                total
            })
            .collect();
        Self { ends }
    }

    /// Number of frames.
    pub fn len(&self) -> usize {
        self.ends.len()
    }

    /// `true` for an image without frames.
    pub fn is_empty(&self) -> bool {
        self.ends.is_empty()
    }

    /// Length of one loop.
    pub fn total(&self) -> Duration {
        self.ends.last().copied().unwrap_or_default()
    }

    /// The frame to show `elapsed` after the global clock started (looping forever), and how
    /// long until the next frame change. Still images return `(0, None)`.
    pub fn frame_at(&self, elapsed: Duration) -> (usize, Option<Duration>) {
        let total = self.total();
        if self.ends.len() < 2 || total.is_zero() {
            return (0, None);
        }
        let total_ns = total.as_nanos();
        let t = Duration::from_nanos((elapsed.as_nanos() % total_ns) as u64);
        // First frame whose end is strictly after `t`.
        let index = self.ends.partition_point(|end| *end <= t);
        let index = index.min(self.ends.len() - 1);
        (index, Some(self.ends[index] - t))
    }
}

/// A decoded still image or animation. Shared as `Arc<DecodedImage>` by [`crate::ImageCache`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedImage {
    width: u32,
    height: u32,
    format: SourceFormat,
    frames: Vec<DecodedFrame>,
    timeline: Timeline,
    truncated: bool,
}

impl DecodedImage {
    fn new(
        width: u32,
        height: u32,
        format: SourceFormat,
        frames: Vec<DecodedFrame>,
        truncated: bool,
    ) -> Self {
        let timeline = Timeline::new(frames.iter().map(|f| f.delay));
        Self {
            width,
            height,
            format,
            frames,
            timeline,
            truncated,
        }
    }

    /// Canvas width in pixels (all frames share it).
    pub fn width(&self) -> u32 {
        self.width
    }

    /// Canvas height in pixels (all frames share it).
    pub fn height(&self) -> u32 {
        self.height
    }

    /// The container format.
    pub fn format(&self) -> SourceFormat {
        self.format
    }

    /// All frames, in display order. Never empty.
    pub fn frames(&self) -> &[DecodedFrame] {
        &self.frames
    }

    /// Takes the frames without copying pixels (for building a GPUI `RenderImage`).
    pub fn into_frames(self) -> Vec<DecodedFrame> {
        self.frames
    }

    /// More than one frame.
    pub fn is_animated(&self) -> bool {
        self.frames.len() > 1
    }

    /// Frame timing for the global animation clock.
    pub fn timeline(&self) -> &Timeline {
        &self.timeline
    }

    /// The animation hit [`DecodeLimits::max_frames`] / [`DecodeLimits::max_bytes`] or had a
    /// broken frame, and only the frames before that point were kept.
    pub fn is_truncated(&self) -> bool {
        self.truncated
    }

    /// Approximate heap + inline size, used for the memory budget.
    pub fn byte_size(&self) -> usize {
        std::mem::size_of::<Self>()
            + self.frames.iter().map(|f| f.pixels.len()).sum::<usize>()
            + self.frames.len() * std::mem::size_of::<DecodedFrame>()
            + self.timeline.ends.len() * std::mem::size_of::<Duration>()
    }
}

/// Decodes PNG/APNG, GIF or WebP bytes into BGRA frames. Blocking and CPU-bound.
pub fn decode(bytes: &[u8], limits: &DecodeLimits) -> Result<DecodedImage, DecodeError> {
    if bytes.is_empty() {
        return Err(DecodeError::Empty);
    }
    let format = image::guess_format(bytes).map_err(|_| DecodeError::UnknownFormat)?;
    let cursor = Cursor::new(bytes);
    match format {
        ImageFormat::Png => {
            let mut decoder = PngDecoder::new(cursor).map_err(malformed)?;
            let (w, h) = check_canvas(&mut decoder, limits)?;
            if decoder.is_apng().map_err(malformed)? {
                let frames = decoder.apng().map_err(malformed)?.into_frames();
                decode_frames(frames, w, h, SourceFormat::Png, limits)
            } else {
                decode_still(decoder, w, h, SourceFormat::Png)
            }
        }
        ImageFormat::Gif => {
            let mut decoder = GifDecoder::new(cursor).map_err(malformed)?;
            let (w, h) = check_canvas(&mut decoder, limits)?;
            decode_frames(decoder.into_frames(), w, h, SourceFormat::Gif, limits)
        }
        ImageFormat::WebP => {
            let bytes = force_webp_animation_alpha(bytes);
            let mut decoder = WebPDecoder::new(Cursor::new(&*bytes)).map_err(malformed)?;
            let (w, h) = check_canvas(&mut decoder, limits)?;
            if decoder.has_animation() {
                // The ANIM chunk's background colour is only a hint; browsers start from a
                // transparent canvas, and emotes rely on that (encoders often write opaque
                // white there).
                decoder
                    .set_background_color(image::Rgba([0, 0, 0, 0]))
                    .map_err(malformed)?;
                decode_frames(decoder.into_frames(), w, h, SourceFormat::WebP, limits)
            } else {
                decode_still(decoder, w, h, SourceFormat::WebP)
            }
        }
        other => Err(DecodeError::Unsupported(
            format!("{other:?}").to_ascii_uppercase().into(),
        )),
    }
}

/// Sets the VP8X "has alpha" flag on animated WebP files that lack it.
///
/// Encoders such as `img2webp -min_size` and `gif2webp` store opaque frames that cover only
/// part of the canvas and leave the flag clear, because no *frame* has alpha. `image` then
/// decodes into RGB and the uncovered canvas comes out opaque. Browsers composite onto a
/// transparent RGBA canvas, so we make `image` do the same. Opaque pixels are unaffected.
fn force_webp_animation_alpha(bytes: &[u8]) -> Cow<'_, [u8]> {
    const FLAGS: usize = 20;
    const ALPHA: u8 = 0x10;
    const ANIMATION: u8 = 0x02;
    let is_vp8x = bytes.len() > FLAGS && bytes[12..16] == *b"VP8X";
    if is_vp8x && bytes[FLAGS] & ANIMATION != 0 && bytes[FLAGS] & ALPHA == 0 {
        let mut patched = bytes.to_vec();
        patched[FLAGS] |= ALPHA;
        Cow::Owned(patched)
    } else {
        Cow::Borrowed(bytes)
    }
}

fn malformed(err: image::ImageError) -> DecodeError {
    match err {
        image::ImageError::Limits(e) => DecodeError::Malformed(format!("limit: {e}").into()),
        other => DecodeError::Malformed(other.to_string().into()),
    }
}

/// Rejects empty or oversized canvases and installs `image`'s own allocation limits.
fn check_canvas(
    decoder: &mut impl ImageDecoder,
    limits: &DecodeLimits,
) -> Result<(u32, u32), DecodeError> {
    let (w, h) = decoder.dimensions();
    if w == 0 || h == 0 {
        return Err(DecodeError::Malformed("zero-sized image".into()));
    }
    let frame_bytes = u64::from(w) * u64::from(h) * 4;
    if w > limits.max_width || h > limits.max_height || frame_bytes > limits.max_bytes as u64 {
        return Err(DecodeError::TooLarge {
            width: w,
            height: h,
        });
    }
    let mut image_limits = image::Limits::default();
    image_limits.max_image_width = Some(limits.max_width);
    image_limits.max_image_height = Some(limits.max_height);
    // Room for the output frame plus the decoder's own scratch buffers.
    image_limits.max_alloc = Some(frame_bytes.saturating_mul(4).max(16 * 1024 * 1024));
    decoder.set_limits(image_limits).map_err(malformed)?;
    Ok((w, h))
}

fn decode_still(
    mut decoder: impl ImageDecoder,
    w: u32,
    h: u32,
    format: SourceFormat,
) -> Result<DecodedImage, DecodeError> {
    let orientation = decoder.orientation().map_err(malformed)?;
    let mut image = DynamicImage::from_decoder(decoder).map_err(malformed)?;
    image.apply_orientation(orientation);
    let rgba = image.into_rgba8();
    // Orientation may swap the axes.
    let (w, h) = if rgba.dimensions() == (w, h) {
        (w, h)
    } else {
        rgba.dimensions()
    };
    let mut pixels = rgba.into_raw();
    rgba_to_bgra(&mut pixels);
    let frame = DecodedFrame {
        pixels,
        delay: Duration::ZERO,
    };
    Ok(DecodedImage::new(w, h, format, vec![frame], false))
}

fn decode_frames(
    frames: image::Frames<'_>,
    w: u32,
    h: u32,
    format: SourceFormat,
    limits: &DecodeLimits,
) -> Result<DecodedImage, DecodeError> {
    let frame_bytes = w as usize * h as usize * 4;
    let mut out: Vec<DecodedFrame> = Vec::new();
    let mut total = 0usize;
    let mut truncated = false;
    for frame in frames {
        if out.len() >= limits.max_frames || total + frame_bytes > limits.max_bytes {
            truncated = true;
            break;
        }
        let frame = match frame {
            Ok(frame) => frame,
            Err(err) if out.is_empty() => return Err(malformed(err)),
            Err(err) => {
                tracing::debug!(%err, frames = out.len(), "animation truncated at a broken frame");
                truncated = true;
                break;
            }
        };
        let delay = clamp_delay(frame.delay());
        let buffer = frame.into_buffer();
        if buffer.dimensions() != (w, h) {
            return Err(DecodeError::Malformed(
                "animation frame does not cover the canvas".into(),
            ));
        }
        let mut pixels = buffer.into_raw();
        rgba_to_bgra(&mut pixels);
        total += pixels.len();
        out.push(DecodedFrame { pixels, delay });
    }
    if out.is_empty() {
        return Err(DecodeError::Malformed("no frames".into()));
    }
    if truncated {
        tracing::debug!(frames = out.len(), "animation truncated by decode limits");
    }
    if out.len() == 1 {
        out[0].delay = Duration::ZERO;
    }
    Ok(DecodedImage::new(w, h, format, out, truncated))
}

/// Browser-style delay clamping: anything shorter than [`MIN_FRAME_DELAY`] becomes
/// [`CLAMPED_FRAME_DELAY`].
pub fn clamp_delay(delay: image::Delay) -> Duration {
    let (numer, denom) = delay.numer_denom_ms();
    let delay = if denom == 0 {
        Duration::ZERO
    } else {
        Duration::from_micros(u64::from(numer) * 1000 / u64::from(denom))
    };
    if delay < MIN_FRAME_DELAY {
        CLAMPED_FRAME_DELAY
    } else {
        delay
    }
}

fn rgba_to_bgra(pixels: &mut [u8]) {
    for px in pixels.as_chunks_mut::<4>().0 {
        px.swap(0, 2);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{fixture, png_bytes};

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    /// Fixture frame `i` is the solid colour `FRAME_COLOURS[i]` with a transparent right half
    /// (see `examples/gen_fixtures.rs`).
    const FRAME_COLOURS: [[u8; 3]; 4] = [[255, 0, 0], [0, 255, 0], [0, 0, 255], [255, 255, 0]];

    fn bgra_at(img: &DecodedImage, frame: usize, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * img.width() + x) * 4) as usize;
        img.frames()[frame].pixels()[i..i + 4].try_into().unwrap()
    }

    fn assert_animation(img: &DecodedImage, format: SourceFormat) {
        assert_eq!(img.format(), format);
        assert_eq!((img.width(), img.height()), (28, 28));
        assert_eq!(img.frames().len(), 4);
        assert!(img.is_animated());
        assert!(!img.is_truncated());
        let delays: Vec<_> = img.frames().iter().map(DecodedFrame::delay).collect();
        // Third frame is stored with a 0 ms / 10 ms delay and must be clamped.
        assert_eq!(delays, vec![ms(100), ms(50), ms(100), ms(30)]);
        for (i, [r, g, b]) in FRAME_COLOURS.into_iter().enumerate() {
            // WebP compositing in `image-webp` may round a channel by one.
            let got = bgra_at(img, i, 2, 2);
            let close = got
                .iter()
                .zip([b, g, r, 255])
                .all(|(a, e)| a.abs_diff(e) <= 1);
            assert!(
                close,
                "frame {i} left half: {got:?} vs {:?}",
                [b, g, r, 255]
            );
            assert_eq!(bgra_at(img, i, 25, 2)[3], 0, "frame {i} right half");
        }
        for f in img.frames() {
            assert_eq!(f.pixels().len(), 28 * 28 * 4);
        }
    }

    #[test]
    fn static_png_is_bgra_straight_alpha() {
        let img = decode(&fixture("static.png"), &DecodeLimits::default()).unwrap();
        assert_eq!(img.format(), SourceFormat::Png);
        assert_eq!((img.width(), img.height()), (28, 28));
        assert_eq!(img.frames().len(), 1);
        assert!(!img.is_animated());
        assert_eq!(img.frames()[0].delay(), Duration::ZERO);
        // (0,0) is RGBA (200, 100, 50, 128): BGRA, not premultiplied.
        assert_eq!(bgra_at(&img, 0, 0, 0), [50, 100, 200, 128]);
        assert_eq!(img.timeline().frame_at(ms(1234)), (0, None));
    }

    #[test]
    fn static_webp() {
        let img = decode(&fixture("static.webp"), &DecodeLimits::default()).unwrap();
        assert_eq!(img.format(), SourceFormat::WebP);
        assert_eq!(img.frames().len(), 1);
        assert_eq!(bgra_at(&img, 0, 0, 0), [50, 100, 200, 128]);
    }

    #[test]
    fn animated_gif_frames_and_clamped_delays() {
        let img = decode(&fixture("animated.gif"), &DecodeLimits::default()).unwrap();
        assert_animation(&img, SourceFormat::Gif);
    }

    #[test]
    fn animated_webp_frames_and_clamped_delays() {
        let img = decode(&fixture("animated.webp"), &DecodeLimits::default()).unwrap();
        assert_animation(&img, SourceFormat::WebP);
    }

    #[test]
    fn larger_animations_decode() {
        for name in ["animated-112.gif", "animated-112.webp"] {
            let img = decode(&fixture(name), &DecodeLimits::default()).unwrap();
            assert_eq!((img.width(), img.height()), (112, 112), "{name}");
            assert_eq!(img.frames().len(), 24, "{name}");
        }
    }

    #[test]
    fn corrupt_and_unknown_inputs_fail_cleanly() {
        let limits = DecodeLimits::default();
        assert!(matches!(
            decode(&fixture("corrupt.png"), &limits),
            Err(DecodeError::Malformed(_))
        ));
        assert_eq!(decode(b"", &limits), Err(DecodeError::Empty));
        assert_eq!(
            decode(b"definitely not an image", &limits),
            Err(DecodeError::UnknownFormat)
        );
        // AVIF magic (`ftypavif`) is recognised and refused.
        let avif = b"\0\0\0\x1cftypavif\0\0\0\0avifmif1miaf";
        assert!(matches!(
            decode(avif, &limits),
            Err(DecodeError::Unsupported(_))
        ));
    }

    #[test]
    fn oversized_canvas_is_rejected_before_decoding() {
        let err = decode(&fixture("oversized.png"), &DecodeLimits::default()).unwrap_err();
        assert_eq!(
            err,
            DecodeError::TooLarge {
                width: 4096,
                height: 16
            }
        );
        // The same file is fine with a looser limit.
        let loose = DecodeLimits {
            max_width: 8192,
            ..DecodeLimits::default()
        };
        assert_eq!(
            decode(&fixture("oversized.png"), &loose).unwrap().width(),
            4096
        );
    }

    #[test]
    fn animations_are_truncated_by_frame_and_byte_caps() {
        let by_frames = DecodeLimits {
            max_frames: 2,
            ..DecodeLimits::default()
        };
        let img = decode(&fixture("animated.gif"), &by_frames).unwrap();
        assert_eq!(img.frames().len(), 2);
        assert!(img.is_truncated());

        let by_bytes = DecodeLimits {
            max_bytes: 28 * 28 * 4 * 3,
            ..DecodeLimits::default()
        };
        let img = decode(&fixture("animated.webp"), &by_bytes).unwrap();
        assert_eq!(img.frames().len(), 3);
        assert!(img.is_truncated());

        let too_small = DecodeLimits {
            max_bytes: 100,
            ..DecodeLimits::default()
        };
        assert!(matches!(
            decode(&fixture("animated.webp"), &too_small),
            Err(DecodeError::TooLarge { .. })
        ));
    }

    #[test]
    fn delay_clamping_matches_browsers() {
        let d = |n| clamp_delay(image::Delay::from_numer_denom_ms(n, 1));
        assert_eq!(d(0), ms(100));
        assert_eq!(d(10), ms(100));
        assert_eq!(d(19), ms(100));
        assert_eq!(d(20), ms(20));
        assert_eq!(d(70), ms(70));
    }

    #[test]
    fn timeline_follows_a_global_clock() {
        let t = Timeline::new([ms(100), ms(50), ms(100), ms(30)]);
        assert_eq!(t.total(), ms(280));
        assert_eq!(t.frame_at(ms(0)), (0, Some(ms(100))));
        assert_eq!(t.frame_at(ms(99)), (0, Some(ms(1))));
        assert_eq!(t.frame_at(ms(100)), (1, Some(ms(50))));
        assert_eq!(t.frame_at(ms(260)), (3, Some(ms(20))));
        // Loops.
        assert_eq!(t.frame_at(ms(280 * 7 + 120)), (1, Some(ms(30))));
        assert_eq!(Timeline::new([ms(100)]).frame_at(ms(50)), (0, None));
    }

    #[test]
    fn byte_size_counts_pixels() {
        let img = decode(&png_bytes(10, 10), &DecodeLimits::default()).unwrap();
        assert!(img.byte_size() >= 400);
        assert!(img.byte_size() < 400 + 512);
    }
}
