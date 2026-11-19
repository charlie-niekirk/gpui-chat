# Homebrew cask for gpui-chat (PR 7.2, docs/RELEASING.md "Homebrew").
#
# This file is the template kept in the repository. `version` and `sha256` are placeholders until a
# release exists: `scripts/update-cask.sh <version>` fills them in from the published release, and
# the filled-in copy lives in the owner's tap (`homebrew-tap/Casks/gpui-chat.rb`), not here.
cask "gpui-chat" do
  version "0.0.0"
  sha256 "0000000000000000000000000000000000000000000000000000000000000000"

  # The universal (arm64 + x86_64) DMG named by scripts/bundle-macos.sh:
  # <APP_NAME>-<version>-macos-universal.dmg
  url "https://github.com/charlie-niekirk/gpui-chat/releases/download/v#{version}/gpui-chat-#{version}-macos-universal.dmg"
  name "gpui-chat"
  desc "Native Twitch chat client"
  homepage "https://github.com/charlie-niekirk/gpui-chat"

  livecheck do
    url :url
    strategy :github_latest
  end

  # The oldest macOS GPUI supports (`min_macos` in scripts/bundle-macos.sh).
  depends_on macos: ">= :monterey"

  app "gpui-chat.app"

  zap trash: [
    "~/.config/gpui-chat",
    "~/Library/Application Support/gpui-chat",
    "~/Library/Caches/gpui-chat",
  ]

  caveats <<~EOS
    Releases are signed ad hoc until a Developer ID certificate is set up, so macOS may say the
    app is from an unidentified developer. Right-click the app and choose Open once, or run:

      xattr -dr com.apple.quarantine #{appdir}/gpui-chat.app
  EOS
end
