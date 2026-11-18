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

### Dry run

*Actions → Release → Run workflow* (or any pull request that touches the packaging: the workflow,
`scripts/bundle-macos.sh`, `assets/icon/`, `rust-toolchain.toml`) runs the same build and keeps the DMG
as the `macos-dmg` workflow artifact for 14 days. It never creates a release, and pull requests never
use the signing secrets.

### Building locally

```bash
scripts/bundle-macos.sh                 # universal release build -> dist/gpui-chat.app and dist/*.dmg
scripts/bundle-macos.sh --arch native   # only this Mac's architecture, about half the compile time
open dist/gpui-chat.app
```

`rust-toolchain.toml` lists both macOS targets, so `rustup toolchain install` fetches them. A
toolchain not managed by rustup (for example Homebrew's Rust) needs the `x86_64-apple-darwin` standard
library added by hand, or use `--arch native`. The icon is built from `assets/icon/icon-1024.png`.

## Signing and notarization

Both are optional. Without secrets the workflow logs a notice (`Signing skipped …`), and the DMG is
signed ad hoc (`codesign -s -`), which Apple Silicon requires for any binary to run but which carries no
identity: Gatekeeper treats the app as from an unidentified developer.

| Secret | Value | Needed for |
|---|---|---|
| `MACOS_CERTIFICATE` | base64 of the *Developer ID Application* certificate exported as `.p12` | signing |
| `MACOS_CERTIFICATE_PASSWORD` | the password chosen when exporting the `.p12` | signing |
| `APPLE_API_KEY` | base64 of the App Store Connect API key (`AuthKey_XXXXXXXXXX.p8`) | notarization |
| `APPLE_API_KEY_ID` | the key's ID (10 characters) | notarization |
| `APPLE_API_ISSUER_ID` | the issuer ID (a UUID) | notarization |

Signing needs the first two; notarization needs all five. With only the certificate the DMG is Developer
ID signed but not notarized, and the log says that Gatekeeper will still warn. The signing identity is
found in the imported certificate, so its name is not a secret to maintain.

### Creating them

You need a paid Apple Developer Program membership.

1. **Certificate.** At developer.apple.com → Certificates, create a *Developer ID Application*
   certificate (upload a CSR made with Keychain Access → Certificate Assistant), download it and
   double-click to install. In Keychain Access find it under *My Certificates*, right-click →
   *Export…* as `.p12` with a password. Then:

   ```bash
   base64 -i DeveloperID.p12 | gh secret set MACOS_CERTIFICATE
   gh secret set MACOS_CERTIFICATE_PASSWORD
   ```
2. **API key.** At appstoreconnect.apple.com → Users and Access → Integrations → Team Keys, generate a
   key with the *Developer* role and download the `.p8` (Apple offers it once). Note the Key ID and the
   Issuer ID shown on that page:

   ```bash
   base64 -i AuthKey_XXXXXXXXXX.p8 | gh secret set APPLE_API_KEY
   gh secret set APPLE_API_KEY_ID
   gh secret set APPLE_API_ISSUER_ID
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
