// swift-tools-version: 6.0

import PackageDescription

let package = Package(
    name: "AIMemoryMenu",
    platforms: [.macOS(.v14)],
    products: [
        .executable(name: "AIMemoryMenu", targets: ["AIMemoryMenu"]),
    ],
    targets: [
        .target(
            name: "AIMemoryMenuCore",
            path: "Sources/AIMemoryMenuCore"
        ),
        .executableTarget(
            name: "AIMemoryMenu",
            dependencies: ["AIMemoryMenuCore"],
            path: "Sources/AIMemoryMenu"
        ),
        .testTarget(
            name: "AIMemoryMenuTests",
            dependencies: ["AIMemoryMenuCore"],
            path: "Tests/AIMemoryMenuTests",
            resources: [.copy("fixtures")]
        ),
    ]
)
