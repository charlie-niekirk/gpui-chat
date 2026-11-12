//! Link detection for a single, already punctuation-trimmed token.

/// Top-level domains accepted for *bare* domains (`example.com`). Links with an
/// explicit `http(s)://` scheme are not subject to this check. Sorted; lowercase.
const TLDS: &[&str] = &[
    "ac", "ad", "ae", "af", "ag", "ai", "al", "am", "ao", "app", "aq", "ar", "art", "as", "at",
    "au", "aw", "ax", "az", "ba", "bb", "bd", "be", "bf", "bg", "bh", "bi", "biz", "bj", "blog",
    "bm", "bn", "bo", "br", "bs", "bt", "bw", "by", "bz", "ca", "cat", "cc", "cd", "cf", "cg",
    "ch", "chat", "ci", "ck", "cl", "club", "cm", "cn", "co", "com", "cr", "cu", "cv", "cw", "cx",
    "cy", "cz", "de", "dev", "dj", "dk", "dm", "do", "dz", "ec", "edu", "ee", "eg", "email", "er",
    "es", "et", "eu", "fi", "fj", "fk", "fm", "fo", "fr", "fun", "ga", "game", "games", "gd", "ge",
    "gf", "gg", "gh", "gi", "gl", "gm", "gn", "gov", "gp", "gq", "gr", "gs", "gt", "gu", "gw",
    "gy", "hk", "hm", "hn", "hr", "ht", "hu", "id", "ie", "il", "im", "in", "info", "int", "io",
    "iq", "ir", "is", "it", "je", "jm", "jo", "jobs", "jp", "ke", "kg", "kh", "ki", "km", "kn",
    "kp", "kr", "kw", "ky", "kz", "la", "lb", "lc", "li", "link", "live", "lk", "lol", "lr", "ls",
    "lt", "lu", "lv", "ly", "ma", "mc", "md", "me", "media", "mg", "mh", "mil", "mk", "ml", "mm",
    "mn", "mo", "mobi", "mp", "mq", "mr", "ms", "mt", "mu", "mv", "mw", "mx", "my", "mz", "na",
    "name", "nc", "ne", "net", "network", "news", "nf", "ng", "ni", "ninja", "nl", "no", "np",
    "nr", "nu", "nz", "om", "online", "org", "pa", "page", "pe", "pf", "pg", "ph", "pk", "pl",
    "pm", "pn", "pr", "pro", "ps", "pt", "pw", "py", "qa", "re", "ro", "rs", "ru", "rw", "sa",
    "sb", "sc", "sd", "se", "sg", "sh", "shop", "si", "site", "sk", "sl", "sm", "sn", "so",
    "social", "space", "sr", "ss", "st", "store", "stream", "studio", "su", "sv", "sx", "sy", "sz",
    "tc", "td", "tech", "tf", "tg", "th", "tj", "tk", "tl", "tm", "tn", "to", "today", "top", "tr",
    "tt", "tv", "tw", "tz", "ua", "ug", "uk", "us", "uy", "uz", "va", "vc", "ve", "vg", "vi",
    "video", "vip", "vn", "vu", "wf", "wiki", "world", "ws", "wtf", "xyz", "ye", "yt", "za", "zm",
    "zone", "zw",
];

/// If `token` is a link, returns the URL to open (always with a scheme).
///
/// Accepts `http://` / `https://` URLs (any non-empty valid host) and bare domains
/// whose last label is a known TLD (or a punycode `xn--` label), plus bare IPv4
/// addresses, each optionally followed by `:port`, a path, query or fragment.
pub(crate) fn link_url(token: &str) -> Option<String> {
    if !token.contains('.') && !token.contains("://") {
        return None;
    }
    let (explicit, rest) = match strip_scheme(token) {
        Some(rest) => (true, rest),
        None => (false, token),
    };
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    let host = split_port(authority)?;
    if !valid_host(host, explicit) {
        return None;
    }
    Some(if explicit {
        token.to_owned()
    } else {
        format!("https://{token}")
    })
}

fn strip_scheme(token: &str) -> Option<&str> {
    for scheme in ["https://", "http://"] {
        if token.len() > scheme.len()
            && token.is_char_boundary(scheme.len())
            && token[..scheme.len()].eq_ignore_ascii_case(scheme)
        {
            return Some(&token[scheme.len()..]);
        }
    }
    None
}

/// Splits an optional `:port` off the authority; `None` if it is malformed
/// (userinfo, IPv6 literals and non-numeric ports are not supported).
fn split_port(authority: &str) -> Option<&str> {
    match authority.rsplit_once(':') {
        None => Some(authority),
        Some((host, port)) => {
            let ok = !port.is_empty()
                && port.len() <= 5
                && port.bytes().all(|b| b.is_ascii_digit())
                && port.parse::<u32>().is_ok_and(|p| p <= 65_535);
            ok.then_some(host)
        }
    }
}

fn valid_host(host: &str, explicit: bool) -> bool {
    if host.is_empty() || host.len() > 253 {
        return false;
    }
    let mut labels = 0usize;
    let mut last = "";
    for label in host.split('.') {
        let valid = !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label.chars().all(|c| c.is_alphanumeric() || c == '-');
        if !valid {
            return false;
        }
        labels += 1;
        last = label;
    }
    if explicit {
        return true;
    }
    labels >= 2 && (is_known_tld(last) || is_ipv4(host))
}

fn is_known_tld(label: &str) -> bool {
    if label.len() >= 6
        && label
            .get(..4)
            .is_some_and(|p| p.eq_ignore_ascii_case("xn--"))
    {
        return true;
    }
    if label.len() > 16 || !label.is_ascii() {
        return false;
    }
    let lower = label.to_ascii_lowercase();
    TLDS.binary_search(&lower.as_str()).is_ok()
}

fn is_ipv4(host: &str) -> bool {
    let mut parts = 0;
    for part in host.split('.') {
        let ok = !part.is_empty()
            && part.len() <= 3
            && part.bytes().all(|b| b.is_ascii_digit())
            && part.parse::<u16>().is_ok_and(|n| n <= 255);
        if !ok {
            return false;
        }
        parts += 1;
    }
    parts == 4
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tld_table_is_sorted_and_unique() {
        for w in TLDS.windows(2) {
            assert!(w[0] < w[1], "{} !< {}", w[0], w[1]);
        }
        assert!(
            TLDS.iter()
                .all(|t| t.chars().all(|c| c.is_ascii_lowercase()))
        );
    }

    #[test]
    fn explicit_scheme_links() {
        assert_eq!(
            link_url("https://example.com/a?b=1#c").as_deref(),
            Some("https://example.com/a?b=1#c")
        );
        assert_eq!(
            link_url("HTTP://Example.COM").as_deref(),
            Some("HTTP://Example.COM")
        );
        assert_eq!(
            link_url("http://localhost:3000/x").as_deref(),
            Some("http://localhost:3000/x")
        );
        assert_eq!(link_url("https://"), None);
        assert_eq!(link_url("https:///path"), None);
        assert_eq!(link_url("ftp://example.com"), None);
        assert_eq!(link_url("https://a..com"), None);
        assert_eq!(link_url("https://-a.com"), None);
        assert_eq!(link_url("http://host:99999"), None);
    }

    #[test]
    fn bare_domains_need_a_known_tld() {
        assert_eq!(
            link_url("example.com").as_deref(),
            Some("https://example.com")
        );
        assert_eq!(
            link_url("twitch.tv/forsen").as_deref(),
            Some("https://twitch.tv/forsen")
        );
        assert_eq!(
            link_url("sub.example.co.uk:8080/path").as_deref(),
            Some("https://sub.example.co.uk:8080/path")
        );
        assert_eq!(
            link_url("EXAMPLE.COM").as_deref(),
            Some("https://EXAMPLE.COM")
        );
        assert_eq!(
            link_url("example.xn--p1ai").as_deref(),
            Some("https://example.xn--p1ai")
        );
        assert_eq!(
            link_url("192.168.1.1").as_deref(),
            Some("https://192.168.1.1")
        );
        assert_eq!(link_url("something.notatld"), None);
        assert_eq!(link_url("e.g"), None);
        assert_eq!(link_url("i.e"), None);
        assert_eq!(link_url("3.14"), None);
        assert_eq!(link_url("1.2.3"), None);
        assert_eq!(link_url("999.1.1.1"), None);
        assert_eq!(link_url("wow...com"), None);
        assert_eq!(link_url(".com"), None);
        assert_eq!(link_url("example."), None);
        assert_eq!(link_url("user@example.com"), None);
        assert_eq!(link_url("localhost:3000"), None);
        assert_eq!(link_url("Kappa"), None);
        // Non-ASCII labels must not trip byte slicing.
        assert_eq!(link_url("ａｂｃ.ｃｏｍ"), None);
        assert_eq!(link_url("ａｂｃ.ｘｎ--ｐｑ"), None);
    }
}
