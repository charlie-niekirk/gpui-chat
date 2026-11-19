# Releasing (macOS)

A release is a `v*` tag. Pushing one runs `.github/workflows/release.yml`, which builds a universal
(arm64 + x86_64) `.app`, wraps it in a DMG and attaches it to a **draft** GitHub Release. Nothing is
published until the owner presses *Publish release* by hand.

## Cutting a release

1. In a PR, set `version` under `[workspace.package]` in `Cargo.toml` (and let `cargo` update
   `Cargo.lock`). The app's About box, `CFBundleShortVersionString` and the DMG name all come from it.
2. After the PR is merged, tag the merge commit and push the tag:

   ```bash
   git tag v0.1.0 && git push origin v0.1.0
   ```

   The workflow refuses a tag that does not match the Cargo version.
3. When the run is green, open the draft release, check the notes, download the DMG and open it on a
   Mac that is not the build machine, then publish.
4. After publishing, [update the Homebrew cask](#homebrew) in the tap.

Re-running the workflow for a tag whose release is still a draft replaces the draft's assets. Once a
release is published, a re-run fails instead: published assets are never replaced, cut a new version.

### Dry run

*Actions → Release → Run workflow* (or any pull request that touches the packaging: the workflow,
`scripts/bundle-macos.sh`, `assets/icon/`, `rust-toolchain.toml`) runs the same build and keeps the DMG
as the `macos-dmg` workflow artifact for 14 days. It never creates a release. **Dry runs never sign:**
only a pushed `v*` tag uses the signing secrets, so a workflow dispatched from any branch produces an
ad-hoc signed DMG.

### Building locally

```bash
scripts/bundle-macos.sh                 # universal release build -> dist/gpui-chat.app and dist/*.dmg
scripts/bundle-macos.sh --arch native   # only this Mac's architecture, about half the compile time
open dist/gpui-chat.app
```

A universal build needs a **rustup-managed toolchain**. `rust-toolchain.toml` lists both macOS targets,
and the script runs `rustup target add` for each before building. Homebrew's Rust has no rustup: it
ignores `rust-toolchain.toml` and cannot build the other architecture, so the script stops with an
error for a universal build there; `--arch native` still works. Install rustup from https://rustup.rs.
The icon is built from `assets/icon/icon-1024.png`.

## What the update check sees

Running apps look for updates with `GET /repos/<owner>/<repo>/releases` (the repository comes from
`repository` in `Cargo.toml`) and compare `CARGO_PKG_VERSION` with the newest release that is
**published** (not a draft), **not marked a prerelease**, and whose tag is semver (`v0.2.0`; a tag with a
semver pre-release part such as `v0.2.0-rc.1`, or any tag that is not semver, is skipped). So:

- A draft is invisible to every app until you press *Publish release*, and publishing is what makes users see
  the notice. Do it only when the DMG is final.
- Tag release candidates `vX.Y.Z-rc.N` and tick *Set as a pre-release*; they never produce a notice.
- The notice links to the release page (`https://github.com/<owner>/<repo>/releases/tag/<tag>`); nothing is
  downloaded by the app. Apps ask at most once a day, so a notice appears within a day of publishing.
- To try the notice without a release, serve `fixtures/updates/releases.json` (or any list of releases) from a
  local server at `$GPUI_CHAT_UPDATE_API/repos/<owner>/<repo>/releases`, run the app against it and choose
  *Check for Updates…*. The running version must be lower than the newest tag in the file.

## Homebrew

`packaging/homebrew/gpui-chat.rb` is the cask for the universal DMG named by `scripts/bundle-macos.sh`
(`gpui-chat-<version>-macos-universal.dmg`). The copy in this repository is a **template** with a placeholder
`version` and `sha256`; the filled-in cask lives in a tap you own. Nothing in this repository creates the tap,
pushes to it or touches `homebrew-cask`.

### One-time tap setup

A tap is a GitHub repository named `homebrew-<name>`. For the owner's account:

```bash
gh repo create charlie-niekirk/homebrew-tap --public --description "Homebrew tap for gpui-chat"
git clone https://github.com/charlie-niekirk/homebrew-tap ../homebrew-tap
mkdir -p ../homebrew-tap/Casks
cp packaging/homebrew/gpui-chat.rb ../homebrew-tap/Casks/gpui-chat.rb
```

Users then install with `brew install --cask charlie-niekirk/tap/gpui-chat` (Homebrew adds the
`charlie-niekirk/tap` tap by itself), or `brew tap charlie-niekirk/tap` first.

### For each release

After *Publish release* (the DMG must be downloadable), in this repository:

```bash
scripts/update-cask.sh 0.1.0 --cask ../homebrew-tap/Casks/gpui-chat.rb
cd ../homebrew-tap && git add Casks/gpui-chat.rb && git commit -m "gpui-chat 0.1.0" && git push
```

The script reads `gpui-chat-0.1.0-macos-universal.dmg.sha256` from the release with `gh` (or use `--dmg <file>` to
hash a local DMG, or `--sha256 <digest>`), writes the `version` and `sha256` lines and checks the Ruby syntax. Then
try it on a Mac:

```bash
brew install --cask charlie-niekirk/tap/gpui-chat
brew audit --cask --online charlie-niekirk/tap/gpui-chat
```

The cask's `livecheck` follows the latest GitHub release, so `brew livecheck` reports when the tap is behind. The
cask's `caveats` carry the Gatekeeper advice from "Installing an unsigned release" below; drop them once releases
are Developer ID signed and notarized.

## Signing and notarization

Both are optional. Only a pushed `v*` tag signs, and only with the secrets below. Without them (or on a
dispatch or pull request run) the workflow logs a notice (`Signing skipped …`), and the DMG is
signed ad hoc (`codesign -s -`), which Apple Silicon requires for any binary to run but which carries no
identity: Gatekeeper treats the app as from an unidentified developer.

### The `release` environment

Store the five secrets as secrets of a GitHub **Environment** named `release` (*Settings →
Environments → New environment*), not as repository secrets, and under *Deployment branches and tags*
choose *Selected branches and tags* and add the tag rule `v*`. A tag push runs the `bundle` job in that
environment; every other run uses a `dry-run` environment that holds no secrets, so a workflow edited
on a branch and dispatched cannot read them. Optionally add yourself as a required reviewer to approve
each release run.

The certificate and the API key are only on the runner for the packaging step: both triples are
compiled before they are written, and the keychain and key file are deleted before anything else runs.

| Secret | Value | Needed for |
|---|---|---|
| `MACOS_CERTIFICATE` | base64 of the *Developer ID Application* certificate exported as `.p12` | signing |
| `MACOS_CERTIFICATE_PASSWORD` | the password chosen when exporting the `.p12` | signing |
| `APPLE_API_KEY` | base64 of the App Store Connect API key (`AuthKey_XXXXXXXXXX.p8`) | notarization |
| `APPLE_API_KEY_ID` | the key's ID (10 characters) | notarization |
| `APPLE_API_ISSUER_ID` | the issuer ID (a UUID) | notarization |

Signing needs the first two; notarization needs all five (add them with `gh secret set NAME --env release`). With only the certificate the DMG is Developer
ID signed but not notarized, and the log says that Gatekeeper will still warn. The signing identity is
found in the imported certificate, so its name is not a secret to maintain.

### Creating them

You need a paid Apple Developer Program membership.

1. **Certificate.** At developer.apple.com → Certificates, create a *Developer ID Application*
   certificate (upload a CSR made with Keychain Access → Certificate Assistant), download it and
   double-click to install. In Keychain Access find it under *My Certificates*, right-click →
   *Export…* as `.p12` with a password. Then:

   ```bash
   base64 -i DeveloperID.p12 | gh secret set MACOS_CERTIFICATE --env release
   gh secret set MACOS_CERTIFICATE_PASSWORD --env release
   ```
2. **API key.** At appstoreconnect.apple.com → Users and Access → Integrations → Team Keys, generate a
   key with the *Developer* role and download the `.p8` (Apple offers it once). Note the Key ID and the
   Issuer ID shown on that page:

   ```bash
   base64 -i AuthKey_XXXXXXXXXX.p8 | gh secret set APPLE_API_KEY --env release
   gh secret set APPLE_API_KEY_ID --env release
   gh secret set APPLE_API_ISSUER_ID --env release
   ```
3. Delete the local `.p12` and `.p8` once they are in a password manager.

The same signing and notarization can be run locally with
`scripts/bundle-macos.sh --sign "Developer ID Application: Name (TEAMID)" --notarize`, with
`APPLE_API_KEY_PATH` (the `.p8` file), `APPLE_API_KEY_ID` and `APPLE_API_ISSUER_ID` in the environment.

## Installing an unsigned release

An ad-hoc signed app is quarantined by Gatekeeper after a browser download. Either right-click the app
in *Applications* → *Open* → *Open* once (on macOS 15 and later: try to open it, then *System Settings →
Privacy & Security → Open Anyway*), or remove the quarantine flag:

```bash
xattr -dr com.apple.quarantine /Applications/gpui-chat.app
```

The Twitch login lives in the system keychain, whose access list for an item is tied to the app's code
signature. An ad-hoc signature changes with every build, so expect macOS to ask again for keychain access
after an update until releases are signed with a Developer ID. This has not been observed yet.
