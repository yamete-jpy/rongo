cask "rongo" do
  version "0.1.9"
  sha256 "47e74cebca5f05ef4006da608e8824d7a1784b6726f3681ed410070bba8d1315"

  url "https://github.com/yamete-jpy/rongo/releases/download/v#{version}/rongo-#{version}-arm64.dmg"
  name "rongo"
  desc "Desktop MongoDB client"
  homepage "https://github.com/yamete-jpy/rongo"

  depends_on arch: :arm64
  depends_on macos: :big_sur

  app "rongo.app"

  zap trash: [
    "~/Library/Application Support/dev.rongo.rongo",
    "~/Library/Application Support/rongo",
  ]

  caveats <<~EOS
    rongo is not notarized by Apple, so macOS blocks its first launch,
    after an update too. Open System Settings > Privacy & Security and
    press Open Anyway, or run:
      xattr -dr com.apple.quarantine /Applications/rongo.app
    To update: brew update && brew upgrade --cask rongo
  EOS
end
