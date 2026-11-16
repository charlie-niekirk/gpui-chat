//! 7TV name paints: a gradient (linear or radial), a flat colour or an image, and drop shadows,
//! applied to a user's name.
//!
//! The model is pure data plus the maths to sample it ([`Paint::color_at`]), so the painter in the
//! app only asks "which colour is the glyph at x?" and never allocates. Colours are the 8-bit
//! RGBA 7TV sends (as a signed 32-bit integer, `0xRRGGBBAA`).
//!
//! * Linear gradients follow CSS: `angle` is in degrees, 0° points up and 90° right, the gradient
//!   line is as long as the box needs for the corners to hit 0 % and 100 %.
//! * Radial gradients are circles around the box's centre reaching the farthest corner.
//! * `repeat` repeats the stops, which then span `0..last stop`, like CSS `repeating-*-gradient`.
//! * Image paints cannot be drawn glyph by glyph; they carry their base colour (when 7TV gives
//!   one) as [`PaintFill::Image::fallback`] and the app draws the plain colour otherwise.

use std::sync::Arc;

use serde::Deserialize;
use serde_json::Value;

use crate::lenient::{null_default, parse_entries};

/// An 8-bit non-premultiplied RGBA colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PaintColor {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl PaintColor {
    pub const WHITE: Self = Self::new(255, 255, 255, 255);

    pub const fn new(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }

    /// 7TV's signed integer (`0xRRGGBBAA` reinterpreted as `i32`).
    pub fn from_i32(value: i32) -> Self {
        let [r, g, b, a] = value.to_be_bytes();
        Self { r, g, b, a }
    }

    fn lerp(self, other: Self, t: f32) -> Self {
        let mix = |a: u8, b: u8| {
            let (a, b) = (f32::from(a), f32::from(b));
            (a + (b - a) * t).round().clamp(0.0, 255.0) as u8
        };
        Self {
            r: mix(self.r, other.r),
            g: mix(self.g, other.g),
            b: mix(self.b, other.b),
            a: mix(self.a, other.a),
        }
    }
}

/// A colour stop: `at` is the position along the gradient, `0.0..=1.0` (more with `repeat`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Stop {
    pub at: f32,
    pub color: PaintColor,
}

/// What a paint fills the name with.
#[derive(Debug, Clone, PartialEq)]
pub enum PaintFill {
    Solid(PaintColor),
    Linear {
        /// Degrees, CSS convention (0 = up, 90 = right).
        angle: f32,
        repeat: bool,
        stops: Vec<Stop>,
    },
    Radial {
        repeat: bool,
        stops: Vec<Stop>,
    },
    /// A tiled or stretched picture. Not drawn by this app (see the module docs).
    Image {
        url: String,
        fallback: Option<PaintColor>,
    },
}

/// A drop shadow behind the name. 7TV's `radius` is a blur radius; the app draws offset shadows
/// only (a glyph has no blur).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Shadow {
    pub x: f32,
    pub y: f32,
    pub radius: f32,
    pub color: PaintColor,
}

/// A name paint.
#[derive(Debug, Clone, PartialEq)]
pub struct Paint {
    pub id: Arc<str>,
    pub name: Arc<str>,
    pub fill: PaintFill,
    pub shadows: Vec<Shadow>,
    /// WCAG relative luminance of every stop and of the middle of every pair of neighbouring
    /// stops: the colours a name can show, for [`Paint::min_contrast`].
    luminances: Vec<f32>,
}

/// WCAG 2 relative luminance of an sRGB colour.
pub fn luminance(color: PaintColor) -> f32 {
    fn channel(c: u8) -> f32 {
        let c = f32::from(c) / 255.0;
        if c <= 0.039_28 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    }
    0.2126 * channel(color.r) + 0.7152 * channel(color.g) + 0.0722 * channel(color.b)
}

/// WCAG 2 contrast ratio between two luminances, `1.0..=21.0`.
pub fn contrast(a: f32, b: f32) -> f32 {
    let (hi, lo) = if a > b { (a, b) } else { (b, a) };
    (hi + 0.05) / (lo + 0.05)
}

impl Paint {
    /// A paint, with what is needed to judge its readability worked out once.
    pub fn new(id: Arc<str>, name: Arc<str>, fill: PaintFill, shadows: Vec<Shadow>) -> Self {
        let mut paint = Self {
            id,
            name,
            fill,
            shadows,
            luminances: Vec::new(),
        };
        let colors: Vec<PaintColor> = match &paint.fill {
            PaintFill::Linear { stops, .. } | PaintFill::Radial { stops, .. } => {
                stops.iter().map(|stop| stop.color).collect()
            }
            _ => paint.colors().collect(),
        };
        let mut luminances: Vec<f32> = colors.iter().map(|&c| luminance(c)).collect();
        luminances.extend(
            colors
                .windows(2)
                .map(|pair| luminance(pair[0].lerp(pair[1], 0.5))),
        );
        paint.luminances = luminances;
        paint
    }

    /// The lowest contrast ratio any colour of the paint has against a background of
    /// luminance `background` (a gradient is judged at its stops and halfway between them).
    /// `f32::INFINITY` for a paint with no colour.
    pub fn min_contrast(&self, background: f32) -> f32 {
        self.luminances
            .iter()
            .map(|&l| contrast(l, background))
            .fold(f32::INFINITY, f32::min)
    }

    /// The colour at `(x, y)` of a `w` × `h` box (the name's extent), `None` for an image fill
    /// with no base colour. Allocation-free.
    pub fn color_at(&self, x: f32, y: f32, w: f32, h: f32) -> Option<PaintColor> {
        match &self.fill {
            PaintFill::Solid(color) => Some(*color),
            PaintFill::Image { fallback, .. } => *fallback,
            PaintFill::Linear {
                angle,
                repeat,
                stops,
            } => {
                let (sin, cos) = angle.to_radians().sin_cos();
                let length = (w * sin).abs() + (h * cos).abs();
                let t = if length <= f32::EPSILON {
                    0.0
                } else {
                    0.5 + ((x - w / 2.0) * sin - (y - h / 2.0) * cos) / length
                };
                sample(stops, t, *repeat)
            }
            PaintFill::Radial { repeat, stops } => {
                let (dx, dy) = (x - w / 2.0, y - h / 2.0);
                let reach = (w * w + h * h).sqrt() / 2.0;
                let t = if reach <= f32::EPSILON {
                    0.0
                } else {
                    (dx * dx + dy * dy).sqrt() / reach
                };
                sample(stops, t, *repeat)
            }
        }
    }

    /// Every colour the paint can show (the stops), for readability checks. Empty for an image
    /// with no base colour.
    pub fn colors(&self) -> impl Iterator<Item = PaintColor> + '_ {
        let (stops, single): (&[Stop], Option<PaintColor>) = match &self.fill {
            PaintFill::Solid(color) => (&[], Some(*color)),
            PaintFill::Image { fallback, .. } => (&[], *fallback),
            PaintFill::Linear { stops, .. } | PaintFill::Radial { stops, .. } => (stops, None),
        };
        stops.iter().map(|stop| stop.color).chain(single)
    }

    /// `true` when the fill can be drawn on glyphs (everything but a bare image).
    pub fn is_drawable(&self) -> bool {
        self.colors().next().is_some()
    }
}

/// The gradient colour at position `t` (before any repetition).
fn sample(stops: &[Stop], t: f32, repeat: bool) -> Option<PaintColor> {
    let first = stops.first()?;
    let last = stops.last()?;
    let mut t = t;
    if repeat {
        let span = last.at - first.at;
        if span > f32::EPSILON {
            t = first.at + (t - first.at).rem_euclid(span);
        }
    }
    if t <= first.at {
        return Some(first.color);
    }
    if t >= last.at {
        return Some(last.color);
    }
    // The first stop strictly after `t`; stops are in order.
    let next = stops.partition_point(|stop| stop.at <= t);
    let (a, b) = (stops[next - 1], stops[next]);
    let span = b.at - a.at;
    Some(if span <= f32::EPSILON {
        b.color
    } else {
        a.color.lerp(b.color, (t - a.at) / span)
    })
}

#[derive(Deserialize)]
struct RawStop {
    at: f32,
    color: i32,
}

#[derive(Deserialize)]
struct RawShadow {
    #[serde(default, deserialize_with = "null_default")]
    x_offset: f32,
    #[serde(default, deserialize_with = "null_default")]
    y_offset: f32,
    #[serde(default, deserialize_with = "null_default")]
    radius: f32,
    color: i32,
}

#[derive(Deserialize)]
struct RawPaint {
    id: String,
    #[serde(default, deserialize_with = "null_default")]
    name: String,
    /// `LINEAR_GRADIENT`, `RADIAL_GRADIENT` or `URL`.
    #[serde(default, deserialize_with = "null_default")]
    function: String,
    color: Option<i32>,
    #[serde(default, deserialize_with = "null_default")]
    angle: f32,
    #[serde(default, deserialize_with = "null_default")]
    repeat: bool,
    #[serde(default, deserialize_with = "null_default")]
    image_url: String,
    #[serde(default, deserialize_with = "null_default")]
    stops: Vec<Value>,
    #[serde(default, deserialize_with = "null_default")]
    shadows: Vec<Value>,
}

/// Parses a paint as the GraphQL API and the EventAPI's `cosmetic.create` describe it. `None`
/// when it is not a paint this module can represent.
pub fn parse_paint(value: &Value) -> Option<Paint> {
    let raw: RawPaint = match serde_json::from_value(value.clone()) {
        Ok(raw) => raw,
        Err(error) => {
            tracing::debug!(%error, "skipping malformed 7TV paint");
            return None;
        }
    };
    let mut stops: Vec<Stop> = parse_entries::<RawStop>("7TV paint stop", raw.stops)
        .into_iter()
        .map(|stop| Stop {
            at: stop.at,
            color: PaintColor::from_i32(stop.color),
        })
        .collect();
    stops.sort_by(|a, b| a.at.total_cmp(&b.at));
    let base = raw.color.map(PaintColor::from_i32);
    let fill = match raw.function.as_str() {
        "URL" if !raw.image_url.is_empty() => PaintFill::Image {
            url: raw.image_url,
            fallback: base,
        },
        "RADIAL_GRADIENT" if !stops.is_empty() => PaintFill::Radial {
            repeat: raw.repeat,
            stops,
        },
        _ if !stops.is_empty() => PaintFill::Linear {
            angle: raw.angle,
            repeat: raw.repeat,
            stops,
        },
        _ => PaintFill::Solid(base?),
    };
    let shadows = parse_entries::<RawShadow>("7TV paint shadow", raw.shadows)
        .into_iter()
        .map(|shadow| Shadow {
            x: shadow.x_offset,
            y: shadow.y_offset,
            radius: shadow.radius,
            color: PaintColor::from_i32(shadow.color),
        })
        .collect();
    Some(Paint::new(raw.id.into(), raw.name.into(), fill, shadows))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn paint(value: Value) -> Paint {
        parse_paint(&value).expect("a paint")
    }

    #[test]
    fn colours_are_rgba_in_a_signed_integer() {
        assert_eq!(
            PaintColor::from_i32(-16_723_713),
            PaintColor::new(255, 0, 208, 255)
        );
        assert_eq!(PaintColor::from_i32(-1), PaintColor::WHITE);
        assert_eq!(
            PaintColor::from_i32(-1_946_091_521),
            PaintColor::new(140, 0, 255, 255)
        );
        assert_eq!(
            PaintColor::from_i32(0x1234_5678),
            PaintColor::new(0x12, 0x34, 0x56, 0x78)
        );
    }

    #[test]
    fn parses_a_linear_gradient_with_shadows_like_7tv_sends_it() {
        let p = paint(json!({
            "id": "P1", "name": "Pretty in Pink", "function": "LINEAR_GRADIENT", "color": null,
            "angle": 0, "shape": "circle", "repeat": false, "image_url": "",
            "stops": [{"at": 0, "color": -16723713}, {"at": 0.38, "color": -1}, {"at": 0.71, "color": -16723713}],
            "shadows": [{"x_offset": 0, "y_offset": 0, "radius": 1, "color": -1946091521}]
        }));
        assert_eq!(&*p.id, "P1");
        let PaintFill::Linear {
            angle,
            repeat,
            stops,
        } = &p.fill
        else {
            panic!("{:?}", p.fill)
        };
        assert_eq!((*angle, *repeat, stops.len()), (0.0, false, 3));
        assert_eq!(p.shadows.len(), 1);
        assert_eq!(p.shadows[0].radius, 1.0);
        assert!(p.is_drawable());
    }

    #[test]
    fn function_picks_the_fill_and_a_bare_colour_is_solid() {
        let radial = paint(
            json!({"id":"R","function":"RADIAL_GRADIENT","stops":[{"at":0,"color":-1},{"at":1,"color":0}]}),
        );
        assert!(matches!(radial.fill, PaintFill::Radial { .. }));
        let solid =
            paint(json!({"id":"S","function":"LINEAR_GRADIENT","color":-16723713,"stops":[]}));
        assert_eq!(
            solid.fill,
            PaintFill::Solid(PaintColor::new(255, 0, 208, 255))
        );
        let image = paint(
            json!({"id":"I","function":"URL","image_url":"https://x/y.webp","color":null,"stops":[]}),
        );
        assert!(matches!(
            image.fill,
            PaintFill::Image { fallback: None, .. }
        ));
        assert!(!image.is_drawable());
        assert!(parse_paint(&json!({"id":"E","function":"LINEAR_GRADIENT","stops":[]})).is_none());
        assert!(parse_paint(&json!({"name":"no id"})).is_none());
    }

    #[test]
    fn a_horizontal_linear_gradient_runs_left_to_right() {
        let p = paint(json!({"id":"L","function":"LINEAR_GRADIENT","angle":90,
            "stops":[{"at":0,"color":255},{"at":1,"color":-1}]}));
        let at = |x: f32| p.color_at(x, 5.0, 100.0, 10.0).unwrap();
        assert_eq!(at(0.0).r, 0);
        assert_eq!(at(100.0).r, 255);
        assert!((i16::from(at(50.0).r) - 128).abs() <= 1);
        // 180° runs top to bottom.
        let down = paint(json!({"id":"D","function":"LINEAR_GRADIENT","angle":180,
            "stops":[{"at":0,"color":255},{"at":1,"color":-1}]}));
        assert_eq!(down.color_at(10.0, 0.0, 100.0, 10.0).unwrap().r, 0);
        assert_eq!(down.color_at(10.0, 10.0, 100.0, 10.0).unwrap().r, 255);
    }

    #[test]
    fn repeating_gradients_wrap_at_the_last_stop() {
        let p = paint(
            json!({"id":"L","function":"LINEAR_GRADIENT","angle":90,"repeat":true,
            "stops":[{"at":0,"color":255},{"at":0.25,"color":-1}]}),
        );
        let at = |x: f32| p.color_at(x, 5.0, 100.0, 10.0).unwrap().r;
        // 0 %, 25 % (a full period) and 50 % all start a period black-to-white.
        assert!(at(0.0) < 5);
        assert!(at(24.0) > 240);
        assert!(at(26.0) < 20, "wrapped around: {}", at(26.0));
    }

    #[test]
    fn radial_gradients_grow_from_the_centre() {
        let p = paint(json!({"id":"R","function":"RADIAL_GRADIENT",
            "stops":[{"at":0,"color":255},{"at":1,"color":-1}]}));
        let centre = p.color_at(50.0, 5.0, 100.0, 10.0).unwrap().r;
        let edge = p.color_at(0.0, 0.0, 100.0, 10.0).unwrap().r;
        assert!(centre < 5 && edge > 250, "{centre} {edge}");
    }

    #[test]
    fn stops_are_sorted_and_sampling_clamps_outside_them() {
        let p = paint(json!({"id":"L","function":"LINEAR_GRADIENT","angle":90,
            "stops":[{"at":0.8,"color":-1},{"at":0.2,"color":255}]}));
        assert_eq!(p.color_at(0.0, 0.0, 10.0, 10.0).unwrap().r, 0);
        assert_eq!(p.color_at(10.0, 0.0, 10.0, 10.0).unwrap().r, 255);
        // A hard edge (two stops at one position) does not divide by zero.
        let hard = paint(json!({"id":"H","function":"LINEAR_GRADIENT","angle":90,
            "stops":[{"at":0,"color":255},{"at":0.5,"color":255},{"at":0.5,"color":-1},{"at":1,"color":-1}]}));
        assert_eq!(hard.color_at(2.0, 0.0, 10.0, 10.0).unwrap().r, 0);
        assert_eq!(hard.color_at(8.0, 0.0, 10.0, 10.0).unwrap().r, 255);
    }

    #[test]
    fn readability_is_judged_at_the_stops_and_between_them() {
        let paint = |stops: &str| {
            paint(
                json!({"id":"P","function":"LINEAR_GRADIENT","angle":90,"stops": serde_json::from_str::<Value>(stops).unwrap()}),
            )
        };
        let black = luminance(PaintColor::new(0, 0, 0, 255));
        let white = luminance(PaintColor::WHITE);
        // Bright stops on a black background: fine; on white: the worst stop decides.
        let bright = paint(r#"[{"at":0,"color":-1},{"at":1,"color":-65281}]"#);
        assert!(bright.min_contrast(black) > 7.0);
        assert!(bright.min_contrast(white) < 3.0);
        // Black to white is unreadable on mid-grey even though both ends are not: the middle is.
        let ends = paint(r#"[{"at":0,"color":255},{"at":1,"color":-1}]"#);
        let grey = luminance(PaintColor::new(120, 120, 120, 255));
        assert!(ends.min_contrast(grey) < 1.5, "{}", ends.min_contrast(grey));
        // A paint with no colour is never a problem.
        let image = Paint::new(
            "I".into(),
            "I".into(),
            PaintFill::Image {
                url: "u".into(),
                fallback: None,
            },
            Vec::new(),
        );
        assert_eq!(image.min_contrast(black), f32::INFINITY);
    }

    #[test]
    fn degenerate_boxes_do_not_produce_nan() {
        let p = paint(json!({"id":"L","function":"LINEAR_GRADIENT","angle":90,
            "stops":[{"at":0,"color":255},{"at":1,"color":-1}]}));
        assert!(p.color_at(0.0, 0.0, 0.0, 0.0).is_some());
    }
}
