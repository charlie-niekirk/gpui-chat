use super::*;

fn text(s: &str) -> Fragment {
    Fragment::Text(s.to_owned())
}

fn emoji(s: &str) -> Fragment {
    Fragment::Emoji { text: s.to_owned() }
}

fn link(text: &str, url: &str) -> Fragment {
    Fragment::Link {
        text: text.to_owned(),
        url: url.to_owned(),
    }
}

fn mention(text: &str, login: &str, is_self: bool) -> Fragment {
    Fragment::Mention {
        text: text.to_owned(),
        login: login.to_owned(),
        is_self,
    }
}

fn emote_frag(name: &str, overlays: &[&str]) -> Fragment {
    Fragment::Emote {
        emote: Arc::new(test_emote(name, false)),
        zero_width_overlays: overlays
            .iter()
            .map(|n| Arc::new(test_emote(n, true)))
            .collect(),
    }
}

fn test_emote(name: &str, zero_width: bool) -> Emote {
    let mut e = Emote::twitch(name, name);
    e.zero_width = zero_width;
    e
}

fn tok(text: &str) -> Vec<Fragment> {
    Tokenizer::new().tokenize(text, &[]).fragments
}

/// Compares emote fragments by name/overlay names only (ids differ per helper).
fn shape(fragments: &[Fragment]) -> Vec<String> {
    fragments
        .iter()
        .map(|f| match f {
            Fragment::Text(t) => format!("T:{t}"),
            Fragment::Emote {
                emote,
                zero_width_overlays,
            } => {
                let overlays: Vec<&str> = zero_width_overlays
                    .iter()
                    .map(|e| &*e.name as &str)
                    .collect();
                format!("E:{}+{}", emote.name, overlays.join("+"))
            }
            Fragment::Emoji { text } => format!("J:{text}"),
            Fragment::Mention { text, is_self, .. } => format!("M:{text}:{is_self}"),
            Fragment::Link { text, .. } => format!("L:{text}"),
            Fragment::Cheer { prefix, bits } => format!("C:{prefix}:{bits}"),
        })
        .collect()
}

fn range(name: &str, start: usize, end: usize) -> EmoteRange {
    EmoteRange {
        emote: Arc::new(test_emote(name, false)),
        start,
        end,
    }
}

#[test]
fn plain_text_is_one_run() {
    assert_eq!(
        tok("hello  brave\tnew world"),
        [text("hello  brave\tnew world")]
    );
    assert_eq!(tok(""), []);
    assert_eq!(tok("   "), [text("   ")]);
}

// ---- Twitch ranges: code points ------------------------------------------------

#[test]
fn ranges_are_code_points_with_multibyte_text_before() {
    // "é" is 2 bytes / 1 unit; "日本" 3 bytes each. Kappa is at code points 5..=9.
    let s = "é日本 x Kappa end";
    let cp = s.chars().position(|c| c == 'K').unwrap();
    assert_eq!(cp, 6);
    let r = [range("Kappa", cp, cp + 4)];
    let out = Tokenizer::new().tokenize(s, &r).fragments;
    assert_eq!(shape(&out), ["T:é日本 x ", "E:Kappa+", "T: end"]);
}

#[test]
fn ranges_are_code_points_with_emoji_before() {
    // 😀 is 1 code point but 4 UTF-8 bytes and 2 UTF-16 units; the ZWJ family is 7
    // code points. Ranges from Twitch point past them in code points.
    let s = "😀 👨\u{200D}👩\u{200D}👧\u{200D}👦 Kappa!";
    let start = s.chars().position(|c| c == 'K').unwrap();
    assert_eq!(start, 10);
    let r = [range("Kappa", start, start + 4)];
    let out = Tokenizer::new().tokenize(s, &r).fragments;
    assert_eq!(
        shape(&out),
        [
            "J:😀",
            "T: ",
            "J:👨\u{200D}👩\u{200D}👧\u{200D}👦",
            "T: ",
            "E:Kappa+",
            "T:!"
        ]
    );
}

#[test]
fn byte_or_utf16_interpretation_would_be_wrong() {
    let s = "😀😀 Kappa";
    // Code points: 😀(0) 😀(1) ' '(2) K(3)..a(7). Byte offsets would be 9..=13.
    let r = [range("Kappa", 3, 7)];
    let out = Tokenizer::new().tokenize(s, &r).fragments;
    assert_eq!(shape(&out), ["J:😀", "J:😀", "T: ", "E:Kappa+"]);
}

#[test]
fn emote_at_start_and_end_and_adjacent() {
    let s = "Kappa Kappa";
    let r = [range("Kappa", 0, 4), range("Kappa", 6, 10)];
    let out = Tokenizer::new().tokenize(s, &r).fragments;
    assert_eq!(shape(&out), ["E:Kappa+", "T: ", "E:Kappa+"]);
    let s = "KappaKappa";
    let r = [range("Kappa", 0, 4), range("Kappa", 5, 9)];
    let out = Tokenizer::new().tokenize(s, &r).fragments;
    assert_eq!(shape(&out), ["E:Kappa+", "E:Kappa+"]);
}

#[test]
fn out_of_range_inverted_and_overlapping_ranges_do_not_panic() {
    let s = "a Kappa b";
    let kappa = || range("Kappa", 2, 6);
    let cases: Vec<Vec<EmoteRange>> = vec![
        vec![range("X", 5, 100)],               // end past the text
        vec![range("X", 100, 200)],             // entirely outside
        vec![range("X", 4, 2)],                 // inverted
        vec![range("X", 0, s.chars().count())], // one past the last code point
        vec![range("X", usize::MAX, usize::MAX)],
        vec![range("X", 0, usize::MAX)],
        vec![kappa(), range("Y", 4, 8)], // overlap
        vec![kappa(), kappa()],          // duplicate
        vec![range("Y", 4, 8), kappa()], // overlap, unsorted
    ];
    for ranges in cases {
        let out = Tokenizer::new().tokenize(s, &ranges);
        // Whatever is dropped, the text is never lost or duplicated by more than
        // the emote names.
        assert!(!out.fragments.is_empty(), "{ranges:?}");
    }
    // Invalid ranges are ignored entirely: plain text.
    let out = Tokenizer::new().tokenize(s, &[range("X", 5, 100), range("X", 4, 2)]);
    assert_eq!(out.fragments, [text(s)]);
}

#[test]
fn overlapping_ranges_keep_the_earlier_one() {
    let s = "a Kappa b";
    let r = [range("Second", 4, 8), range("Kappa", 2, 6)];
    let out = Tokenizer::new().tokenize(s, &r).fragments;
    assert_eq!(shape(&out), ["T:a ", "E:Kappa+", "T: b"]);
    // Equal starts: the longer one wins.
    let r = [range("Short", 2, 3), range("Long", 2, 6)];
    let out = Tokenizer::new().tokenize(s, &r).fragments;
    assert_eq!(shape(&out), ["T:a ", "E:Long+", "T: b"]);
}

#[test]
fn range_inside_a_word_splits_it() {
    // Not produced by Twitch in practice, but must be well-formed.
    let out = Tokenizer::new()
        .tokenize("xKappay", &[range("Kappa", 1, 5)])
        .fragments;
    assert_eq!(shape(&out), ["T:x", "E:Kappa+", "T:y"]);
}

#[test]
fn ranges_on_empty_text_are_ignored() {
    let out = Tokenizer::new().tokenize("", &[range("Kappa", 0, 0)]);
    assert!(out.fragments.is_empty());
}

#[test]
fn emotes_tag_parsing() {
    let raw = parse_emotes_tag("25:0-4,12-16/1902:6-10");
    let got: Vec<(&str, usize, usize)> = raw
        .iter()
        .map(|r| (r.id.as_str(), r.start, r.end))
        .collect();
    assert_eq!(got, [("25", 0, 4), ("25", 12, 16), ("1902", 6, 10)]);
    assert!(parse_emotes_tag("").is_empty());
    assert!(parse_emotes_tag("garbage").is_empty());
    assert!(parse_emotes_tag("25:a-b,3,-4,1-").is_empty());
    assert!(parse_emotes_tag(":0-4").is_empty());
    // Mixed good and bad entries keep the good ones.
    assert_eq!(parse_emotes_tag("x/25:0-4/y:z").len(), 1);
}

#[test]
fn twitch_emote_ranges_names_emotes_by_code_points() {
    let s = "😀 é Kappa Keepo Kappa";
    // code points: 😀0 ' '1 é2 ' '3 K4..a8 ' '9 K10..o14 ' '15 K16..a20
    let ranges = twitch_emote_ranges("25:4-8,16-20/1902:10-14", s);
    assert_eq!(ranges.len(), 3);
    assert_eq!(&*ranges[0].emote.name, "Kappa");
    assert_eq!(&*ranges[2].emote.name, "Keepo");
    assert_eq!(ranges[0].emote.id.as_str(), "25");
    assert!(
        Arc::ptr_eq(&ranges[0].emote, &ranges[1].emote),
        "same id shares one Arc"
    );
    // Round trip through the tokenizer.
    let mut sorted = ranges;
    sorted.sort_by_key(|r| r.start);
    let out = Tokenizer::new().tokenize(s, &sorted).fragments;
    assert_eq!(
        shape(&out),
        [
            "J:😀", "T: é ", "E:Kappa+", "T: ", "E:Keepo+", "T: ", "E:Kappa+"
        ]
    );
    // Out-of-range entries are dropped rather than panicking.
    assert!(twitch_emote_ranges("25:0-99", "short").is_empty());
    assert!(twitch_emote_ranges("25:3-1", "short").is_empty());
}

// ---- Links -----------------------------------------------------------------------

#[test]
fn links_with_scheme_and_bare_domains() {
    assert_eq!(
        tok("see https://example.com/a?b=1 ok"),
        [
            text("see "),
            link("https://example.com/a?b=1", "https://example.com/a?b=1"),
            text(" ok")
        ]
    );
    assert_eq!(
        tok("twitch.tv/forsen"),
        [link("twitch.tv/forsen", "https://twitch.tv/forsen")]
    );
    assert_eq!(tok("not a link.notatld"), [text("not a link.notatld")]);
    assert_eq!(tok("e.g. this"), [text("e.g. this")]);
}

#[test]
fn trailing_punctuation_is_not_part_of_the_link() {
    for (input, link_text, tail) in [
        ("example.com.", "example.com", "."),
        ("example.com,", "example.com", ","),
        ("example.com!?", "example.com", "!?"),
        ("https://example.com/a).", "https://example.com/a", ")."),
        ("example.com/path;", "example.com/path", ";"),
    ] {
        let out = tok(input);
        assert_eq!(out.len(), 2, "{input}: {out:?}");
        assert!(
            matches!(&out[0], Fragment::Link { text, .. } if text == link_text),
            "{input}"
        );
        assert_eq!(out[1], text(tail), "{input}");
    }
}

#[test]
fn surrounding_brackets_and_balanced_parens() {
    assert_eq!(
        tok("(example.com)"),
        [
            text("("),
            link("example.com", "https://example.com"),
            text(")")
        ]
    );
    assert_eq!(
        tok("<https://example.com>"),
        [
            text("<"),
            link("https://example.com", "https://example.com"),
            text(">")
        ]
    );
    // The paren belongs to the URL when it has its opener.
    let url = "https://en.wikipedia.org/wiki/Foo_(bar)";
    assert_eq!(tok(url), [link(url, url)]);
    assert_eq!(
        tok(&format!("({url})")),
        [text("("), link(url, url), text(")")]
    );
}

#[test]
fn link_followed_by_emoji_stops_at_the_emoji() {
    assert_eq!(
        tok("example.com😀"),
        [link("example.com", "https://example.com"), emoji("😀")]
    );
}

#[test]
fn bare_words_with_dots_are_not_links() {
    for s in [
        "wow...no",
        "3.14",
        "v1.2.3",
        "..",
        "a.b",
        "user@example.com",
        "Mr.Smith",
    ] {
        assert_eq!(tok(s), [text(s)], "{s}");
    }
}

// ---- Mentions --------------------------------------------------------------------

#[test]
fn mentions_basic_and_punctuation() {
    assert_eq!(
        tok("hi @Alice_1!"),
        [
            text("hi "),
            mention("@Alice_1", "alice_1", false),
            text("!")
        ]
    );
    assert_eq!(
        tok("(@bob)"),
        [text("("), mention("@bob", "bob", false), text(")")]
    );
    assert_eq!(
        tok("@bob, yes"),
        [mention("@bob", "bob", false), text(", yes")]
    );
    assert_eq!(tok("@bob's"), [mention("@bob", "bob", false), text("'s")]);
}

#[test]
fn lone_at_and_long_names_are_not_mentions() {
    assert_eq!(tok("@ hello"), [text("@ hello")]);
    assert_eq!(tok("@@"), [text("@@")]);
    let long = format!("@{}", "a".repeat(26));
    assert_eq!(tok(&long), [text(&long)]);
    assert_eq!(tok("foo@bar"), [text("foo@bar")]);
}

#[test]
fn mention_followed_by_emoji() {
    assert_eq!(tok("@bob😀"), [mention("@bob", "bob", false), emoji("😀")]);
}

#[test]
fn own_login_is_flagged_case_insensitively() {
    let t = Tokenizer::new().with_own_login("Streamer");
    let out = t.tokenize("hey @STREAMER and @other", &[]);
    assert!(out.mentions_self);
    assert_eq!(
        shape(&out.fragments),
        ["T:hey ", "M:@STREAMER:true", "T: and ", "M:@other:false"]
    );
    let out = t.tokenize("hey @other", &[]);
    assert!(!out.mentions_self);
}

#[test]
fn plain_own_name_flags_but_stays_text() {
    let t = Tokenizer::new().with_own_login("streamer");
    let out = t.tokenize("hello Streamer!", &[]);
    assert!(out.mentions_self);
    assert_eq!(out.fragments, [text("hello Streamer!")]);
    let out = t.tokenize("hello streamers", &[]);
    assert!(!out.mentions_self, "substring is not a mention");
    assert!(
        !Tokenizer::new()
            .tokenize("hello streamer", &[])
            .mentions_self
    );
}

// ---- Emoji -----------------------------------------------------------------------

#[test]
fn emoji_are_split_out_and_sequences_stay_whole() {
    assert_eq!(tok("hi😀there"), [text("hi"), emoji("😀"), text("there")]);
    let family = "👨\u{200D}👩\u{200D}👧\u{200D}👦";
    assert_eq!(
        tok(&format!("a {family} b")),
        [text("a "), emoji(family), text(" b")]
    );
    let tones = "👍🏽";
    assert_eq!(tok(tones), [emoji(tones)]);
    assert_eq!(tok("🇯🇵🇺🇸"), [emoji("🇯🇵"), emoji("🇺🇸")]);
    let rainbow = "🏳\u{FE0F}\u{200D}🌈";
    assert_eq!(tok(rainbow), [emoji(rainbow)]);
    let keycap = "1\u{FE0F}\u{20E3}";
    assert_eq!(
        tok(&format!("press {keycap}")),
        [text("press "), emoji(keycap)]
    );
}

#[test]
fn text_symbols_without_vs16_are_not_emoji() {
    assert_eq!(tok("© ™ ❤ 123 #1"), [text("© ™ ❤ 123 #1")]);
    let heart = "❤\u{FE0F}";
    assert_eq!(tok(heart), [emoji(heart)]);
}

#[test]
fn consecutive_emoji_are_separate_fragments() {
    assert_eq!(tok("😀😀"), [emoji("😀"), emoji("😀")]);
    assert_eq!(tok("😀 😀"), [emoji("😀"), text(" "), emoji("😀")]);
}

// ---- Resolver hook and zero-width stacking ---------------------------------------

fn resolver(
    zero_width: &'static [&'static str],
    normal: &'static [&'static str],
) -> impl Fn(&str) -> Option<Arc<Emote>> {
    move |word| {
        if zero_width.contains(&word) {
            Some(Arc::new(test_emote(word, true)))
        } else if normal.contains(&word) {
            Some(Arc::new(test_emote(word, false)))
        } else {
            None
        }
    }
}

#[test]
fn resolver_matches_whole_words_only() {
    let r = resolver(&[], &["monkaS", ":)", "o_O"]);
    let t = Tokenizer::new().with_resolver(&r);
    let out = t
        .tokenize("hi monkaS monkaS, :) o_O xmonkaS", &[])
        .fragments;
    assert_eq!(
        shape(&out),
        [
            "T:hi ",
            "E:monkaS+",
            "T: monkaS, ",
            "E::)+",
            "T: ",
            "E:o_O+",
            "T: xmonkaS"
        ]
    );
}

#[test]
fn zero_width_stacks_on_the_preceding_emote() {
    let r = resolver(&["SteerR", "Rain"], &["Base", "Other"]);
    let t = Tokenizer::new().with_resolver(&r);
    let out = t.tokenize("Base SteerR Rain Other", &[]).fragments;
    assert_eq!(shape(&out), ["E:Base+SteerR+Rain", "T: ", "E:Other+"]);
    assert_eq!(out[0], emote_frag("Base", &["SteerR", "Rain"]));
}

#[test]
fn zero_width_stacks_on_a_twitch_range_emote() {
    let r = resolver(&["SteerR"], &[]);
    let t = Tokenizer::new().with_resolver(&r);
    let out = t
        .tokenize("Kappa SteerR", &[range("Kappa", 0, 4)])
        .fragments;
    assert_eq!(shape(&out), ["E:Kappa+SteerR"]);
}

#[test]
fn zero_width_without_a_preceding_emote_is_a_normal_emote() {
    let r = resolver(&["SteerR"], &[]);
    let t = Tokenizer::new().with_resolver(&r);
    // First thing in the message.
    assert_eq!(shape(&t.tokenize("SteerR", &[]).fragments), ["E:SteerR+"]);
    // After text.
    assert_eq!(
        shape(&t.tokenize("hello SteerR", &[]).fragments),
        ["T:hello ", "E:SteerR+"]
    );
    // After an emoji.
    assert_eq!(
        shape(&t.tokenize("😀 SteerR", &[]).fragments),
        ["J:😀", "T: ", "E:SteerR+"]
    );
}

#[test]
fn text_between_breaks_the_stack() {
    let r = resolver(&["SteerR"], &["Base"]);
    let t = Tokenizer::new().with_resolver(&r);
    let out = t.tokenize("Base and SteerR", &[]).fragments;
    assert_eq!(shape(&out), ["E:Base+", "T: and ", "E:SteerR+"]);
    // Punctuation (non-whitespace text) breaks it as well.
    let out = t.tokenize("Base ... SteerR", &[]).fragments;
    assert_eq!(shape(&out), ["E:Base+", "T: ... ", "E:SteerR+"]);
}

#[test]
fn whitespace_before_an_overlay_is_dropped_not_kept() {
    let r = resolver(&["SteerR"], &["Base"]);
    let t = Tokenizer::new().with_resolver(&r);
    let out = t.tokenize("Base   \t SteerR tail", &[]).fragments;
    assert_eq!(shape(&out), ["E:Base+SteerR", "T: tail"]);
}

#[test]
fn resolver_runs_before_link_and_mention_detection() {
    // An emote literally named like a domain must win over link detection.
    let r = resolver(&[], &["lol.gg"]);
    let t = Tokenizer::new().with_resolver(&r);
    assert_eq!(shape(&t.tokenize("lol.gg", &[]).fragments), ["E:lol.gg+"]);
    assert_eq!(
        shape(&Tokenizer::new().tokenize("lol.gg", &[]).fragments),
        ["L:lol.gg"]
    );
}

#[test]
fn resolver_is_called_per_word_only() {
    use std::cell::RefCell;
    let seen = RefCell::new(Vec::new());
    let r = |w: &str| {
        seen.borrow_mut().push(w.to_owned());
        None
    };
    let t = Tokenizer::new().with_resolver(&r);
    t.tokenize("a  bb\nccc", &[range("X", 0, 0)]);
    assert_eq!(
        *seen.borrow(),
        ["bb", "ccc"],
        "range-covered text is not resolved"
    );
}

// ---- Cheers ----------------------------------------------------------------------

#[test]
fn cheermotes_need_a_known_prefix_and_clean_digits() {
    let t = Tokenizer::new().with_cheermote_prefixes(DEFAULT_CHEERMOTE_PREFIXES);
    let out = t.tokenize("Cheer100 kappa5 Cheer", &[]).fragments;
    assert_eq!(shape(&out), ["C:Cheer:100", "T: ", "C:kappa:5", "T: Cheer"]);
    for s in [
        "Cheer0",
        "Cheer01",
        "Cheer1x",
        "Cheer100,",
        "Unknown100",
        "100",
    ] {
        assert_eq!(t.tokenize(s, &[]).fragments, [text(s)], "{s}");
    }
    assert_eq!(
        t.tokenize("Cheer99999999999999", &[]).fragments,
        [text("Cheer99999999999999")],
        "u32 overflow is not a cheer"
    );
    // Off by default.
    assert_eq!(tok("Cheer100"), [text("Cheer100")]);
}

#[test]
fn cheermote_prefix_may_contain_digits() {
    let t = Tokenizer::new().with_cheermote_prefixes(DEFAULT_CHEERMOTE_PREFIXES);
    let out = t.tokenize("4Head100 4head5 Kappa1000 4Head", &[]).fragments;
    assert_eq!(
        shape(&out),
        [
            "C:4Head:100",
            "T: ",
            "C:4head:5",
            "T: ",
            "C:Kappa:1000",
            "T: 4Head"
        ]
    );
    // Digits-only and zero-leading amounts are still rejected.
    for s in ["4Head0", "4Head007", "100", "4Head1x"] {
        assert_eq!(t.tokenize(s, &[]).fragments, [text(s)], "{s}");
    }
}

#[test]
fn ignored_tag_char_is_stripped() {
    let t = Tokenizer::new();
    // Appended at the end (the usual case).
    assert_eq!(
        t.tokenize("hello world \u{E0000}", &[]).fragments,
        [text("hello world ")]
    );
    assert_eq!(t.tokenize("\u{E0000}", &[]).fragments, []);
    // Inside a word: the word is rejoined, so link detection still works.
    assert_eq!(
        t.tokenize("exam\u{E0000}ple.com", &[]).fragments,
        [link("example.com", "https://example.com")]
    );
    // Other tag characters (emoji tag sequences) are not touched.
    let england = "🏴\u{E0067}\u{E0062}\u{E0065}\u{E006E}\u{E0067}\u{E007F}";
    assert_eq!(t.tokenize(england, &[]).fragments, [emoji(england)]);
}

#[test]
fn ranges_are_rebased_when_ignored_chars_are_stripped() {
    // Original code points: a0 \u{E0000}1 ' '2 K3..a7 ' '8 K9..a13
    let s = "a\u{E0000} Kappa Kappa";
    let r = [range("Kappa", 3, 7), range("Kappa", 9, 13)];
    let out = Tokenizer::new().tokenize(s, &r).fragments;
    assert_eq!(shape(&out), ["T:a ", "E:Kappa+", "T: ", "E:Kappa+"]);
    // A range that covered only the stripped char is dropped, not mis-applied.
    let out = Tokenizer::new()
        .tokenize("ab\u{E0000}", &[range("X", 2, 2)])
        .fragments;
    assert_eq!(out, [text("ab")]);
    // Out-of-range ends stay out of range after re-basing.
    let out = Tokenizer::new()
        .tokenize("a\u{E0000}b", &[range("X", 0, 3)])
        .fragments;
    assert_eq!(out, [text("ab")]);
}

// ---- Misc / robustness -----------------------------------------------------------

#[test]
fn plain_text_round_trips() {
    let r = resolver(&[], &["monkaS"]);
    let t = Tokenizer::new().with_resolver(&r).with_own_login("me");
    for s in [
        "hello world",
        "  leading and trailing  ",
        "a @bob b https://x.com/y c 😀 d monkaS e",
        "(example.com), @bob! 👍🏽",
        "日本語 テキスト 🇯🇵",
        "tabs\tand\nnewlines",
    ] {
        let out = t.tokenize(s, &[]);
        let mut rebuilt = String::new();
        for f in &out.fragments {
            f.write_plain_text(&mut rebuilt);
        }
        assert_eq!(rebuilt, s);
    }
}

#[test]
fn arbitrary_unicode_does_not_panic() {
    let t = Tokenizer::new()
        .with_own_login("me")
        .with_cheermote_prefixes(DEFAULT_CHEERMOTE_PREFIXES);
    for s in [
        "\u{200D}",
        "\u{FE0F}",
        "\u{20E3}",
        "\u{E0067}",
        "😀\u{200D}",
        "\u{200D}😀",
        "@\u{200D}",
        "https://",
        "http://😀",
        "((((",
        "))))",
        "@@@@@@",
        "\u{0}\u{7f}",
        "ａｂｃ.ｃｏｍ",
        "İ.com",
        "\u{1F3FB}",
    ] {
        let _ = t.tokenize(s, &[range("X", 0, 0), range("Y", 0, 99)]);
    }
}

#[test]
fn resolving_fragments_matches_tokenizing_with_a_resolver() {
    let known = resolver(&["RainbowPuke"], &["monkaS", "Kappa"]);
    for s in [
        "hello monkaS world",
        "monkaS",
        "monkaS RainbowPuke",
        "monkaS    RainbowPuke   RainbowPuke done",
        "RainbowPuke alone",
        "text RainbowPuke monkaS RainbowPuke",
        "a  monkaS\tb\nmonkaS",
        "monkaS, monkaS! (monkaS)",
        "no emotes here",
        "",
    ] {
        let plain = Tokenizer::new().tokenize(s, &[]).fragments;
        let wanted = Tokenizer::new()
            .with_resolver(&known)
            .tokenize(s, &[])
            .fragments;
        let got = resolve_fragments(&plain, &known).unwrap_or_else(|| plain.clone());
        assert_eq!(shape(&got), shape(&wanted), "{s:?}");
    }
}

#[test]
fn resolving_fragments_returns_none_when_nothing_matches() {
    let known = resolver(&[], &["monkaS"]);
    let plain = tok("hi there @bob https://x.com 😀 monkaSS");
    assert!(resolve_fragments(&plain, &known).is_none());
    assert!(resolve_fragments(&[], &known).is_none());
}

#[test]
fn zero_width_stacks_onto_a_twitch_emote_from_the_tag() {
    let known = resolver(&["RainbowPuke"], &[]);
    let kappa = Arc::new(Emote::twitch("25", "Kappa"));
    let ranges = [EmoteRange {
        emote: kappa,
        start: 0,
        end: 4,
    }];
    let plain = Tokenizer::new()
        .tokenize("Kappa RainbowPuke", &ranges)
        .fragments;
    let got = resolve_fragments(&plain, &known).expect("matched");
    assert_eq!(shape(&got), ["E:Kappa+RainbowPuke"]);
}

#[test]
fn a_word_glued_to_an_emoji_mention_or_link_is_not_a_whole_word() {
    let known = resolver(&[], &["abc", "bob", "x"]);
    // "abc👍" and "@bob," are single words; only the lone "abc" matches.
    let plain = tok("abc👍 @bob, (x.com) abc");
    let got = resolve_fragments(&plain, &known).expect("the last word matches");
    let emotes: Vec<&str> = got
        .iter()
        .filter_map(|f| match f {
            Fragment::Emote { emote, .. } => Some(&*emote.name as &str),
            _ => None,
        })
        .collect();
    assert_eq!(emotes, ["abc"]);
    // The emoji, mention and link survive.
    assert!(got.iter().any(|f| matches!(f, Fragment::Emoji { .. })));
    assert!(got.iter().any(|f| matches!(f, Fragment::Mention { .. })));
    assert!(got.iter().any(|f| matches!(f, Fragment::Link { .. })));
}
