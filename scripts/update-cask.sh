#!/usr/bin/env bash
# Fills the `version` and `sha256` of the Homebrew cask (PR 7.2, docs/RELEASING.md "Homebrew").
#
#   scripts/update-cask.sh 0.1.0                       # sha256 from the release's .dmg.sha256 asset (needs gh)
#   scripts/update-cask.sh 0.1.0 --dmg dist/gpui-chat-0.1.0-macos-universal.dmg   # hash a local DMG
#   scripts/update-cask.sh 0.1.0 --sha256 <64 hex digits>
#   scripts/update-cask.sh 0.1.0 --cask ../homebrew-tap/Casks/gpui-chat.rb       # write the tap's copy
#
# Without --cask the template packaging/homebrew/gpui-chat.rb is edited in place (do not commit a
# filled-in template: the tap holds the real one). The cask never talks to anything but the
# release the version names; the script creates no tag, release or repository.
set -euo pipefail

cd "$(dirname "$0")/.."
template="packaging/homebrew/gpui-chat.rb"
repo="charlie-niekirk/gpui-chat"

usage() { sed -n '2,11p' "$0" | sed 's/^# \{0,1\}//'; exit "${1:-2}"; }

version=""
sha=""
dmg=""
cask="$template"
while [ $# -gt 0 ]; do
  case "$1" in
    --sha256) sha="$2"; shift 2 ;;
    --dmg) dmg="$2"; shift 2 ;;
    --cask) cask="$2"; shift 2 ;;
    -h|--help) usage 0 ;;
    -*) echo "unknown option: $1" >&2; usage ;;
    *) if [ -n "$version" ]; then usage; fi; version="${1#v}"; shift ;;
  esac
done
[ -n "$version" ] || usage
if ! printf '%s' "$version" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$'; then
  echo "error: '$version' is not a release version like 0.1.0" >&2
  exit 2
fi

if [ -z "$sha" ]; then
  if [ -n "$dmg" ]; then
    sha="$(shasum -a 256 "$dmg" | cut -d' ' -f1)"
  else
    asset="gpui-chat-$version-macos-universal.dmg.sha256"
    echo "Reading $asset from the v$version release of $repo…"
    sha="$(gh release download "v$version" --repo "$repo" --pattern "$asset" --output - | cut -d' ' -f1)"
  fi
fi
if ! printf '%s' "$sha" | grep -Eq '^[0-9a-f]{64}$'; then
  echo "error: '$sha' is not a SHA-256 (64 lowercase hex digits)" >&2
  exit 1
fi

[ -f "$cask" ] || { echo "error: no cask at $cask" >&2; exit 1; }
tmp="$(mktemp)"
trap 'rm -f "$tmp"' EXIT
awk -v version="$version" -v sha="$sha" '
  /^  version "/ { print "  version \"" version "\""; next }
  /^  sha256 "/  { print "  sha256 \"" sha "\""; next }
  { print }
' "$cask" > "$tmp"
cat "$tmp" > "$cask"
echo "Updated $cask: version $version, sha256 $sha"
if command -v ruby >/dev/null 2>&1; then ruby -c "$cask" >/dev/null && echo "ruby -c: syntax OK"; fi
