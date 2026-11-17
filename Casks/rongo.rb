cask "rongo" do
  version "0.1.6"
  sha256 "3931348e0bfe7c23dd3ce0b729db9a50638edabaf23506e14339e5b66607ce60"

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
