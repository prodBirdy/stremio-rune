#!/bin/sh
# Builds a release binary and packs it with an icon, a launcher entry and an installer script as
# target/release/stremio-rune-<version>-linux-x86_64.tar.gz.
set -eu
cd "$(dirname "$0")/.."
version="${1:-$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)}"
cargo build --release
name="stremio-rune-$version-linux-x86_64"
stage="target/release/$name"
rm -rf "$stage"
mkdir -p "$stage"
cp target/release/stremio-rune "$stage/"
# The largest image in the .ico, as the launcher icon.
magick 'assets/icon.ico[0]' -resize 256x256 "$stage/stremio-rune.png" 2>/dev/null || magick assets/icon.ico -delete 1--1 "$stage/stremio-rune.png"
cat > "$stage/stremio-rune.desktop" <<'DESKTOP'
[Desktop Entry]
Type=Application
Name=Stremio Rune
Comment=Native Stremio client
Exec=stremio-rune
Icon=stremio-rune
Terminal=false
Categories=AudioVideo;Video;Player;
DESKTOP
cat > "$stage/install.sh" <<'INSTALL'
#!/bin/sh
# Installs for this user: the binary in ~/.local/bin, the launcher entry and icon under ~/.local/share.
set -eu
cd "$(dirname "$0")"
install -Dm755 stremio-rune "$HOME/.local/bin/stremio-rune"
install -Dm644 stremio-rune.png "$HOME/.local/share/icons/hicolor/256x256/apps/stremio-rune.png"
sed "s|^Exec=.*|Exec=$HOME/.local/bin/stremio-rune|" stremio-rune.desktop > "$HOME/.local/share/applications/stremio-rune.desktop"
echo "Installed. Start Stremio Rune from your launcher, or run ~/.local/bin/stremio-rune"
INSTALL
chmod +x "$stage/install.sh"
tar -C target/release -czf "target/release/$name.tar.gz" "$name"
echo "target/release/$name.tar.gz"
