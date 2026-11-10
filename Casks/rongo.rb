cask "rongo" do
  version "0.1.1"
  sha256 "2a7d92deff3f8f6fdc320ec1f74e555861eef7fd80409ff1669966db6e370d39"

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
