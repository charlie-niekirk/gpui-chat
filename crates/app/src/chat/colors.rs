//! Username colours: the deterministic fallback for users without a colour, and the readability
//! adjustment that keeps any colour legible on the current theme background.
//!
//! The adjustment keeps hue and saturation and moves only HSL lightness, away from the
//! background (lighter on a dark theme, darker on a light one), by the smallest amount that
//! reaches [`MIN_CONTRAST`] (WCAG 2 contrast ratio). Colours that already pass are untouched.
//! Pure `f32` maths, so it is unit-tested without a window.

use gpui_chat_core::Rgb;
use gpui_kit::{Hsla, Rgba};

/// Contrast ratio a username must reach against the chat background (WCAG AA, normal text).
pub const MIN_CONTRAST: f32 = 4.5;

/// Twitch's default name colours, used when a user never picked one. Which one a user gets is a
/// hash of their login, so it is stable across messages, sessions and machines.
pub const FALLBACK_COLORS: [Rgb; 15] = [
    Rgb::new(0xff, 0x00, 0x00), // Red
    Rgb::new(0x00, 0x00, 0xff), // Blue
    Rgb::new(0x00, 0x80, 0x00), // Green
    Rgb::new(0xb2, 0x22, 0x22), // FireBrick
    Rgb::new(0xff, 0x7f, 0x50), // Coral
    Rgb::new(0x9a, 0xcd, 0x32), // YellowGreen
    Rgb::new(0xff, 0x45, 0x00), // OrangeRed
    Rgb::new(0x2e, 0x8b, 0x57), // SeaGreen
    Rgb::new(0xda, 0xa5, 0x20), // GoldenRod
    Rgb::new(0xd2, 0x69, 0x1e), // Chocolate
    Rgb::new(0x5f, 0x9e, 0xa0), // CadetBlue
    Rgb::new(0x1e, 0x90, 0xff), // DodgerBlue
    Rgb::new(0xff, 0x69, 0xb4), // HotPink
    Rgb::new(0x8a, 0x2b, 0xe2), // BlueViolet
    Rgb::new(0x00, 0xff, 0x7f), // SpringGreen
];

/// The colour for a user without one: FNV-1a of the lowercase login picks a default colour.
pub fn fallback_color(login: &str) -> Rgb {
    let mut hash: u32 = 0x811c_9dc5;
    for byte in login.bytes() {
        hash ^= u32::from(byte.to_ascii_lowercase());
        hash = hash.wrapping_mul(0x0100_0193);
    }
    FALLBACK_COLORS[hash as usize % FALLBACK_COLORS.len()]
}

/// Linear-light RGB in `0.0..=1.0`.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Srgb {
    r: f32,
    g: f32,
    b: f32,
}

impl Srgb {
    fn from_rgb(rgb: Rgb) -> Self {
        Self {
            r: f32::from(rgb.r) / 255.0,
            g: f32::from(rgb.g) / 255.0,
            b: f32::from(rgb.b) / 255.0,
        }
    }

    fn from_hsla(color: Hsla) -> Self {
        let Rgba { r, g, b, .. } = color.to_rgb();
        Self { r, g, b }
    }

    /// WCAG 2 relative luminance.
    fn luminance(self) -> f32 {
        fn channel(c: f32) -> f32 {
            if c <= 0.039_28 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        }
        0.2126 * channel(self.r) + 0.7152 * channel(self.g) + 0.0722 * channel(self.b)
    }
}

/// WCAG 2 contrast ratio between two luminances, `1.0..=21.0`.
fn contrast_of(a: f32, b: f32) -> f32 {
    let (hi, lo) = if a > b { (a, b) } else { (b, a) };
    (hi + 0.05) / (lo + 0.05)
}

/// `(h, s, l)`, each in `0.0..=1.0`.
fn to_hsl(c: Srgb) -> (f32, f32, f32) {
    let max = c.r.max(c.g).max(c.b);
    let min = c.r.min(c.g).min(c.b);
    let l = (max + min) / 2.0;
    let d = max - min;
    if d <= f32::EPSILON {
        return (0.0, 0.0, l);
    }
    let s = d / (1.0 - (2.0 * l - 1.0).abs());
    let h = if max == c.r {
        ((c.g - c.b) / d).rem_euclid(6.0)
    } else if max == c.g {
        (c.b - c.r) / d + 2.0
    } else {
        (c.r - c.g) / d + 4.0
    };
    (h / 6.0, s.clamp(0.0, 1.0), l)
}

fn from_hsl(h: f32, s: f32, l: f32) -> Srgb {
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let hp = h.rem_euclid(1.0) * 6.0;
    let x = c * (1.0 - (hp.rem_euclid(2.0) - 1.0).abs());
    let (r, g, b) = match hp as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = l - c / 2.0;
    Srgb {
        r: (r + m).clamp(0.0, 1.0),
        g: (g + m).clamp(0.0, 1.0),
        b: (b + m).clamp(0.0, 1.0),
    }
}

/// `color` with its lightness moved just far enough from `background` to reach
/// [`MIN_CONTRAST`] (or as far as possible, if even white/black cannot). Hue and saturation are
/// kept; a colour that already passes is returned unchanged.
pub fn readable(color: Rgb, background: Hsla) -> Hsla {
    let bg = Srgb::from_hsla(background).luminance();
    let original = Srgb::from_rgb(color);
    if contrast_of(original.luminance(), bg) >= MIN_CONTRAST {
        return to_hsla(original);
    }
    let (h, s, l) = to_hsl(original);
    // Move towards whichever end (white or black) contrasts more with the background.
    let lighten = contrast_of(1.0, bg) >= contrast_of(0.0, bg);
    let passes = |l: f32| contrast_of(from_hsl(h, s, l).luminance(), bg) >= MIN_CONTRAST;
    let (mut near, mut far) = if lighten { (l, 1.0) } else { (l, 0.0) };
    if !passes(far) {
        return to_hsla(from_hsl(h, s, far));
    }
    // Contrast is monotonic in lightness on each side of the background, so bisect for the
    // smallest change that passes. 20 halvings is far below one 8-bit step.
    for _ in 0..20 {
        let mid = (near + far) / 2.0;
        if passes(mid) {
            far = mid;
        } else {
            near = mid;
        }
    }
    to_hsla(from_hsl(h, s, far))
}

fn to_hsla(c: Srgb) -> Hsla {
    Hsla::from(Rgba {
        r: c.r,
        g: c.g,
        b: c.b,
        a: 1.0,
    })
}

/// [`readable`] for an [`Hsla`] colour (theme tokens such as the link colour).
pub fn readable_hsla(color: Hsla, background: Hsla) -> Hsla {
    let Rgba { r, g, b, .. } = color.to_rgb();
    let to_u8 = |c: f32| (c.clamp(0.0, 1.0) * 255.0).round() as u8;
    readable(Rgb::new(to_u8(r), to_u8(g), to_u8(b)), background)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::Tokens;

    fn contrast(rgb: Rgb, background: Hsla) -> f32 {
        contrast_of(
            Srgb::from_rgb(rgb).luminance(),
            Srgb::from_hsla(background).luminance(),
        )
    }

    fn hsla_contrast(color: Hsla, background: Hsla) -> f32 {
        contrast_of(
            Srgb::from_hsla(color).luminance(),
            Srgb::from_hsla(background).luminance(),
        )
    }

    fn backgrounds() -> [(&'static str, Hsla); 2] {
        [
            ("dark", Tokens::dark().main_bg),
            ("light", Tokens::light().main_bg),
        ]
    }

    #[test]
    fn contrast_matches_wcag_reference_values() {
        let white = Rgb::new(255, 255, 255);
        let black: Hsla = Hsla::from(Rgba {
            r: 0.0,
            g: 0.0,
            b: 0.0,
            a: 1.0,
        });
        assert!((contrast(white, black) - 21.0).abs() < 0.01);
        // #777777 on white is the classic ~4.48:1.
        let white_bg = to_hsla(Srgb::from_rgb(white));
        assert!((contrast(Rgb::new(0x77, 0x77, 0x77), white_bg) - 4.48).abs() < 0.02);
    }

    #[test]
    fn every_fallback_colour_is_readable_on_both_themes() {
        for (theme, bg) in backgrounds() {
            for color in FALLBACK_COLORS {
                let adjusted = readable(color, bg);
                let ratio = hsla_contrast(adjusted, bg);
                assert!(
                    ratio >= MIN_CONTRAST - 0.01,
                    "{} on {theme}: {ratio:.2}",
                    color.to_hex()
                );
            }
        }
    }

    #[test]
    fn extreme_user_colours_are_corrected_in_the_right_direction() {
        let (dark, light) = (Tokens::dark().main_bg, Tokens::light().main_bg);
        // Pure blue and black are unreadable on dark: they get lighter.
        for color in [
            Rgb::new(0, 0, 255),
            Rgb::new(0, 0, 0),
            Rgb::new(0x20, 0x10, 0x30),
        ] {
            let adjusted = readable(color, dark);
            assert!(adjusted.l > to_hsla(Srgb::from_rgb(color)).l);
            assert!(hsla_contrast(adjusted, dark) >= MIN_CONTRAST - 0.01);
        }
        // Yellow and white are unreadable on light: they get darker.
        for color in [
            Rgb::new(255, 255, 0),
            Rgb::new(255, 255, 255),
            Rgb::new(0, 255, 127),
        ] {
            let adjusted = readable(color, light);
            assert!(adjusted.l < to_hsla(Srgb::from_rgb(color)).l);
            assert!(hsla_contrast(adjusted, light) >= MIN_CONTRAST - 0.01);
        }
    }

    #[test]
    fn readable_colours_are_left_alone_and_hue_is_kept() {
        let dark = Tokens::dark().main_bg;
        let fine = Rgb::new(0xff, 0x69, 0xb4); // HotPink on near-black passes as is.
        assert_eq!(readable(fine, dark), to_hsla(Srgb::from_rgb(fine)));

        let blue = Rgb::new(0, 0, 255);
        let adjusted = readable(blue, dark);
        let original = to_hsla(Srgb::from_rgb(blue));
        assert!((adjusted.h - original.h).abs() < 0.01, "hue drifted");
        assert!((adjusted.s - original.s).abs() < 0.02, "saturation drifted");
        // The smallest change that passes: not pushed all the way to white.
        assert!(adjusted.l < 0.95);
    }

    #[test]
    fn hsl_round_trips() {
        for color in FALLBACK_COLORS {
            let c = Srgb::from_rgb(color);
            let (h, s, l) = to_hsl(c);
            let back = from_hsl(h, s, l);
            assert!((back.r - c.r).abs() < 1e-4 && (back.g - c.g).abs() < 1e-4);
            assert!((back.b - c.b).abs() < 1e-4, "{}", color.to_hex());
        }
    }

    #[test]
    fn fallback_is_deterministic_case_insensitive_and_spread() {
        assert_eq!(fallback_color("forsen"), fallback_color("forsen"));
        assert_eq!(fallback_color("Forsen"), fallback_color("forsen"));
        let distinct: std::collections::HashSet<_> = (0..200)
            .map(|n| fallback_color(&format!("user{n}")).to_hex())
            .collect();
        assert!(distinct.len() >= 12, "only {} colours used", distinct.len());
        // Pinned, so a change of hash (which would recolour every user) is deliberate.
        assert_eq!(fallback_color("justinfan12345").to_hex(), "#ff7f50");
    }
}
