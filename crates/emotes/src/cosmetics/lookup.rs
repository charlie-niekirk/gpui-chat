//! Looking up what chatters wear on 7TV: batched GraphQL requests.
//!
//! The chat asks for a chatter ([`StyleLookup::request`], never blocking); a background task
//! collects the ids that arrive within a short window (up to [`BATCH`]) and asks 7TV, in two
//! steps because its query complexity limit rules out fetching every definition inline:
//!
//! 1. `userByConnection(platform: TWITCH, id: ...)` per chatter (aliased, one request) → the
//!    user's style (`paint_id`, `badge_id`),
//! 2. `cosmetics(list: [...])` for the paint and badge definitions not known yet.
//!
//! Both are plain `GET`s of `https://7tv.io/v3/gql?query=...`, so this goes through the same
//! [`HttpClient`] as everything else. A user without a 7TV account answers with the all-zero
//! "null object" id and gets an empty style (and is not asked again for [`super::STYLE_TTL`]).

use std::collections::HashSet;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};
use tokio::runtime::Handle;
use tokio::sync::mpsc;

use super::{Cosmetics, Paint, UserStyle, parse_paint, parse_seventv_badge};
use crate::error::EmoteError;
use crate::http::{HttpClient, get_ok};
use gpui_chat_core::Badge;

pub const GQL_URL: &str = "https://7tv.io/v3/gql";
/// Users per request: 7TV allows a query complexity of about 400 and a user costs 5.
pub const BATCH: usize = 50;
/// How long the task waits for more ids before sending what it has.
const WINDOW: Duration = Duration::from_millis(60);
/// Pause after a failed request, so a down service is not hammered.
const FAILURE_PAUSE: Duration = Duration::from_secs(3);
/// Chatters waiting for a lookup (more are dropped; they are asked again on their next message).
const QUEUE: usize = 4096;

/// The `null object` id 7TV answers with for an unknown user.
const NULL_ID: &str = "00000000000000000000000000";

fn url(query: &str, variables: Option<&Value>) -> String {
    let mut params = vec![("query", query.to_owned())];
    if let Some(variables) = variables {
        params.push(("variables", variables.to_string()));
    }
    match reqwest::Url::parse_with_params(GQL_URL, params) {
        Ok(url) => url.into(),
        // Both parts are constants and parameters; this cannot fail.
        Err(_) => GQL_URL.to_owned(),
    }
}

/// The request for the styles of `ids` (Twitch user ids; callers pass digits only).
pub fn users_url(ids: &[Arc<str>]) -> String {
    let mut query = String::from("query{");
    for (i, id) in ids.iter().enumerate() {
        use std::fmt::Write as _;
        let _ = write!(
            query,
            "u{i}:userByConnection(platform:TWITCH,id:\"{id}\"){{id style{{paint_id badge_id}}}}"
        );
    }
    query.push('}');
    url(&query, None)
}

/// The request for the definitions of the cosmetics `ids`.
pub fn cosmetics_url(ids: &[Arc<str>]) -> String {
    let query = "query($l:[ObjectID!]){cosmetics(list:$l){\
        paints{id name function color angle shape repeat image_url stops{at color} \
        shadows{x_offset y_offset radius color}} \
        badges{id name tooltip tag host{url files{name width height format}}}}}";
    let ids: Vec<&str> = ids.iter().map(|id| &**id).collect();
    url(query, Some(&json!({ "l": ids })))
}

#[derive(Deserialize)]
struct RawStyle {
    paint_id: Option<String>,
    badge_id: Option<String>,
}

#[derive(Deserialize)]
struct RawUser {
    id: String,
    style: Option<RawStyle>,
}

/// The styles in an answer to [`users_url`], in the order of `ids`. A user missing from the
/// answer, unknown to 7TV or without a style yields an empty [`UserStyle`].
pub fn parse_users(
    body: &[u8],
    ids: &[Arc<str>],
) -> Result<Vec<(Arc<str>, UserStyle)>, EmoteError> {
    let doc: Value = serde_json::from_slice(body).map_err(EmoteError::parse)?;
    let Some(data) = doc.get("data").and_then(Value::as_object) else {
        return Err(EmoteError::Parse(
            doc.pointer("/errors/0/message")
                .and_then(Value::as_str)
                .unwrap_or("no data")
                .to_owned(),
        ));
    };
    Ok(ids
        .iter()
        .enumerate()
        .map(|(i, id)| {
            let style = data
                .get(&format!("u{i}"))
                .and_then(|value| serde_json::from_value::<RawUser>(value.clone()).ok())
                .filter(|user| user.id != NULL_ID)
                .and_then(|user| user.style)
                .map(|style| UserStyle {
                    paint_id: style.paint_id.map(Into::into),
                    badge_id: style.badge_id.map(Into::into),
                })
                .unwrap_or_default();
            (id.clone(), style)
        })
        .collect())
}

/// The definitions in an answer to [`cosmetics_url`].
pub fn parse_cosmetics(body: &[u8]) -> Result<(Vec<Paint>, Vec<Badge>), EmoteError> {
    let doc: Value = serde_json::from_slice(body).map_err(EmoteError::parse)?;
    let Some(cosmetics) = doc.pointer("/data/cosmetics") else {
        return Err(EmoteError::Parse(
            doc.pointer("/errors/0/message")
                .and_then(Value::as_str)
                .unwrap_or("no data")
                .to_owned(),
        ));
    };
    let list = |key: &str| {
        cosmetics
            .get(key)
            .and_then(Value::as_array)
            .map_or(&[][..], Vec::as_slice)
    };
    Ok((
        list("paints").iter().filter_map(parse_paint).collect(),
        list("badges")
            .iter()
            .filter_map(parse_seventv_badge)
            .collect(),
    ))
}

/// Looks up the styles of the chatters it is asked about and stores them in a [`Cosmetics`].
#[derive(Clone)]
pub struct StyleLookup {
    queue: mpsc::Sender<Arc<str>>,
    /// Ids queued or in flight, so one chatter is asked about once at a time.
    pending: Arc<Mutex<HashSet<Arc<str>>>>,
    cosmetics: Arc<Cosmetics>,
}

impl StyleLookup {
    /// Starts the lookup task on `handle`. `changed` is called after every batch that stored
    /// something (the UI repaints).
    pub fn start(
        handle: &Handle,
        http: Arc<dyn HttpClient>,
        cosmetics: Arc<Cosmetics>,
        changed: Arc<dyn Fn() + Send + Sync>,
    ) -> Self {
        let (queue, receiver) = mpsc::channel(QUEUE);
        let pending = Arc::new(Mutex::new(HashSet::new()));
        drop(handle.spawn(run(
            receiver,
            http,
            cosmetics.clone(),
            pending.clone(),
            changed,
        )));
        Self {
            queue,
            pending,
            cosmetics,
        }
    }

    /// Asks about `twitch_id` unless it is known, in flight or not a Twitch user id. Never blocks.
    pub fn request(&self, twitch_id: &str) {
        if twitch_id.is_empty() || !twitch_id.bytes().all(|b| b.is_ascii_digit()) {
            return;
        }
        if !self
            .cosmetics
            .needs_lookup(twitch_id, std::time::Instant::now())
        {
            return;
        }
        let id: Arc<str> = Arc::from(twitch_id);
        if !self
            .pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(id.clone())
        {
            return;
        }
        if self.queue.try_send(id.clone()).is_err() {
            self.pending
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&id);
        }
    }
}

async fn run(
    mut receiver: mpsc::Receiver<Arc<str>>,
    http: Arc<dyn HttpClient>,
    cosmetics: Arc<Cosmetics>,
    pending: Arc<Mutex<HashSet<Arc<str>>>>,
    changed: Arc<dyn Fn() + Send + Sync>,
) {
    while let Some(first) = receiver.recv().await {
        let mut batch = vec![first];
        let window = tokio::time::sleep(WINDOW);
        tokio::pin!(window);
        while batch.len() < BATCH {
            tokio::select! {
                () = &mut window => break,
                next = receiver.recv() => match next {
                    Some(id) => batch.push(id),
                    None => break,
                },
            }
        }
        let result = fetch(&*http, &cosmetics, &batch).await;
        {
            let mut pending = pending.lock().unwrap_or_else(PoisonError::into_inner);
            for id in &batch {
                pending.remove(id);
            }
        }
        match result {
            Ok(()) => changed(),
            Err(error) => {
                tracing::debug!(%error, users = batch.len(), "7TV style lookup failed");
                tokio::time::sleep(FAILURE_PAUSE).await;
            }
        }
    }
}

/// One batch: styles, then the definitions that are missing. The styles are stored only once the
/// definitions arrived, so a name never waits for a paint that is not there.
async fn fetch(
    http: &dyn HttpClient,
    cosmetics: &Cosmetics,
    ids: &[Arc<str>],
) -> Result<(), EmoteError> {
    let styles = parse_users(&get_ok(http, &users_url(ids)).await?, ids)?;
    let mut wanted: Vec<Arc<str>> = Vec::new();
    for (_, style) in &styles {
        for id in [&style.paint_id, &style.badge_id].into_iter().flatten() {
            if !cosmetics.knows_cosmetic(id) && !wanted.contains(id) {
                wanted.push(id.clone());
            }
        }
    }
    // A stable order: the same batch asks the same question.
    wanted.sort();
    if !wanted.is_empty() {
        let (paints, badges) = parse_cosmetics(&get_ok(http, &cosmetics_url(&wanted)).await?)?;
        for paint in paints {
            cosmetics.store_paint(paint);
        }
        for badge in badges {
            cosmetics.store_seventv_badge(badge);
        }
    }
    let now = std::time::Instant::now();
    let (mut paints, mut badges) = (0, 0);
    for (id, style) in styles {
        paints += usize::from(style.paint_id.is_some());
        badges += usize::from(style.badge_id.is_some());
        cosmetics.set_style(&id, style, now);
    }
    tracing::debug!(users = ids.len(), paints, badges, "7TV styles looked up");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::badges::BadgeRegistry;
    use crate::cosmetics::CosmeticOptions;
    use crate::testutil::{FakeHttp, fixture};

    fn ids(v: &[&str]) -> Vec<Arc<str>> {
        v.iter().map(|s| Arc::from(*s)).collect()
    }

    #[test]
    fn the_requests_are_urls_that_carry_the_query() {
        let url = users_url(&ids(&["1", "22"]));
        assert!(
            url.starts_with("https://7tv.io/v3/gql?query=query%7Bu0%3AuserByConnection"),
            "{url}"
        );
        let parsed = reqwest::Url::parse(&url).unwrap();
        let query = parsed
            .query_pairs()
            .find(|(k, _)| k == "query")
            .unwrap()
            .1
            .into_owned();
        assert!(
            query.contains(r#"u1:userByConnection(platform:TWITCH,id:"22")"#),
            "{query}"
        );
        let url = cosmetics_url(&ids(&["A", "B"]));
        let parsed = reqwest::Url::parse(&url).unwrap();
        let vars = parsed
            .query_pairs()
            .find(|(k, _)| k == "variables")
            .unwrap()
            .1
            .into_owned();
        assert_eq!(vars, r#"{"l":["A","B"]}"#);
    }

    #[test]
    fn users_without_an_account_or_style_get_an_empty_style() {
        let doc = fixture("7tv-gql-users.json");
        let doc: Value = serde_json::from_slice(&doc).unwrap();
        let wanted: Vec<Arc<str>> = doc["ids"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| Arc::from(v.as_str().unwrap()))
            .collect();
        let body = serde_json::to_vec(&doc["response"]).unwrap();
        let styles = parse_users(&body, &wanted).unwrap();
        assert_eq!(styles.len(), wanted.len());
        // The first is a user with a paint and a badge; the third is id "1": no account.
        assert!(
            styles[0].1.paint_id.is_some() && styles[0].1.badge_id.is_some(),
            "{:?}",
            styles[0]
        );
        assert_eq!(styles[2].1, UserStyle::default());
        // An answer with errors and no data is a parse error carrying the message.
        let err = parse_users(
            br#"{"errors":[{"message":"Query is too complex."}]}"#,
            &wanted,
        )
        .unwrap_err();
        assert_eq!(err, EmoteError::Parse("Query is too complex.".into()));
    }

    #[test]
    fn definitions_of_paints_and_badges_parse() {
        let doc: Value = serde_json::from_slice(&fixture("7tv-gql-cosmetics.json")).unwrap();
        let (paints, badges) =
            parse_cosmetics(&serde_json::to_vec(&doc["response"]).unwrap()).unwrap();
        assert!(!paints.is_empty() && !badges.is_empty());
        let badge = &badges[0];
        assert_eq!(badge.id.set.as_ref(), "seventv");
        assert!(badge.title.starts_with("7TV: "), "{}", badge.title);
        assert!(
            badge
                .urls
                .get(gpui_chat_core::ImageScale::X1)
                .unwrap()
                .starts_with("https://cdn.7tv.app/badge/")
        );
    }

    fn lookup_fixture(cosmetics: &Cosmetics) -> FakeHttp {
        let users: Value = serde_json::from_slice(&fixture("7tv-gql-users.json")).unwrap();
        let wanted: Vec<Arc<str>> = users["ids"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| Arc::from(v.as_str().unwrap()))
            .collect();
        let cos: Value = serde_json::from_slice(&fixture("7tv-gql-cosmetics.json")).unwrap();
        let _ = cosmetics;
        FakeHttp::new()
            .body(
                &users_url(&wanted),
                200,
                serde_json::to_vec(&users["response"]).unwrap(),
            )
            .body(
                &cosmetics_url(
                    &cos["ids"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|v| Arc::from(v.as_str().unwrap()))
                        .collect::<Vec<Arc<str>>>(),
                ),
                200,
                serde_json::to_vec(&cos["response"]).unwrap(),
            )
    }

    #[tokio::test]
    async fn a_batch_stores_styles_after_the_definitions_arrived() {
        let registry = Arc::new(BadgeRegistry::new());
        let cosmetics = Arc::new(Cosmetics::new(registry.clone(), CosmeticOptions::default()));
        let http = lookup_fixture(&cosmetics);
        let users: Value = serde_json::from_slice(&fixture("7tv-gql-users.json")).unwrap();
        let wanted: Vec<Arc<str>> = users["ids"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| Arc::from(v.as_str().unwrap()))
            .collect();
        fetch(&http, &cosmetics, &wanted).await.unwrap();
        // The user with a paint has it, ready to draw, and their badge is an id to show.
        let first = &wanted[0];
        let paint = cosmetics.paint(first).expect("a paint");
        assert!(paint.is_drawable());
        let badge_ids = cosmetics.badge_ids(first);
        assert_eq!(badge_ids.len(), 1, "{badge_ids:?}");
        let channel = gpui_chat_core::ChannelId::new("c");
        assert!(registry.lookup(&channel, &badge_ids[0]).is_some());
        // Nobody is asked about again soon, accounts or not.
        assert!(!cosmetics.needs_lookup(first, std::time::Instant::now()));
        assert!(!cosmetics.needs_lookup("1", std::time::Instant::now()));
        // Two requests: styles, then the definitions.
        assert_eq!(http.requested().len(), 2);
        // A second batch of known cosmetics needs no definitions request.
        fetch(&http, &cosmetics, &wanted).await.unwrap();
        assert_eq!(http.requested().len(), 3);
    }

    #[tokio::test]
    async fn a_failing_batch_stores_nothing() {
        let cosmetics = Arc::new(Cosmetics::new(
            Arc::new(BadgeRegistry::new()),
            CosmeticOptions::default(),
        ));
        let http = FakeHttp::new();
        let wanted = ids(&["98712408"]);
        assert!(fetch(&http, &cosmetics, &wanted).await.is_err());
        assert!(cosmetics.needs_lookup("98712408", std::time::Instant::now()));
    }

    #[tokio::test]
    async fn requests_are_batched_deduplicated_and_validated() {
        let registry = Arc::new(BadgeRegistry::new());
        let cosmetics = Arc::new(Cosmetics::new(registry, CosmeticOptions::default()));
        let users: Value = serde_json::from_slice(&fixture("7tv-gql-users.json")).unwrap();
        let wanted: Vec<Arc<str>> = users["ids"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| Arc::from(v.as_str().unwrap()))
            .collect();
        let http = Arc::new(lookup_fixture(&cosmetics));
        let (done, mut done_rx) = mpsc::unbounded_channel();
        let lookup = StyleLookup::start(
            &Handle::current(),
            http.clone(),
            cosmetics.clone(),
            Arc::new(move || {
                let _ = done.send(());
            }),
        );
        for id in &wanted {
            lookup.request(id);
            lookup.request(id);
        }
        lookup.request("not-a-number");
        lookup.request("");
        tokio::time::timeout(Duration::from_secs(5), done_rx.recv())
            .await
            .unwrap()
            .unwrap();
        // One batch: both requests, once.
        assert_eq!(http.requested().len(), 2, "{:?}", http.requested());
        assert!(cosmetics.paint(&wanted[0]).is_some());
    }
}
