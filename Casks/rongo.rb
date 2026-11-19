cask "rongo" do
  version "0.1.7"
  sha256 "7f4535c011d424254eec2a3270eeb09cca0ccdf8f86a1227884630de38ba5ed9"

  url "https://github.com/yamete-jpy/rongo/releases/download/v#{version}/rongo-#{version}-arm64.dmg"
  name "rongo"
  desc "Desktop MongoDB client"
  homepage "https://github.com/yamete-jpy/rongo"

  depends_on arch: :arm64
  depends_on macos: :big_sur

  app "rongo.app"

  zap trash: "~/Library/Application Support/dev.rongo.rongo"

  caveats <<~EOS
    rongo is not notarized by Apple, so macOS blocks its first launch,
    after an update too. Open System Settings > Privacy & Security and
    press Open Anyway, or run:
      xattr -dr com.apple.quarantine /Applications/rongo.app
    To update: brew update && brew upgrade --cask rongo
  EOS
end
