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

echo "── 3/4 assembling XCFramework"
# xcodebuild wants one headers dir per slice containing the C header +
# a modulemap named exactly module.modulemap.
HDR="$GEN_DIR/include"
rm -rf "$HDR"
mkdir -p "$HDR"
cp "$GEN_DIR/${CRATE}FFI.h" "$HDR/"
cp "$GEN_DIR/${CRATE}FFI.modulemap" "$HDR/module.modulemap"

rm -rf "$PKG_DIR/SnipCore.xcframework"
mkdir -p "$PKG_DIR"
xcodebuild -create-xcframework \
    -library "target/aarch64-apple-ios/$PROFILE/lib${CRATE}.a" -headers "$HDR" \
    -library "target/aarch64-apple-ios-sim/$PROFILE/lib${CRATE}.a" -headers "$HDR" \
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
