#!/usr/bin/env bash
# Build SnipCore.xcframework: the snip-mobile crate compiled for iOS
# device + simulator, wrapped with UniFFI-generated Swift bindings in a
# SwiftPM package under bindings/SnipCore.
#
# Prereqs: rustup targets aarch64-apple-ios and aarch64-apple-ios-sim,
# Xcode command line tools.
#
# Usage: scripts/build-xcframework.sh

set -euo pipefail
cd "$(dirname "$0")/.."

PROFILE=mobile
CRATE=snip_mobile
PKG_DIR=bindings/SnipCore
GEN_DIR=target/uniffi-bindings

echo "── 1/4 building iOS slices ($PROFILE profile)"
cargo build -p snip-mobile --profile "$PROFILE" --target aarch64-apple-ios
cargo build -p snip-mobile --profile "$PROFILE" --target aarch64-apple-ios-sim

echo "── 2/4 generating Swift bindings"
# --library mode introspects a host build of the cdylib.
cargo build -p snip-mobile
rm -rf "$GEN_DIR"
cargo run -p snip-mobile --features cli --bin uniffi-bindgen -- \
    generate --library "target/debug/lib${CRATE}.dylib" \
    --language swift --out-dir "$GEN_DIR"

echo "── 3/4 assembling XCFramework (framework-style)"
# Framework-style slices so the modulemap lives inside the framework
# bundle: library-style xcframeworks copy headers into the shared
# products include/ dir, which collides with other packages doing the
# same (e.g. Clibsodium).
FW_WORK="$GEN_DIR/frameworks"
rm -rf "$FW_WORK"
for SLICE in aarch64-apple-ios aarch64-apple-ios-sim; do
    FW="$FW_WORK/$SLICE/${CRATE}FFI.framework"
    mkdir -p "$FW/Headers" "$FW/Modules"
    cp "target/$SLICE/$PROFILE/lib${CRATE}.a" "$FW/${CRATE}FFI"
    cp "$GEN_DIR/${CRATE}FFI.h" "$FW/Headers/"
    cat > "$FW/Modules/module.modulemap" <<MODEOF
framework module ${CRATE}FFI {
    umbrella header "${CRATE}FFI.h"
    export *
    module * { export * }
}
MODEOF
    cat > "$FW/Info.plist" <<PLISTEOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleIdentifier</key>
	<string>io.sumchain.snip-mobile-ffi</string>
	<key>CFBundleName</key>
	<string>${CRATE}FFI</string>
	<key>CFBundlePackageType</key>
	<string>FMWK</string>
	<key>CFBundleVersion</key>
	<string>1</string>
	<key>MinimumOSVersion</key>
	<string>16.0</string>
</dict>
</plist>
PLISTEOF
done

rm -rf "$PKG_DIR/SnipCore.xcframework"
mkdir -p "$PKG_DIR"
xcodebuild -create-xcframework \
    -framework "$FW_WORK/aarch64-apple-ios/${CRATE}FFI.framework" \
    -framework "$FW_WORK/aarch64-apple-ios-sim/${CRATE}FFI.framework" \
    -output "$PKG_DIR/SnipCore.xcframework"

echo "── 4/4 laying out SwiftPM package"
mkdir -p "$PKG_DIR/Sources/SnipCore"
cp "$GEN_DIR/${CRATE}.swift" "$PKG_DIR/Sources/SnipCore/"

if [ ! -f "$PKG_DIR/Package.swift" ]; then
cat > "$PKG_DIR/Package.swift" <<'EOF'
// swift-tools-version:5.9
import PackageDescription

let package = Package(
    name: "SnipCore",
    platforms: [.iOS(.v16)],
    products: [
        .library(name: "SnipCore", targets: ["SnipCore"])
    ],
    targets: [
        .binaryTarget(name: "SnipCoreFFI", path: "SnipCore.xcframework"),
        .target(
            name: "SnipCore",
            dependencies: ["SnipCoreFFI"],
            path: "Sources/SnipCore"
        ),
    ]
)
EOF
fi

du -sh "$PKG_DIR/SnipCore.xcframework"
echo "done: $PKG_DIR (add as a local SwiftPM package, or zip the"
echo "xcframework for a release-asset binaryTarget)"
