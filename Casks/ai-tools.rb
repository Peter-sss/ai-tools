cask "ai-tools" do
  version "1.3.26"
  sha256 "f69747e8aa885f64e1addc47c5ebd23d556b657e88131a7f8a5f8306f6f83424"

  url "https://github.com/Peter-sss/ai-tools/releases/download/v#{version}/ai-tools_#{version}_universal.dmg",
      verified: "github.com/Peter-sss/ai-tools/"
  name "ai-tools"
  desc "Account manager for AI IDEs (Antigravity and Codex)"
  homepage "https://github.com/Peter-sss/ai-tools"

  auto_updates true

  postflight do
    system_command "/usr/bin/xattr",
                   args: ["-cr", "#{appdir}/ai-tools.app"],
                   sudo: true
  end

  app "ai-tools.app"

  zap trash: [
    "~/Library/Application Support/com.ai-tools.app",
    "~/Library/Caches/com.ai-tools.app",
    "~/Library/Preferences/com.ai-tools.app.plist",
    "~/Library/Saved Application State/com.ai-tools.app.savedState",
  ]

  caveats <<~EOS
    The app is automatically quarantined by macOS. A postflight hook has been added to remove this quarantine.
    If you still encounter the "App is damaged" error, please run:
      sudo xattr -rd com.apple.quarantine "/Applications/ai-tools.app"
  EOS
end
