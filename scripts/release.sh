#!/bin/zsh
# Builds dist/ferroamp-<version>.dmg: a universal app (Apple silicon and Intel)
# on a disk image with a shortcut to Applications, ready to attach to a release.
#
# Signing is optional. Without these the app is ad-hoc signed, which works but
# makes macOS warn on first launch:
#   SIGN_IDENTITY   "Developer ID Application: Name (TEAMID)" in your keychain
#   NOTARY_PROFILE  a profile saved with `xcrun notarytool store-credentials`,
#                   to have Apple notarize the disk image so it opens cleanly
set -euo pipefail
cd "$(dirname "$0")/.."
export PATH="/opt/homebrew/opt/rustup/bin:$HOME/.cargo/bin:$PATH"
export MACOSX_DEPLOYMENT_TARGET=12.0

version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
targets=(aarch64-apple-darwin x86_64-apple-darwin)
rustup target add "${targets[@]}" >/dev/null
for t in "${targets[@]}"; do
  cargo build --release --locked --target "$t"
done
mkdir -p dist
lipo -create -output dist/ferroamp-universal \
  target/aarch64-apple-darwin/release/ferroamp target/x86_64-apple-darwin/release/ferroamp

FERROAMP_BINARY=dist/ferroamp-universal scripts/bundle.sh >/dev/null
rm dist/ferroamp-universal
app=dist/ferroamp.app
python3 scripts/third_party_licenses.py > "$app/Contents/Resources/ThirdPartyLicenses.txt"

# Signed again, as adding a file broke the seal from bundle.sh.
if [[ -n "${SIGN_IDENTITY:-}" ]]; then
  codesign --force --options runtime --timestamp --sign "$SIGN_IDENTITY" "$app"
else
  codesign --force --sign - "$app"
fi
codesign --verify --strict "$app"

stage=$(mktemp -d)
cp -R "$app" "$stage/"
ln -s /Applications "$stage/Applications"
cp "$app/Contents/Resources/ThirdPartyLicenses.txt" "$stage/Licenses.txt"
dmg=dist/ferroamp-$version.dmg
rm -f "$dmg"
hdiutil create -quiet -volname "ferroamp $version" -srcfolder "$stage" -fs HFS+ -format UDZO "$dmg"
rm -rf "$stage"

if [[ -n "${SIGN_IDENTITY:-}" ]]; then
  codesign --force --timestamp --sign "$SIGN_IDENTITY" "$dmg"
fi
if [[ -n "${NOTARY_PROFILE:-}" ]]; then
  xcrun notarytool submit "$dmg" --keychain-profile "$NOTARY_PROFILE" --wait
  xcrun stapler staple "$dmg"
fi
shasum -a 256 "$dmg" | tee "$dmg.sha256"
echo "Built $dmg ($(du -h "$dmg" | cut -f1))"
