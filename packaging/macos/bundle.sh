#!/bin/sh
# Assembles Aviary.app around an already-built release binary.
#
# A bundle is not cosmetic on macOS: without one there is no bundle identifier,
# so notifications are refused, and no CFBundleURLTypes, so the system will
# never hand Aviary a `mailto:` link. Run `cargo build --release` first.
#
#   packaging/macos/bundle.sh [output-directory]
#
# The result is unsigned. macOS will refuse to open it on another machine until
# it is signed and notarized, or until the user clears the quarantine attribute
# (`xattr -d com.apple.quarantine Aviary.app`).
set -eu

here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../.." && pwd)"
out="${1:-$root/target/macos}"

binary="$root/target/release/aviary"
[ -f "$binary" ] || { echo "no release binary at $binary — run cargo build --release" >&2; exit 1; }

version="$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/Cargo.toml" | head -1)"

app="$out/Aviary.app"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"

sed "s/@VERSION@/$version/g" "$here/Info.plist" > "$app/Contents/Info.plist"
cp "$binary" "$app/Contents/MacOS/aviary"
chmod 755 "$app/Contents/MacOS/aviary"

# The icon is optional: an .icns cannot be produced from the SVG without tools
# that are not guaranteed to be installed, and a bundle without one still runs.
if [ -f "$here/aviary.icns" ]; then
    cp "$here/aviary.icns" "$app/Contents/Resources/aviary.icns"
else
    echo "note: packaging/macos/aviary.icns is absent — the bundle gets the generic icon" >&2
fi

# Legacy but still expected by parts of Launch Services.
printf 'APPL????' > "$app/Contents/PkgInfo"

echo "built $app"
echo "install it with: cp -R \"$app\" /Applications/"
