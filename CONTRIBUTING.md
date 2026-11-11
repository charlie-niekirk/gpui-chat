# Contributing

Thanks for helping with gpui-chat! The project is built in small PRs following
[`PLAN.md`](PLAN.md); [`CLAUDE.md`](CLAUDE.md) has the detailed conventions
(also useful for humans).

## Setup

```bash
git clone https://github.com/charlie-niekirk/gpui-chat
cd gpui-chat
cargo run -p gpui-chat
```

The toolchain is pinned in `rust-toolchain.toml`; `rustup` installs it automatically.
`cargo install cargo-deny` is optional for checking licences locally.

## Before you open a PR

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

CI runs the same on macOS, plus tests of the non-GPUI crates on Linux, and a
`cargo deny` check. All must be green.

## Guidelines

- Keep a PR to one planned unit of work. Ideas outside that scope go to
  [`docs/BACKLOG.md`](docs/BACKLOG.md).
- GPUI / GPUI Kit may only be used from `crates/app`. Everything else must stay
  testable without a GUI and on Linux.
- Check GPUI Kit API usage against the pinned version's source or docs.rs, not memory.
- Dependencies are pinned in the root `Cargo.toml` `[workspace.dependencies]`. Only
  permissive licences are accepted (see `deny.toml`).
- Fill in the PR template honestly, including what you did not verify.

## License

By contributing you agree that your contributions are licensed under the [MIT License](LICENSE).
