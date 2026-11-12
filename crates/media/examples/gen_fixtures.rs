//! Regenerates the image fixtures in `fixtures/images/`.
//!
//! ```bash
//! cargo run -p gpui-chat-media --example gen_fixtures
//! ```
//!
//! Every image is synthesised here (no third-party artwork). PNG, GIF and still WebP are
//! encoded with the `image` crate; animated WebP needs `img2webp` from libwebp on `PATH`
//! (`brew install webp`). Tests in `crates/media` assert on the exact colours and delays
//! chosen below, so keep them in sync.

use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::process::Command;

use image::codecs::gif::{GifEncoder, Repeat};
use image::codecs::webp::WebPEncoder;
use image::{Delay, Frame, ImageFormat, Rgba, RgbaImage};

/// Frame `i` of the small animation: left half this colour, right half transparent.
const FRAME_COLOURS: [[u8; 3]; 4] = [[255, 0, 0], [0, 255, 0], [0, 0, 255], [255, 255, 0]];
/// Concentric rings of the large animation; the pattern moves outwards one ring per frame.
const RING_COLOURS: [Rgba<u8>; 6] = [
    Rgba([0, 0, 0, 0]),
    Rgba([145, 70, 255, 255]),
    Rgba([255, 255, 255, 255]),
    Rgba([30, 30, 40, 255]),
    Rgba([0, 200, 120, 255]),
    Rgba([255, 120, 0, 255]),
];
/// GIF delays in ms (GIF stores centiseconds). The 0 must be clamped to 100 by the decoder.
const GIF_DELAYS: [u32; 4] = [100, 50, 0, 30];
/// WebP delays in ms. The 10 must be clamped to 100 by the decoder.
const WEBP_DELAYS: [u32; 4] = [100, 50, 10, 30];

fn main() {
    let out = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/images");
    std::fs::create_dir_all(&out).expect("create fixtures/images");
    let tmp = tempfile::tempdir().expect("temp dir");

    // static.png / static.webp: 28x28 gradient, (0,0) = RGBA(200, 100, 50, 128).
    let still = RgbaImage::from_fn(28, 28, |x, y| {
        if (x, y) == (0, 0) {
            Rgba([200, 100, 50, 128])
        } else {
            Rgba([(x * 9) as u8, (y * 9) as u8, 128, 255])
        }
    });
    save(&out, "static.png", &png(&still));
    let mut webp = Vec::new();
    WebPEncoder::new_lossless(&mut webp)
        .encode(still.as_raw(), 28, 28, image::ExtendedColorType::Rgba8)
        .expect("encode webp");
    save(&out, "static.webp", &webp);

    // corrupt.png: the first half of static.png (header intact, image data cut short).
    let full = png(&still);
    save(&out, "corrupt.png", &full[..full.len() / 2]);

    // oversized.png: 4096x16, over the default 2048 px limit; compresses to a few hundred bytes.
    save(
        &out,
        "oversized.png",
        &png(&RgbaImage::from_pixel(4096, 16, Rgba([1, 2, 3, 255]))),
    );

    // animated.gif / animated.webp: 28x28, 4 frames.
    let small: Vec<RgbaImage> = FRAME_COLOURS
        .iter()
        .map(|&[r, g, b]| {
            RgbaImage::from_fn(28, 28, |x, _| {
                if x < 14 {
                    Rgba([r, g, b, 255])
                } else {
                    Rgba([0, 0, 0, 0])
                }
            })
        })
        .collect();
    save(&out, "animated.gif", &gif(&small, &GIF_DELAYS));
    animated_webp(&out, tmp.path(), "animated.webp", &small, &WEBP_DELAYS);

    // animated-112.gif / animated-112.webp: 112x112 (a 4x emote), 24 frames at 40 ms. The WebP
    // is lossless: these flat colours compress far better that way.
    let large: Vec<RgbaImage> = (0..24u32)
        .map(|i| {
            RgbaImage::from_fn(112, 112, |x, y| {
                let ring =
                    (((x as i32 - 56).pow(2) + (y as i32 - 56).pow(2)) as f32).sqrt() as u32 / 6;
                RING_COLOURS[((ring + i) % 6) as usize]
            })
        })
        .collect();
    save(&out, "animated-112.gif", &gif(&large, &[40; 24]));
    animated_webp(&out, tmp.path(), "animated-112.webp", &large, &[40; 24]);
}

fn save(dir: &Path, name: &str, bytes: &[u8]) {
    std::fs::write(dir.join(name), bytes).expect("write fixture");
    println!("{name}: {} bytes", bytes.len());
}

fn png(img: &RgbaImage) -> Vec<u8> {
    let mut out = Vec::new();
    img.write_to(&mut Cursor::new(&mut out), ImageFormat::Png)
        .expect("encode png");
    out
}

fn gif(frames: &[RgbaImage], delays_ms: &[u32]) -> Vec<u8> {
    let mut out = Vec::new();
    {
        let mut enc = GifEncoder::new_with_speed(&mut out, 10);
        enc.set_repeat(Repeat::Infinite).expect("repeat");
        for (img, &ms) in frames.iter().zip(delays_ms) {
            let frame = Frame::from_parts(img.clone(), 0, 0, Delay::from_numer_denom_ms(ms, 1));
            enc.encode_frame(frame).expect("encode gif frame");
        }
    }
    out
}

fn animated_webp(out: &Path, tmp: &Path, name: &str, frames: &[RgbaImage], delays_ms: &[u32]) {
    let mut cmd = Command::new("img2webp");
    cmd.args(["-loop", "0", "-min_size", "-lossless"]);
    for (i, (img, ms)) in frames.iter().zip(delays_ms).enumerate() {
        let path = tmp.join(format!("{name}-{i}.png"));
        img.save(&path).expect("write frame png");
        cmd.arg("-d").arg(ms.to_string()).arg(&path);
    }
    let target = out.join(name);
    cmd.arg("-o").arg(&target);
    let status = cmd
        .status()
        .expect("img2webp not found (brew install webp)");
    assert!(status.success(), "img2webp failed for {name}");
    let len = std::fs::metadata(&target).expect("webp written").len();
    println!("{name}: {len} bytes (img2webp)");
}
