//! Decode cost per format and size.
//!
//! ```bash
//! cargo bench -p gpui-chat-media --bench decode
//! ```

use std::hint::black_box;
use std::io::Cursor;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use gpui_chat_media::{DecodeLimits, decode};

macro_rules! fixture {
    ($name:literal) => {
        (
            $name,
            &include_bytes!(concat!("../../../fixtures/images/", $name))[..],
        )
    };
}

/// An opaque 112x112 PNG (a 4x still emote), generated so it is not a fixture file.
fn png_112() -> Vec<u8> {
    let img = image::RgbaImage::from_fn(112, 112, |x, y| {
        image::Rgba([(x * 2) as u8, (y * 2) as u8, ((x ^ y) * 2) as u8, 255])
    });
    let mut out = Vec::new();
    img.write_to(&mut Cursor::new(&mut out), image::ImageFormat::Png)
        .expect("encode");
    out
}

fn bench_decode(c: &mut Criterion) {
    let limits = DecodeLimits::default();
    let generated = png_112();
    let inputs: Vec<(&str, &[u8])> = vec![
        fixture!("static.png"),
        fixture!("static.webp"),
        ("generated-112.png", &generated),
        fixture!("animated.gif"),
        fixture!("animated.webp"),
        fixture!("animated-112.gif"),
        fixture!("animated-112.webp"),
    ];
    let mut group = c.benchmark_group("decode");
    for (name, bytes) in inputs {
        let img = decode(bytes, &limits).expect("fixture decodes");
        // Throughput in decoded frames, so "elem/s" reads as frames per second.
        group.throughput(Throughput::Elements(img.frames().len() as u64));
        group.bench_function(name, |b| b.iter(|| decode(black_box(bytes), &limits)));
    }
    group.finish();
}

criterion_group!(benches, bench_decode);
criterion_main!(benches);
