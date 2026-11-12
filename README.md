# gpui-chat

A native, fast Twitch chat client for desktop — in the spirit of
[Chatterino](https://chatterino.com) — written in Rust on
[GPUI](https://www.gpui.rs) via [GPUI Kit](https://crates.io/crates/gpui-kit).

> **Status: early development.** Right now the app opens an empty window. See
> [`PLAN.md`](PLAN.md) for the roadmap.

Planned: anonymous read-only chat, Twitch/BTTV/FFZ/7TV emotes and badges, Twitch
login and sending, a collapsible channel sidebar with split panes, a per-chat
button that launches [streamlink](https://streamlink.github.io), and a monospace
default font (JetBrains Mono) that you can change. macOS first; Linux and Windows
later.

## Build and run

Requires a Rust toolchain (version pinned in [`rust-toolchain.toml`](rust-toolchain.toml)) on macOS.

```bash
cargo run -p gpui-chat
```

Developer flags (offline, no network or login needed):

```bash
cargo run -p gpui-chat --release -- --firehose 1000 --hud   # synthetic chat, 1 000 msgs/s, HUD open
cargo run -p gpui-chat -- --help                            # all flags
```

| Flag | Meaning |
|---|---|
| `--firehose <msgs/sec>` | Feed the app deterministic synthetic chat at a steady rate |
| `--seed <n>` | Seed of the synthetic chat (default is fixed, so runs repeat) |
| `--firehose-channels <n>` | Spread the synthetic chat over `n` channels (default 1) |
| `--replay <file>` | Replay a recorded raw IRC log (e.g. `fixtures/irc/privmsg.irc`) through the real parser |
| `--replay-speed <x>` | `1` follows the log's `tmi-sent-ts` spacing (100 msgs/s if it has none), `2` is twice as fast, `0` is as fast as possible (default `1`) |
| `--hud` | Start with the debug HUD visible |

The debug HUD (frame time and ingest apply time against the 8.3 ms budget, messages/sec, batch size, store
sizes, dropped events, replay parse errors) toggles with ⌘⇧D (Ctrl+Shift+D elsewhere) or View > Toggle Debug HUD.

Tests (everything except the GPUI app also runs on Linux in CI):

```bash
cargo test --workspace
```

## Project layout

See [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md). Contributor and AI-agent
conventions are in [`CONTRIBUTING.md`](CONTRIBUTING.md) and [`CLAUDE.md`](CLAUDE.md).

## License

[MIT](LICENSE)
