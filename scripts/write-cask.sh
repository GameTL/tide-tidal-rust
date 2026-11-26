#!/bin/sh
# Print the Homebrew cask (Casks/tide.rb) for a Linux release.
#
#   sh scripts/write-cask.sh <version> <sha256 x86_64 tarball> <sha256 aarch64 tarball>
#
# A cask, not a formula: Homebrew refuses a formula without a bottle unless a
# C compiler is installed, and Bazzite has none. The release workflow commits
# the result to staging, the default branch, which is what
# `brew tap gametl/tide https://github.com/GameTL/tide-tidal-rust` clones.
# Each tarball holds the prebuilt `TIDE` binary and `tide.png`.
# TIDE_RELEASE_URL overrides where the tarballs come from (file:///... to try
# the cask before a release exists).
set -eu

version=${1#v}
sha_x64=$2
sha_arm=$3
base=${TIDE_RELEASE_URL:-https://github.com/${GITHUB_REPOSITORY:-GameTL/tide-tidal-rust}/releases/download/v#\{version\}}

cat <<EOF
# Written by scripts/write-cask.sh on every release. Do not edit by hand.
cask "tide" do
  arch arm: "aarch64", intel: "x86_64"

  version "${version}"
  sha256 arm64_linux:  "${sha_arm}",
         x86_64_linux: "${sha_x64}"

  url "${base}/TIDE-#{version}-linux-#{arch}.tar.gz"
  name "TIDE"
  desc "Native TIDAL desktop app with bit-perfect playback and parametric EQ"
  homepage "https://gametl.github.io/tide-tidal-rust/"

  depends_on linux: :any

  binary "TIDE", target: "tide"
  artifact "tide.png", target: "#{Dir.home}/.local/share/icons/tide.png"

  # The app menu entry (GNOME, KDE): written here because it needs the
  # absolute path of this Homebrew prefix.
  postflight_steps do
    mkdir_p ".local/share/applications", base: :home
    write_file ".local/share/applications/tide.desktop", <<~DESKTOP, base: :home
      [Desktop Entry]
      Type=Application
      Name=TIDE
      Comment=Native TIDAL desktop app
      Exec=#{HOMEBREW_PREFIX}/bin/tide
      Icon=#{Dir.home}/.local/share/icons/tide.png
      Categories=AudioVideo;Audio;Player;
      Terminal=false
    DESKTOP
  end

  uninstall_postflight_steps do
    remove ".local/share/applications/tide.desktop", base: :home
  end

  zap trash: [
    "~/.cache/tidal-rust",
    "~/.local/share/tidal-rust",
  ]
end
EOF
