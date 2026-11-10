#!/bin/zsh
# Builds ferroamp.app in ./dist. Pass --install to also copy it to ~/Applications.
set -euo pipefail
cd "$(dirname "$0")/.."
export PATH="/opt/homebrew/opt/rustup/bin:$HOME/.cargo/bin:$PATH"

cargo build --release

app=dist/ferroamp.app
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp target/release/ferroamp "$app/Contents/MacOS/ferroamp"
cp assets/Info.plist "$app/Contents/Info.plist"

# Icon: nearest-neighbour-looking pixel art at every size macOS asks for.
iconset=$(mktemp -d)/ferroamp.iconset
mkdir -p "$iconset"
for size in 16 32 64 128 256 512; do
  sips -z $size $size assets/icon.png --out "$iconset/icon_${size}x${size}.png" >/dev/null
  double=$((size * 2))
  sips -z $double $double assets/icon.png --out "$iconset/icon_${size}x${size}@2x.png" >/dev/null
done
iconutil -c icns "$iconset" -o "$app/Contents/Resources/ferroamp.icns"

# Ad-hoc signature: enough for your own Mac. Distribution needs a Developer ID.
codesign --force --sign - "$app" >/dev/null 2>&1

echo "Built $app ($(du -sh "$app" | cut -f1))"
if [[ "${1:-}" == "--install" ]]; then
  mkdir -p "$HOME/Applications"
  rm -rf "$HOME/Applications/ferroamp.app"
  cp -R "$app" "$HOME/Applications/ferroamp.app"
  echo "Installed to ~/Applications/ferroamp.app"
fi
