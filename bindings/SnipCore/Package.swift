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
            path: "Sources/SnipCore",
            linkerSettings: [
                // libp2p's transports reference SystemConfiguration symbols
                // (reachability, dynamic store); all are exported by the
                // iOS SDK but the framework must be linked explicitly.
                .linkedFramework("SystemConfiguration")
            ]
        ),
    ]
)
