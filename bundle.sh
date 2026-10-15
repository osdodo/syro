#!/bin/sh
# Build the macOS .app bundle for syro.

set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
cd "$root"

main_rs="crates/app/src/main.rs"
app_toml="crates/app/Cargo.toml"

# The constant, not the value: both files are the user's to edit, and the point
# is to catch them disagreeing.
declared=$(sed -n 's/^const BUNDLE_ID: *&str *= *"\([^"]*\)".*/\1/p' "$main_rs" | head -1)
metadata=$(sed -n 's/^identifier *= *"\([^"]*\)".*/\1/p' "$app_toml" | head -1)

if [ -z "$declared" ]; then
    echo "bundle.sh: no BUNDLE_ID in $main_rs" >&2
    exit 1
fi
if [ -z "$metadata" ]; then
    echo "bundle.sh: no [package.metadata.bundle] identifier in $app_toml" >&2
    exit 1
fi
if [ "$declared" != "$metadata" ]; then
    echo "bundle.sh: bundle id disagrees between $main_rs and $app_toml:" >&2
    echo "  $main_rs:         $declared" >&2
    echo "  $app_toml:        $metadata" >&2
    echo "  Notifications are dropped unless both are the same. Fix one of them." >&2
    exit 1
fi

if ! command -v cargo-bundle >/dev/null 2>&1; then
    echo "bundle.sh: cargo-bundle is not installed (cargo install cargo-bundle)" >&2
    exit 1
fi

cargo bundle --release --package syro --format osx

bundle="target/release/bundle/osx/syro.app"
if [ ! -d "$bundle" ]; then
    echo "bundle.sh: cargo bundle reported success but $bundle is not there" >&2
    exit 1
fi

# The check above compares our two copies with each other; this one compares the
# copy macOS will actually read.
plist=/usr/libexec/PlistBuddy
if [ -x "$plist" ]; then
    shipped=$("$plist" -c 'Print :CFBundleIdentifier' "$bundle/Contents/Info.plist" 2>/dev/null || echo "")
    if [ "$shipped" != "$declared" ]; then
        echo "bundle.sh: the Info.plist says '$shipped', main.rs says '$declared'" >&2
        exit 1
    fi

    # cargo-bundle resolves the `icon` paths as globs from the current directory
    # and ships a bundle with no icon, silently, when they miss.
    icon=$("$plist" -c 'Print :CFBundleIconFile' "$bundle/Contents/Info.plist" 2>/dev/null || echo "")
    if [ ! -f "$bundle/Contents/Resources/$icon" ]; then
        echo "bundle.sh: no icon in the bundle (Contents/Resources/$icon is missing);" >&2
        echo "  run this script from the repo root so cargo-bundle's icon glob matches" >&2
        exit 1
    fi
fi

# `cargo-bundle` leaves the app linker-signed: the signature has no bound
# `Info.plist`, and its identifier comes from the executable name (`syro-<hash>`)
# rather than the bundle. `usernotificationsd` then rejects every authorization
# request — "Entitlement 'com.apple.private.usernotifications.bundle-identifiers'
# required to request user notifications" — so the permission prompt never
# appears and no notification is delivered. Re-signing ad-hoc with the bundle in
# place binds the plist and the identifier macOS reads.
codesign --force --sign - "$bundle"
signed_id=$(codesign -dv "$bundle" 2>&1 | sed -n 's/^Identifier=//p' | head -1)
if [ "$signed_id" != "$declared" ]; then
    echo "bundle.sh: the signature says '$signed_id', main.rs says '$declared'" >&2
    exit 1
fi

# The drag-to-install disk image: logo behind the two icons, app on the left,
# Applications on the right. `hdiutil` and `sips` are in the base system, so
# this is the whole installer: no create-dmg, no pkgbuild, no extra run of
# cargo. It does drive Finder once, to write the .DS_Store that carries the
# window layout — that is the only way macOS stores it.
work=$(mktemp -d)
stage="$work/stage"
mount=""
cleanup() {
    # A build that dies mid-way must not leave the image mounted.
    [ -z "$mount" ] || hdiutil detach -quiet "$mount" 2>/dev/null || true
    rm -rf "$work"
}
trap cleanup EXIT

mkdir -p "$stage/.background"
cp -R "$bundle" "$stage/"
ln -s /Applications "$stage/Applications"

# logo.png is the mark on transparency, so the window behind it is Finder's
# own. The mark keeps the position and scale it has in the file, so scaling the
# whole square and cropping the middle lands it where the two icons leave room.
square="$work/square.png"
bg="$work/background.png"
sips -z 660 660 crates/app/assets/logo.png --out "$square" >/dev/null
sips -c 480 660 "$square" --out "$bg" >/dev/null
cp "$bg" "$stage/.background/background.png"

rw="$work/syro-rw.dmg"
hdiutil create -quiet -volname syro -srcfolder "$stage" -format UDRW -ov "$rw"

# Finder addresses the volume by name, so it has to land on the plain
# /Volumes/syro; a second one would be renamed and the script below would miss.
if [ -e /Volumes/syro ]; then
    echo "bundle.sh: /Volumes/syro is already mounted; eject it and re-run" >&2
    exit 1
fi
hdiutil attach -quiet -readwrite "$rw"
mount=/Volumes/syro

# The positions are the icon's centre in window coordinates, so 430 is below
# the logo, which the crop leaves ending around 361. The picture lives in
# a dot-named folder because Finder refuses a *hidden file* as the background;
# a normal file inside a hidden folder is fine and stays out of sight.
osascript <<APPLESCRIPT
tell application "Finder"
    tell disk "syro"
        open
        set current view of container window to icon view
        set toolbar visible of container window to false
        set statusbar visible of container window to false
        set the bounds of container window to {200, 120, 860, 628}
        set opts to the icon view options of container window
        set arrangement of opts to not arranged
        set icon size of opts to 100
        set background picture of opts to (POSIX file "$mount/.background/background.png" as alias)
        set position of item "syro.app" of container window to {180, 430}
        set position of item "Applications" of container window to {480, 430}
        close
    end tell
end tell
APPLESCRIPT

# Finder flushes .DS_Store on its own clock, and a convert that runs before it
# lands ships a plain window with no error at all.
tries=0
while [ ! -f "$mount/.DS_Store" ] && [ "$tries" -lt 20 ]; do
    sleep 0.5
    tries=$((tries + 1))
done
if [ ! -f "$mount/.DS_Store" ]; then
    echo "bundle.sh: Finder did not write the window layout (.DS_Store) to the image" >&2
    exit 1
fi

hdiutil detach -quiet "$mount"
mount=""

dmg="target/release/bundle/osx/syro.dmg"
rm -f "$dmg"
hdiutil convert -quiet "$rw" -format UDZO -o "$dmg"

if [ ! -f "$dmg" ]; then
    echo "bundle.sh: hdiutil reported success but $dmg is not there" >&2
    exit 1
fi

echo "bundle: $root/$bundle"
echo "dmg:    $root/$dmg"
echo "run:    open '$root/$dmg'"
