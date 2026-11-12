cask "rongo" do
  version "0.1.4"
  sha256 "06c14f99e857d2539ccbdd5a008a199bf1e2263a6b347b8fe9ea9be5c1d40741"

  url "https://github.com/yamete-jpy/rongo/releases/download/v#{version}/rongo-#{version}-arm64.dmg"
  name "rongo"
  desc "Desktop MongoDB client"
  homepage "https://github.com/yamete-jpy/rongo"

  depends_on arch: :arm64
  depends_on macos: :big_sur

  app "rongo.app"

  zap trash: "~/Library/Application Support/dev.rongo.rongo"

  caveats <<~EOS
    rongo is not notarized by Apple, so macOS blocks the first launch.
    Open System Settings > Privacy & Security and press Open Anyway.
  EOS
end
