use std::collections::HashMap;
use std::time::Instant;

use serde_json::json;

use super::*;
use gpui_chat_core::ChannelId;

fn cosmetics() -> (Arc<BadgeRegistry>, Cosmetics) {
    let registry = Arc::new(BadgeRegistry::new());
    (
        registry.clone(),
        Cosmetics::new(registry, CosmeticOptions::default()),
    )
}

fn badge(set: &str, version: &str, title: &str) -> Badge {
    Badge {
        id: BadgeId::new(set, version),
        title: title.to_owned(),
        urls: ImageUrls::new().with(ImageScale::X1, "https://x/1"),
    }
}

fn gradient(id: &str) -> Paint {
    parse_paint(
        &json!({"id": id, "function": "LINEAR_GRADIENT", "angle": 90,
        "stops": [{"at": 0, "color": -1}, {"at": 1, "color": -16777216}]}),
    )
    .unwrap()
}

#[test]
fn a_badge_is_a_7tv_badge_with_the_provider_in_its_title() {
    let value = json!({"id": "B1", "name": "Cutely", "tooltip": "A cute fashionista", "tag": "cute",
        "host": {"url": "//cdn.7tv.app/badge/B1", "files": [
            {"name": "1x.avif", "format": "AVIF"}, {"name": "1x.webp", "format": "WEBP"},
            {"name": "4x.webp", "format": "WEBP"}]}});
    let badge = parse_seventv_badge(&value).unwrap();
    assert_eq!(
        (badge.id.set.as_ref(), badge.id.version.as_ref()),
        ("seventv", "B1")
    );
    assert_eq!(badge.title, "7TV: A cute fashionista");
    assert_eq!(
        badge.urls.get(ImageScale::X1),
        Some("https://cdn.7tv.app/badge/B1/1x.webp")
    );
    assert_eq!(
        badge.urls.get(ImageScale::X4),
        Some("https://cdn.7tv.app/badge/B1/4x.webp")
    );
    // No tooltip: the name stands in. No usable image: no badge.
    let named = json!({"id": "B2", "name": "Plain", "host": {"url": "//cdn.7tv.app/badge/b", "files": [{"name": "1x.webp", "format": "WEBP"}]}});
    assert_eq!(parse_seventv_badge(&named).unwrap().title, "7TV: Plain");
    let none = json!({"id": "B3", "host": {"url": "//cdn.7tv.app/badge/b", "files": [{"name": "1x.avif", "format": "AVIF"}]}});
    assert!(parse_seventv_badge(&none).is_none());
}

#[test]
fn badges_come_in_provider_order_after_twitchs() {
    let (_, cosmetics) = cosmetics();
    let now = Instant::now();
    cosmetics.set_style(
        "42",
        UserStyle {
            paint_id: None,
            badge_id: Some("B1".into()),
        },
        now,
    );
    cosmetics.store_ffz(
        vec![badge("ffz", "2", "FFZ: Bot")],
        HashMap::from([(Arc::from("42"), vec![Arc::from("2")])]),
    );
    cosmetics.store_chatterino(
        vec![badge("chatterino", "0", "Chatterino Supporter")],
        HashMap::from([(Arc::from("42"), vec![Arc::from("0")])]),
    );
    let ids = cosmetics.badge_ids("42");
    let shown: Vec<_> = ids
        .iter()
        .map(|id| format!("{}/{}", id.set, id.version))
        .collect();
    assert_eq!(shown, ["seventv/B1", "ffz/2", "chatterino/0"]);
    // Someone else has none.
    assert!(cosmetics.badge_ids("7").is_empty());
}

#[test]
fn a_provider_switched_off_hides_its_definitions_and_back_on_shows_them() {
    let (registry, cosmetics) = cosmetics();
    cosmetics.store_ffz(vec![badge("ffz", "2", "FFZ: Bot")], HashMap::new());
    cosmetics.store_seventv_badge(badge("seventv", "B1", "7TV: x"));
    let channel = ChannelId::new("c");
    let find = |set: &str, v: &str| registry.lookup_str(&channel, set, v).is_some();
    assert!(find("ffz", "2") && find("seventv", "B1"));
    let version = registry.version(&channel);

    cosmetics.set_options(CosmeticOptions {
        ffz_badges: false,
        ..CosmeticOptions::default()
    });
    assert!(!find("ffz", "2") && find("seventv", "B1"));
    // Views notice: the registry version moved, so they lay messages out again.
    assert!(registry.version(&channel) > version);

    cosmetics.set_options(CosmeticOptions::default());
    assert!(find("ffz", "2"));
    // Setting what is already set changes nothing.
    let version = registry.version(&channel);
    cosmetics.set_options(CosmeticOptions::default());
    assert_eq!(registry.version(&channel), version);
}

#[test]
fn a_known_definition_stored_again_does_not_move_the_registry() {
    let (registry, cosmetics) = cosmetics();
    let channel = ChannelId::new("c");
    cosmetics.store_seventv_badge(badge("seventv", "B1", "7TV: x"));
    let version = registry.version(&channel);
    cosmetics.store_seventv_badge(badge("seventv", "B1", "7TV: x"));
    assert_eq!(registry.version(&channel), version);
    cosmetics.store_seventv_badge(badge("seventv", "B1", "7TV: renamed"));
    assert!(registry.version(&channel) > version);
}

#[test]
fn a_paint_needs_a_style_a_definition_and_the_switch() {
    let (_, cosmetics) = cosmetics();
    let now = Instant::now();
    assert!(cosmetics.paint("42").is_none());
    // Entitled before the definition arrived: nothing yet, then the paint.
    cosmetics.entitle("42", true, "P1", true, now);
    assert!(cosmetics.paint("42").is_none());
    cosmetics.store_paint(gradient("P1"));
    assert_eq!(
        cosmetics.paint("42").map(|p| p.id.to_string()),
        Some("P1".to_owned())
    );
    // Paints off: none, whatever is stored.
    let before = cosmetics.version();
    cosmetics.set_options(CosmeticOptions {
        paints: false,
        ..CosmeticOptions::default()
    });
    assert!(cosmetics.paint("42").is_none());
    assert!(cosmetics.version() > before);
    cosmetics.set_options(CosmeticOptions::default());
    assert!(cosmetics.paint("42").is_some());
    // Losing some other paint changes nothing; losing this one does.
    cosmetics.entitle("42", true, "P2", false, now);
    assert!(cosmetics.paint("42").is_some());
    cosmetics.entitle("42", true, "P1", false, now);
    assert!(cosmetics.paint("42").is_none());
    // An event alone does not count as a lookup: the rest of the style is still unknown.
    assert!(cosmetics.needs_lookup("42", now));
}

#[test]
fn an_image_paint_with_no_base_colour_is_not_drawable() {
    let (_, cosmetics) = cosmetics();
    cosmetics.store_paint(
        parse_paint(
            &json!({"id": "I", "function": "URL", "image_url": "https://cdn.7tv.app/paint/x/1x.webp", "stops": []}),
        )
        .unwrap(),
    );
    cosmetics.entitle("1", true, "I", true, Instant::now());
    assert!(cosmetics.paint("1").is_none());
}

#[test]
fn lookups_are_remembered_for_a_while() {
    let (_, cosmetics) = cosmetics();
    let now = Instant::now();
    assert!(cosmetics.needs_lookup("5", now));
    cosmetics.set_style("5", UserStyle::default(), now);
    assert!(!cosmetics.needs_lookup("5", now));
    assert!(cosmetics.needs_lookup("5", now + STYLE_TTL + Duration::from_secs(1)));
}

#[test]
fn badges_and_paint_images_on_other_hosts_are_refused() {
    let badge = |url: &str| json!({"id": "B", "tooltip": "t", "host": {"url": url, "files": [{"name": "1x.webp", "format": "WEBP"}]}});
    assert!(parse_seventv_badge(&badge("//cdn.7tv.app/badge/B")).is_some());
    assert!(parse_seventv_badge(&badge("//evil.example/badge/B")).is_none());
    assert!(parse_seventv_badge(&badge("http://cdn.7tv.app/badge/B")).is_none());
    // An image paint from elsewhere is no image paint: its base colour (or nothing) is left.
    let paint = parse_paint(&json!({"id": "P", "function": "URL", "image_url": "https://evil.example/p.webp", "color": -1, "stops": []})).unwrap();
    assert_eq!(paint.fill, PaintFill::Solid(PaintColor::WHITE));
}

#[test]
fn the_style_cache_is_bounded_and_forgets_the_oldest() {
    let (_, cosmetics) = cosmetics();
    let start = Instant::now();
    // Fill it past the cap with styles looked up one second apart.
    for i in 0..(STYLE_CAP + 50) {
        cosmetics.set_style(
            &i.to_string(),
            UserStyle::default(),
            start + Duration::from_millis(i as u64),
        );
    }
    let kept = cosmetics.style_count();
    assert!(kept <= STYLE_CAP, "{kept}");
    // The oldest went, the newest stayed.
    assert!(cosmetics.needs_lookup("0", start + Duration::from_secs(1)));
    assert!(!cosmetics.needs_lookup(
        &(STYLE_CAP + 49).to_string(),
        start + Duration::from_secs(1)
    ));
}

#[test]
fn entries_past_their_time_go_first_and_a_delete_for_a_stranger_makes_nothing() {
    let (_, cosmetics) = cosmetics();
    let start = Instant::now();
    cosmetics.set_style("old", UserStyle::default(), start);
    // Well past the TTL, a style for someone else is stored: the prune drops "old" even though the
    // cache is nowhere near its cap.
    cosmetics.prune(start + STYLE_TTL + Duration::from_secs(1));
    assert_eq!(cosmetics.style_count(), 0);
    cosmetics.entitle("stranger", true, "P1", false, start);
    assert_eq!(
        cosmetics.style_count(),
        0,
        "losing something nobody was known to wear"
    );
    cosmetics.entitle("stranger", true, "P1", true, start);
    assert_eq!(cosmetics.style_count(), 1);
}
