cask "rongo" do
  version "0.1.8"
  sha256 "6c0cc97c75ca225a08ec9ff37df91fb258d7ce13eed9abb0fe934d613341825d"

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
