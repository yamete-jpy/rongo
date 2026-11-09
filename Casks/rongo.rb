cask "rongo" do
  version "0.1.0"
  sha256 "09b873af00cb18c392271fc75cdb3e0a43badec75facb01b7837fb482811a866"

  url "https://github.com/yamete-jpy/rongo/releases/download/v#{version}/rongo-#{version}-arm64.dmg"
  name "rongo"
  desc "Desktop MongoDB client"
  homepage "https://github.com/yamete-jpy/rongo"

  depends_on arch: :arm64
  depends_on macos: ">= :big_sur"

  app "rongo.app"

  zap trash: "~/Library/Application Support/dev.rongo.rongo"

  caveats <<~EOS
    rongo is not notarized by Apple, so macOS blocks the first launch.
    Open System Settings > Privacy & Security and press Open Anyway.
  EOS
end
