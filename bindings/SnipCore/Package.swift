// swift-tools-version:5.9
import PackageDescription

let package = Package(
    name: "SnipCore",
    platforms: [.iOS(.v16), .macOS(.v13)],
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
