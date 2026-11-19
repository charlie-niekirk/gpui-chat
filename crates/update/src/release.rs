//! What the GitHub "list releases" response says: which releases count, and which is the newest.
//!
//! A release counts when it is **published** (not a draft), **not a prerelease**, and its tag is
//! semver (`v0.1.0` or `0.1.0`) **without** a semver pre-release part (`v0.2.0-rc.1` is a
//! prerelease whatever the flag says). Anything else is skipped, never an error: a release that
//! cannot be understood must not hide the ones that can.

use semver::Version;
use serde::Deserialize;

/// A release the running version can be compared with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub version: Version,
    /// The git tag as published (`v0.2.0`).
    pub tag: String,
    /// The release's page on GitHub, built from the repository and the tag (the response's own
    /// `html_url` is never used, so a link the app opens always points at the repository).
    pub url: String,
}

/// The response is not a JSON list of releases.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct ParseError(pub String);

/// The fields of one release that matter; every other key is ignored.
#[derive(Debug, Deserialize)]
struct Raw {
    tag_name: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
}

/// The stable version a tag names, or `None` (not semver, or a semver pre-release).
pub fn stable_version(tag: &str) -> Option<Version> {
    let version = Version::parse(tag.strip_prefix('v').unwrap_or(tag)).ok()?;
    version.pre.is_empty().then_some(version)
}

/// The release page of `tag` in `repo` (`owner/name`).
pub fn release_url(repo: &str, tag: &str) -> String {
    format!("https://github.com/{repo}/releases/tag/{tag}")
}

/// The release for `tag` if it names a stable version.
pub fn release_for_tag(repo: &str, tag: &str) -> Option<Release> {
    Some(Release {
        version: stable_version(tag)?,
        tag: tag.to_owned(),
        url: release_url(repo, tag),
    })
}

/// The counting releases of a "list releases" response body, newest version first.
///
/// The body must be a JSON array; an element of the wrong shape is skipped.
pub fn parse_releases(body: &[u8], repo: &str) -> Result<Vec<Release>, ParseError> {
    let items: Vec<serde_json::Value> = serde_json::from_slice(body)
        .map_err(|error| ParseError(format!("not a JSON list of releases: {error}")))?;
    let mut releases: Vec<Release> = items
        .into_iter()
        .filter_map(|item| serde_json::from_value::<Raw>(item).ok())
        .filter(|raw| !raw.draft && !raw.prerelease)
        .filter_map(|raw| release_for_tag(repo, &raw.tag_name))
        .collect();
    releases.sort_by(|a, b| b.version.cmp_precedence(&a.version));
    Ok(releases)
}

/// The newest counting release of a response body, if there is one.
pub fn latest(body: &[u8], repo: &str) -> Result<Option<Release>, ParseError> {
    Ok(parse_releases(body, repo)?.into_iter().next())
}

#[cfg(test)]
mod tests {
    use super::*;

    const REPO: &str = "charlie-niekirk/gpui-chat";
    const AVAILABLE: &str = include_str!("../../../fixtures/updates/releases.json");
    const EMPTY: &str = include_str!("../../../fixtures/updates/releases-empty.json");
    const MALFORMED: &str = include_str!("../../../fixtures/updates/releases-malformed.json");
    const NOT_FOUND: &str = include_str!("../../../fixtures/updates/not-found.json");
    const NON_SEMVER: &str = include_str!("../../../fixtures/updates/releases-non-semver.json");
    const BAD_ELEMENT: &str = include_str!("../../../fixtures/updates/releases-bad-element.json");

    fn v(text: &str) -> Version {
        Version::parse(text).unwrap()
    }

    #[test]
    fn drafts_prereleases_and_non_semver_tags_are_skipped_and_the_rest_sorted() {
        let releases = parse_releases(AVAILABLE.as_bytes(), REPO).unwrap();
        let tags: Vec<_> = releases.iter().map(|r| r.tag.as_str()).collect();
        // The fixture also holds a draft v0.9.0, a prerelease v0.4.0-rc.1, a prerelease v0.3.1
        // flagged as such, a "nightly" tag, and the stable v0.3.0 listed before v0.10.0.
        assert_eq!(tags, ["v0.10.0", "v0.3.0", "v0.2.0", "0.1.0"]);
        assert_eq!(releases[0].version, v("0.10.0"));
    }

    #[test]
    fn the_url_is_built_from_the_repository_and_the_tag() {
        let release = latest(AVAILABLE.as_bytes(), REPO).unwrap().unwrap();
        assert_eq!(
            release.url,
            "https://github.com/charlie-niekirk/gpui-chat/releases/tag/v0.10.0"
        );
    }

    #[test]
    fn an_empty_list_has_no_release() {
        assert_eq!(latest(EMPTY.as_bytes(), REPO), Ok(None));
    }

    #[test]
    fn malformed_json_and_a_non_list_are_errors() {
        assert!(latest(MALFORMED.as_bytes(), REPO).is_err());
        assert!(latest(NOT_FOUND.as_bytes(), REPO).is_err());
        assert!(latest(b"", REPO).is_err());
        assert!(latest(b"null", REPO).is_err());
    }

    #[test]
    fn tags_that_are_not_semver_give_no_release() {
        assert_eq!(latest(NON_SEMVER.as_bytes(), REPO), Ok(None));
    }

    #[test]
    fn an_element_of_the_wrong_shape_is_skipped() {
        let releases = parse_releases(BAD_ELEMENT.as_bytes(), REPO).unwrap();
        let tags: Vec<_> = releases.iter().map(|r| r.tag.as_str()).collect();
        assert_eq!(tags, ["v0.2.0"]);
    }

    #[test]
    fn tag_forms() {
        assert_eq!(stable_version("v1.2.3"), Some(v("1.2.3")));
        assert_eq!(stable_version("1.2.3"), Some(v("1.2.3")));
        assert_eq!(stable_version("v1.2.3+build.5"), Some(v("1.2.3+build.5")));
        for bad in [
            "",
            "v",
            "v1",
            "v1.2",
            "release-1",
            "V1.2.3",
            "vv1.2.3",
            "v1.2.3-rc.1",
        ] {
            assert_eq!(stable_version(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn build_metadata_does_not_change_precedence() {
        let a = v("1.2.3+a");
        let b = v("1.2.3+b");
        assert_eq!(a.cmp_precedence(&b), std::cmp::Ordering::Equal);
    }
}
