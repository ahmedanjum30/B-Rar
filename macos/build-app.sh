#!/bin/sh
# Builds B-Rar.app for macOS and zips it as dist/B-Rar-<version>-macos.zip.
#
#   sh macos/build-app.sh            # Apple Silicon and Intel (universal; needs rustup)
#   sh macos/build-app.sh --native   # only this Mac's kind, faster for testing
#
# The app: the Swift/SwiftUI window code in macos/Sources, linked with the Rust engine as a
# static library (crates/brar-ffi), and the B-Rar command line (crates/brar-cli) next to it
# for Homebrew to put on the PATH. Signed ad hoc, not with an Apple Developer ID.

set -eu
cd "$(dirname "$0")/.."

version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n 1)
app=dist/B-Rar.app
min_macos=13.0

if [ "${1:-}" = "--native" ]; then
    targets=$(uname -m | sed 's/arm64/aarch64/')-apple-darwin
else
    targets="aarch64-apple-darwin x86_64-apple-darwin"
    rustup target add $targets
fi

engines=""
clis=""
guis=""
for target in $targets; do
    MACOSX_DEPLOYMENT_TARGET=$min_macos cargo build --release -p brar-ffi -p brar-cli --target "$target"
    swift_target=$(echo "$target" | sed 's/aarch64/arm64/; s/-apple-darwin/-apple-macos'$min_macos'/')
    out="target/$target/release"
    xcrun swiftc -O -swift-version 5 -target "$swift_target" \
        -import-objc-header macos/brar.h \
        macos/Sources/*.swift "$out/libbrar_ffi.a" \
        -framework AppKit -framework SwiftUI -framework UniformTypeIdentifiers -lc++ \
        -o "$out/B-Rar-GUI"
    guis="$guis $out/B-Rar-GUI"
    clis="$clis $out/B-Rar"
done

rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
# shellcheck disable=SC2086
lipo -create -output "$app/Contents/MacOS/B-Rar-GUI" $guis
# shellcheck disable=SC2086
lipo -create -output "$app/Contents/MacOS/B-Rar" $clis

# The icon, from the same artwork as the Windows one.
iconset=target/B-Rar.iconset
rm -rf "$iconset" && mkdir -p "$iconset"
for size in 16 32 128 256 512; do
    sips -z $size $size assets/brar-full-512.png --out "$iconset/icon_${size}x${size}.png" >/dev/null
    double=$((size * 2))
    [ $double -le 512 ] && sips -z $double $double assets/brar-full-512.png --out "$iconset/icon_${size}x${size}@2x.png" >/dev/null
done
cp assets/brar-full-512.png "$iconset/icon_512x512@2x.png"
iconutil -c icns "$iconset" -o "$app/Contents/Resources/B-Rar.icns"

# The archive types B-Rar opens. macOS knows zip, tar and the compressors; rar, 7z and
# zst are declared here (ignored where another app or macOS already does).
archive_types="public.zip-archive com.rarlab.rar-archive org.7-zip.7-zip-archive public.tar-archive org.gnu.gnu-zip-archive org.gnu.gnu-zip-tar-archive public.bzip2-archive org.tukaani.xz-archive com.facebook.zstandard-archive"
types=""
for t in $archive_types; do types="$types<string>$t</string>"; done

# One Finder Quick Action (macOS Service): message, menu title, file types.
service() {
    cat <<EOF
        <dict>
            <key>NSMessage</key><string>$1</string>
            <key>NSPortName</key><string>B-Rar</string>
            <key>NSMenuItem</key><dict><key>default</key><string>$2</string></dict>
            <key>NSSendFileTypes</key><array>$3</array>
            <key>NSRequiredContext</key><dict/>
        </dict>
EOF
}

cat > "$app/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleIdentifier</key><string>com.ahmadanjum.brar</string>
    <key>CFBundleName</key><string>B-Rar</string>
    <key>CFBundleDisplayName</key><string>B-Rar</string>
    <key>CFBundleExecutable</key><string>B-Rar-GUI</string>
    <key>CFBundleIconFile</key><string>B-Rar</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleShortVersionString</key><string>$version</string>
    <key>CFBundleVersion</key><string>$version</string>
    <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
    <key>LSMinimumSystemVersion</key><string>$min_macos</string>
    <key>LSApplicationCategoryType</key><string>public.app-category.utilities</string>
    <key>NSHighResolutionCapable</key><true/>
    <key>NSHumanReadableCopyright</key><string>Ahmad Anjum</string>
    <key>CFBundleDocumentTypes</key>
    <array>
        <dict>
            <key>CFBundleTypeName</key><string>Archive</string>
            <key>CFBundleTypeRole</key><string>Viewer</string>
            <!-- Offered in Open With; Archive Utility stays the default for zip unless changed. -->
            <key>LSHandlerRank</key><string>Alternate</string>
            <key>LSItemContentTypes</key><array>$types</array>
        </dict>
    </array>
    <key>UTImportedTypeDeclarations</key>
    <array>
        <dict>
            <key>UTTypeIdentifier</key><string>com.rarlab.rar-archive</string>
            <key>UTTypeDescription</key><string>RAR archive</string>
            <key>UTTypeConformsTo</key><array><string>public.data</string><string>public.archive</string></array>
            <key>UTTypeTagSpecification</key><dict>
                <key>public.filename-extension</key><array><string>rar</string></array>
                <key>public.mime-type</key><array><string>application/vnd.rar</string><string>application/x-rar-compressed</string></array>
            </dict>
        </dict>
        <dict>
            <key>UTTypeIdentifier</key><string>org.7-zip.7-zip-archive</string>
            <key>UTTypeDescription</key><string>7-Zip archive</string>
            <key>UTTypeConformsTo</key><array><string>public.data</string><string>public.archive</string></array>
            <key>UTTypeTagSpecification</key><dict>
                <key>public.filename-extension</key><array><string>7z</string></array>
                <key>public.mime-type</key><array><string>application/x-7z-compressed</string></array>
            </dict>
        </dict>
        <dict>
            <key>UTTypeIdentifier</key><string>com.facebook.zstandard-archive</string>
            <key>UTTypeDescription</key><string>Zstandard archive</string>
            <key>UTTypeConformsTo</key><array><string>public.data</string><string>public.archive</string></array>
            <key>UTTypeTagSpecification</key><dict>
                <key>public.filename-extension</key><array><string>zst</string></array>
            </dict>
        </dict>
    </array>
    <key>NSServices</key>
    <array>
$(service openArchive "B-Rar: Open in B-Rar" "$types")
$(service extractFiles "B-Rar: Extract Files…" "$types")
$(service extractToFolder "B-Rar: Extract to Folder" "$types")
$(service extractHere "B-Rar: Extract Here" "$types")
$(service testArchive "B-Rar: Test Archive" "$types")
$(service addToZip "B-Rar: Add to ZIP" "<string>public.item</string>")
$(service addToArchive "B-Rar: Add to Archive…" "<string>public.item</string>")
    </array>
</dict>
</plist>
EOF
plutil -lint "$app/Contents/Info.plist" >/dev/null

codesign --force --sign - "$app/Contents/MacOS/B-Rar"
codesign --force --sign - "$app"
codesign --verify --strict "$app"

zip="dist/B-Rar-$version-macos.zip"
rm -f "$zip"
ditto -c -k --keepParent "$app" "$zip"
echo "$app"
echo "$zip ($(du -h "$zip" | cut -f1))"
