cask "rongo" do
  version "0.1.2"
  sha256 "ec44b738ac9bc7f2a55e06d927c3656e027163d7b7d65899a21e979a3216d800"

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
