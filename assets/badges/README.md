# Bundled global badges

`global.json` is a snapshot of Twitch's **global chat badge definitions** (moderator, VIP, broadcaster,
staff, Prime, event badges, ...) so they resolve offline and before any request. It is read at build time
(`include_str!` in `crates/emotes/src/badges/snapshot.rs`) and stored in the badge registry at startup.

## What is in it

Metadata only, one entry per `(set, version)`: the set id, version id, title (tooltip text) and the id of the
image on Twitch's CDN (`https://static-cdn.jtvnw.net/badges/v1/<id>/1|2|3` = 1x/2x/4x). **No images are
bundled**; the app loads them from Twitch's CDN at run time through its image store (cached on disk).

## Where it comes from and licence

Generated from `https://api.ivr.fi/v2/twitch/badges/global`, a public, unauthenticated API that returns the
same items as Twitch's Helix `chat/badges/global` (as a bare list rather than Helix's `{"data": [...]}`).
The facts in the file (identifiers, titles, URLs of Twitch-hosted images) are Twitch's public data; the file
contains none of Twitch's artwork. We found no licence or terms of use published for the IVR API (its
documentation page states none), so this is a best-effort reading, not a grant. If the data has to be sourced
differently, Helix `GET /helix/chat/badges/global` carries the same information (it needs a token, so it
arrives with login in PR 3.1; `ivr::parse` already accepts its response shape). The old anonymous
`badges.twitch.tv` endpoint is not an option: it has been shut down. `badges_snapshot` is the only place that
knows the source.

## Regenerating

```bash
cargo run -p gpui-chat-emotes --example badges_snapshot              # rewrite assets/badges/global.json
cargo run -p gpui-chat-emotes --example badges_snapshot -- --check   # exit 1 if it differs from the API
```

The output is deterministic (sorted), so a regeneration with no upstream change is an empty diff.
Commit the result. Channel badges (subscriber tiers, bits) are never bundled: they are fetched per channel.
