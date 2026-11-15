//! Colour themes: the file format, parsing, validation and derived colours.
//!
//! A theme is a TOML file (bundled in `assets/themes/`, or the user's own in
//! `<config>/themes/*.toml`) with one or two palettes:
//!
//! ```toml
//! name = "My Theme"          # optional; defaults to the file name
//!
//! [dark]                     # and/or [light]
//! background = "#1a1b26"     # required: the chat area
//! text       = "#a9b1d6"     # required
//! accent     = "#7aa2f7"     # required
//! # Optional (derived from the three above when missing):
//! # window, sidebar, title_bar, border, surface, text_muted, accent_text,
//! # danger, success, warning
//! ```
//!
//! Colours are `#rgb` or `#rrggbb`. Unknown keys are ignored, so a theme written for a newer
//! build still loads. The palette is the app's own token set, not a GPUI type: this module has
//! no GPUI and is tested on Linux. The app turns a [`Palette`] into GPUI colours.

use std::path::PathBuf;

use serde::Deserialize;

/// Light or dark.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ThemeVariant {
    Dark,
    Light,
}

impl ThemeVariant {
    fn key(self) -> &'static str {
        match self {
            Self::Dark => "dark",
            Self::Light => "light",
        }
    }
}

/// An sRGB colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Color {
    pub const BLACK: Self = Self::from_u32(0x000000);
    pub const WHITE: Self = Self::from_u32(0xffffff);

    /// `0xRRGGBB`.
    pub const fn from_u32(rgb: u32) -> Self {
        Self {
            r: (rgb >> 16) as u8,
            g: (rgb >> 8) as u8,
            b: rgb as u8,
        }
    }

    /// `0xRRGGBB`.
    pub const fn to_u32(self) -> u32 {
        ((self.r as u32) << 16) | ((self.g as u32) << 8) | self.b as u32
    }

    /// Parses `#rgb` or `#rrggbb` (the `#` is required).
    pub fn parse(text: &str) -> Option<Self> {
        let digits = text.trim().strip_prefix('#')?;
        if !digits.is_ascii() {
            return None;
        }
        let value = |s: &str| u32::from_str_radix(s, 16).ok();
        match digits.len() {
            3 => {
                let v = value(digits)?;
                let (r, g, b) = ((v >> 8) & 0xf, (v >> 4) & 0xf, v & 0xf);
                Some(Self::from_u32(
                    ((r * 17) << 16) | ((g * 17) << 8) | (b * 17),
                ))
            }
            6 => Some(Self::from_u32(value(digits)?)),
            _ => None,
        }
    }

    /// `self` moved `amount` (0 to 1) of the way towards `other`, per sRGB channel.
    pub fn mix(self, other: Self, amount: f32) -> Self {
        let amount = amount.clamp(0.0, 1.0);
        let lerp = |a: u8, b: u8| {
            (f32::from(a) + (f32::from(b) - f32::from(a)) * amount)
                .round()
                .clamp(0.0, 255.0) as u8
        };
        Self {
            r: lerp(self.r, other.r),
            g: lerp(self.g, other.g),
            b: lerp(self.b, other.b),
        }
    }

    /// WCAG relative luminance.
    pub fn luminance(self) -> f32 {
        let linear = |c: u8| {
            let c = f32::from(c) / 255.0;
            if c <= 0.04045 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * linear(self.r) + 0.7152 * linear(self.g) + 0.0722 * linear(self.b)
    }

    /// WCAG contrast ratio, 1 to 21.
    pub fn contrast(self, other: Self) -> f32 {
        let (a, b) = (self.luminance(), other.luminance());
        let (hi, lo) = if a > b { (a, b) } else { (b, a) };
        (hi + 0.05) / (lo + 0.05)
    }
}

impl std::fmt::Display for Color {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "#{:02x}{:02x}{:02x}", self.r, self.g, self.b)
    }
}

/// The colours of one light or dark variant: the app's semantic token set, all resolved
/// (a theme file only has to give `background`, `text` and `accent`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    /// The chat area.
    pub background: Color,
    /// Behind everything; visible at the window edges.
    pub window: Color,
    pub title_bar: Color,
    pub sidebar: Color,
    pub border: Color,
    /// Raised surfaces: menus, popovers, dialogs, inputs, the emote picker.
    pub surface: Color,
    pub text: Color,
    pub text_muted: Color,
    pub accent: Color,
    /// Text and icons drawn on an `accent` fill.
    pub accent_text: Color,
    /// Errors, the live dot.
    pub danger: Color,
    /// Connected / healthy.
    pub success: Color,
    /// Connecting / reconnecting / caution.
    pub warning: Color,
}

/// Fallback status colours for a variant when a theme does not choose its own.
struct StatusDefaults {
    danger: Color,
    success: Color,
    warning: Color,
}

impl StatusDefaults {
    fn of(variant: ThemeVariant) -> Self {
        match variant {
            ThemeVariant::Dark => Self {
                danger: Color::from_u32(0xff6b6b),
                success: Color::from_u32(0x2fbf71),
                warning: Color::from_u32(0xe0a030),
            },
            ThemeVariant::Light => Self {
                danger: Color::from_u32(0xc4221f),
                success: Color::from_u32(0x1f9d55),
                warning: Color::from_u32(0xb7791f),
            },
        }
    }
}

/// `#000000` or `#ffffff`, whichever is easier to read on `fill`.
fn text_on(fill: Color) -> Color {
    if fill.contrast(Color::WHITE) >= fill.contrast(Color::BLACK) {
        Color::WHITE
    } else {
        Color::BLACK
    }
}

/// `text` faded towards `background` as far as keeps a 4.5:1 contrast, at most 40 %.
fn muted_text(text: Color, background: Color) -> Color {
    for step in (0..=8).rev() {
        let candidate = text.mix(background, step as f32 * 0.05);
        if candidate.contrast(background) >= 4.5 {
            return candidate;
        }
    }
    text
}

/// What a theme file may say about one variant. Everything is a string here so a bad colour
/// is reported with its key instead of as a serde type error.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawPalette {
    background: Option<String>,
    window: Option<String>,
    title_bar: Option<String>,
    sidebar: Option<String>,
    border: Option<String>,
    surface: Option<String>,
    text: Option<String>,
    text_muted: Option<String>,
    accent: Option<String>,
    accent_text: Option<String>,
    danger: Option<String>,
    success: Option<String>,
    warning: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawTheme {
    name: Option<String>,
    dark: Option<RawPalette>,
    light: Option<RawPalette>,
}

impl RawPalette {
    fn resolve(&self, variant: ThemeVariant) -> Result<Palette, ThemeProblem> {
        let section = variant.key();
        let color = |key: &str, value: &Option<String>| -> Result<Option<Color>, ThemeProblem> {
            value
                .as_deref()
                .map(|text| {
                    Color::parse(text).ok_or_else(|| ThemeProblem {
                        field: format!("{section}.{key}"),
                        reason: format!("{text:?} is not a colour such as \"#1a1b26\""),
                    })
                })
                .transpose()
        };
        let required = |key: &str, value: &Option<String>| -> Result<Color, ThemeProblem> {
            color(key, value)?.ok_or_else(|| ThemeProblem {
                field: format!("{section}.{key}"),
                reason: "is required".into(),
            })
        };

        let background = required("background", &self.background)?;
        let text = required("text", &self.text)?;
        let accent = required("accent", &self.accent)?;
        let sidebar = color("sidebar", &self.sidebar)?;
        let window = color("window", &self.window)?;
        let title_bar = color("title_bar", &self.title_bar)?;
        let status = StatusDefaults::of(variant);

        let sidebar_color = sidebar
            .or(window)
            .unwrap_or_else(|| background.mix(text, 0.04));
        let title_bar_color = title_bar.unwrap_or_else(|| background.mix(text, 0.08));
        Ok(Palette {
            background,
            window: window.unwrap_or(sidebar_color),
            title_bar: title_bar_color,
            sidebar: sidebar_color,
            border: color("border", &self.border)?.unwrap_or_else(|| background.mix(text, 0.16)),
            surface: color("surface", &self.surface)?.unwrap_or(title_bar_color),
            text,
            text_muted: color("text_muted", &self.text_muted)?
                .unwrap_or_else(|| muted_text(text, background)),
            accent,
            accent_text: color("accent_text", &self.accent_text)?
                .unwrap_or_else(|| text_on(accent)),
            danger: color("danger", &self.danger)?.unwrap_or(status.danger),
            success: color("success", &self.success)?.unwrap_or(status.success),
            warning: color("warning", &self.warning)?.unwrap_or(status.warning),
        })
    }
}

/// Where a theme came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThemeOrigin {
    /// Compiled into the app.
    Bundled,
    /// A file in the user's `themes/` directory.
    User(PathBuf),
}

/// A named theme: a palette for dark, for light, or for both.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Theme {
    /// Normalised name used in settings (`theme_name`): the file stem, lower-cased, with
    /// spaces and underscores as `-`.
    pub id: String,
    /// Human-readable name for pickers.
    pub name: String,
    pub dark: Option<Palette>,
    pub light: Option<Palette>,
    pub origin: ThemeOrigin,
}

/// Normalises a theme name for lookup: `Tokyo Night`, `tokyo_night` and `tokyo-night` match.
pub fn normalize_id(name: &str) -> String {
    name.trim()
        .chars()
        .map(|c| match c {
            ' ' | '_' => '-',
            c => c.to_ascii_lowercase(),
        })
        .collect()
}

/// A theme file that could not be used. `Display` is suitable for showing in the app.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ThemeError {
    #[error("could not read theme {source_name}: {message}")]
    Read {
        source_name: String,
        message: String,
    },
    #[error("theme {source_name}{}: {message}", location(*line, *column))]
    Parse {
        source_name: String,
        message: String,
        line: Option<usize>,
        column: Option<usize>,
    },
    #[error("theme {source_name}: invalid `{field}`: {reason}")]
    Invalid {
        source_name: String,
        field: String,
        reason: String,
    },
}

fn location(line: Option<usize>, column: Option<usize>) -> String {
    match (line, column) {
        (Some(l), Some(c)) => format!(":{l}:{c}"),
        (Some(l), None) => format!(":{l}"),
        _ => String::new(),
    }
}

struct ThemeProblem {
    field: String,
    reason: String,
}

impl Theme {
    /// Parses a theme file. `id_source` is the file stem (or bundled name); `source_name` is
    /// how errors name the file (a path for user themes).
    pub fn parse(
        id_source: &str,
        source_name: &str,
        text: &str,
        origin: ThemeOrigin,
    ) -> Result<Self, ThemeError> {
        let raw: RawTheme = toml::from_str(text).map_err(|e| {
            let (line, column) = e.span().map(|span| line_col(text, span.start)).unzip();
            ThemeError::Parse {
                source_name: source_name.to_owned(),
                message: e.message().to_owned(),
                line,
                column,
            }
        })?;
        let invalid = |p: ThemeProblem| ThemeError::Invalid {
            source_name: source_name.to_owned(),
            field: p.field,
            reason: p.reason,
        };
        if raw.dark.is_none() && raw.light.is_none() {
            return Err(invalid(ThemeProblem {
                field: "dark / light".into(),
                reason: "needs a [dark] or a [light] section".into(),
            }));
        }
        let dark = raw
            .dark
            .as_ref()
            .map(|p| p.resolve(ThemeVariant::Dark))
            .transpose()
            .map_err(invalid)?;
        let light = raw
            .light
            .as_ref()
            .map(|p| p.resolve(ThemeVariant::Light))
            .transpose()
            .map_err(invalid)?;
        let name = raw
            .name
            .map(|n| n.trim().to_owned())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| id_source.to_owned());
        Ok(Self {
            id: normalize_id(id_source),
            name,
            dark,
            light,
            origin,
        })
    }

    /// The palette for `want`, or the one the theme has when it only has the other variant.
    /// Returns the variant actually used.
    pub fn palette(&self, want: ThemeVariant) -> (ThemeVariant, Palette) {
        match (want, self.dark, self.light) {
            (ThemeVariant::Dark, Some(p), _) | (ThemeVariant::Light, _, Some(p)) => (want, p),
            (_, Some(p), None) => (ThemeVariant::Dark, p),
            (_, None, Some(p)) => (ThemeVariant::Light, p),
            // `parse` guarantees at least one variant.
            (_, None, None) => unreachable!("a theme has at least one palette"),
        }
    }

    /// Whether the theme has a palette for `variant`.
    pub fn has(&self, variant: ThemeVariant) -> bool {
        match variant {
            ThemeVariant::Dark => self.dark.is_some(),
            ThemeVariant::Light => self.light.is_some(),
        }
    }
}

fn line_col(text: &str, offset: usize) -> (usize, usize) {
    let before = &text[..offset.min(text.len())];
    let line = before.matches('\n').count() + 1;
    let column = before.rsplit('\n').next().map_or(0, |l| l.chars().count()) + 1;
    (line, column)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<Theme, ThemeError> {
        Theme::parse("my_theme", "my_theme.toml", text, ThemeOrigin::Bundled)
    }

    const MINIMAL: &str = r##"
[dark]
background = "#101010"
text = "#eeeeee"
accent = "#ff8800"
"##;

    #[test]
    fn colours_parse_short_and_long_hex() {
        assert_eq!(Color::parse("#fff"), Some(Color::WHITE));
        assert_eq!(Color::parse("#1A1b26"), Some(Color::from_u32(0x1a1b26)));
        assert_eq!(Color::parse(" #000000 "), Some(Color::BLACK));
        for bad in ["", "#", "fff", "#ffff", "#gggggg", "#12345", "red", "#ééé"] {
            assert_eq!(Color::parse(bad), None, "{bad:?}");
        }
        assert_eq!(Color::from_u32(0x1a1b26).to_string(), "#1a1b26");
    }

    #[test]
    fn contrast_matches_wcag() {
        assert!((Color::BLACK.contrast(Color::WHITE) - 21.0).abs() < 0.01);
        assert!((Color::WHITE.contrast(Color::WHITE) - 1.0).abs() < 0.001);
    }

    #[test]
    fn minimal_theme_derives_the_rest() {
        let theme = parse(MINIMAL).unwrap();
        assert_eq!(theme.id, "my-theme");
        assert_eq!(theme.name, "my_theme");
        assert!(theme.light.is_none());
        let p = theme.dark.unwrap();
        assert_eq!(p.background, Color::from_u32(0x101010));
        assert_eq!(p.window, p.sidebar);
        assert_ne!(p.title_bar, p.background);
        assert!(p.text_muted.contrast(p.background) >= 4.5);
        assert_eq!(p.accent_text, Color::BLACK);
        assert_eq!(p.danger, StatusDefaults::of(ThemeVariant::Dark).danger);
    }

    #[test]
    fn explicit_values_win_over_derived_ones() {
        let theme = parse(
            r##"
name = "Mine"
[light]
background = "#ffffff"
text = "#000000"
accent = "#336699"
sidebar = "#eeeeee"
border = "#abcdef"
danger = "#f00"
"##,
        )
        .unwrap();
        assert_eq!(theme.name, "Mine");
        let p = theme.light.unwrap();
        assert_eq!(p.sidebar, Color::from_u32(0xeeeeee));
        assert_eq!(p.window, p.sidebar, "window follows sidebar when not given");
        assert_eq!(p.border, Color::from_u32(0xabcdef));
        assert_eq!(p.danger, Color::from_u32(0xff0000));
        assert_eq!(p.accent_text, Color::WHITE);
        assert_eq!(p.success, StatusDefaults::of(ThemeVariant::Light).success);
    }

    #[test]
    fn missing_required_keys_name_the_field() {
        let err = parse("[dark]\nbackground = \"#000\"\ntext = \"#fff\"\n").unwrap_err();
        assert_eq!(
            err,
            ThemeError::Invalid {
                source_name: "my_theme.toml".into(),
                field: "dark.accent".into(),
                reason: "is required".into()
            }
        );
        let err = parse("name = \"x\"").unwrap_err();
        assert!(matches!(err, ThemeError::Invalid { .. }), "{err}");
    }

    #[test]
    fn bad_colour_and_bad_toml_are_errors() {
        let err = parse(&MINIMAL.replace("#ff8800", "orange")).unwrap_err();
        let shown = err.to_string();
        assert!(
            shown.contains("dark.accent") && shown.contains("orange"),
            "{shown}"
        );

        let err = parse("[dark\nbackground = 1").unwrap_err();
        assert!(
            matches!(err, ThemeError::Parse { line: Some(1), .. }),
            "{err:?}"
        );
        // A wrong value type is a parse error too.
        assert!(parse("[dark]\nbackground = 5\n").is_err());
    }

    #[test]
    fn unknown_keys_are_ignored() {
        let text = format!("future = true\n{MINIMAL}extra = 1\n");
        assert!(parse(&text).is_ok());
    }

    #[test]
    fn palette_falls_back_to_the_variant_the_theme_has() {
        let dark_only = parse(MINIMAL).unwrap();
        assert_eq!(dark_only.palette(ThemeVariant::Light).0, ThemeVariant::Dark);
        assert_eq!(dark_only.palette(ThemeVariant::Dark).0, ThemeVariant::Dark);
        assert!(dark_only.has(ThemeVariant::Dark) && !dark_only.has(ThemeVariant::Light));
        let light_only = parse(&MINIMAL.replace("[dark]", "[light]")).unwrap();
        assert_eq!(
            light_only.palette(ThemeVariant::Dark).0,
            ThemeVariant::Light
        );
    }

    #[test]
    fn ids_are_normalised() {
        assert_eq!(normalize_id("Tokyo Night"), "tokyo-night");
        assert_eq!(normalize_id(" Rosé_Pine "), "rosé-pine");
        assert_eq!(normalize_id("nord"), "nord");
    }

    #[test]
    fn muted_text_keeps_contrast_even_for_low_contrast_themes() {
        let bg = Color::from_u32(0x777777);
        let text = Color::from_u32(0x888888);
        // Cannot reach 4.5, falls back to the text colour itself.
        assert_eq!(muted_text(text, bg), text);
        let bg = Color::from_u32(0x101010);
        let text = Color::from_u32(0xffffff);
        assert!(muted_text(text, bg).contrast(bg) >= 4.5);
        assert_ne!(muted_text(text, bg), text);
    }
}
