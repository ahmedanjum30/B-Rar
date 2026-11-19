#!/bin/sh
# Prints the Homebrew cask for a release, for the tap (ahmedanjum30/homebrew-tap,
# Casks/b-rar.rb). The release workflow runs it for each new version.
#
#   sh macos/homebrew-cask.sh <version> <zip>      # sha256 from the zip
#   sh macos/homebrew-cask.sh <version> <sha256>
#   URL=file:///path/to.zip sh macos/homebrew-cask.sh ...   # local testing
#
# The app is signed ad hoc, not notarized, so Gatekeeper refuses it while it carries the
# quarantine flag Homebrew sets on downloads; the postflight removes the flag. That's also
# why this is in our own tap: the official homebrew/cask only takes notarized apps.

set -eu

version=$1
if [ -f "$2" ]; then
    sha256=$(shasum -a 256 "$2" | cut -d' ' -f1)
else
    sha256=$2
fi
url=${URL:-'https://github.com/ahmedanjum30/B-Rar/releases/download/v#{version}/B-Rar-#{version}-macos.zip'}

cat <<EOF
cask "b-rar" do
  version "$version"
  sha256 "$sha256"

  url "$url"
  name "B-Rar"
  desc "Fast archive extractor for ZIP, RAR, 7z and tar"
  homepage "https://ahmedanjum30.github.io/B-Rar/"

  livecheck do
    url :url
    strategy :github_latest
  end

  depends_on arch: :arm64
  depends_on macos: ">= :ventura"

  app "B-Rar.app"
  # The command line, as on Linux: brar x archive.rar
  binary "#{appdir}/B-Rar.app/Contents/MacOS/B-Rar", target: "brar"

  postflight do
    # Signed ad hoc, not notarized: without this Gatekeeper won't open it.
    system_command "/usr/bin/xattr",
                   args: ["-dr", "com.apple.quarantine", "#{appdir}/B-Rar.app"]
    # Shows B-Rar's Quick Actions in Finder's right-click menu right away.
    system_command "/System/Library/CoreServices/pbs", args: ["-update"]
  end

  uninstall quit: "com.ahmadanjum.brar"

  zap trash: [
    "~/Library/Saved Application State/com.ahmadanjum.brar.savedState",
  ]
end
EOF
