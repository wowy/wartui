// swift-tools-version: 6.4
// The Swift package over `wartui-ffi`: the generated `WartuiCore` over the C module
// `WartuiCoreFFI`, whose XCFramework holds the static library for the device and the simulator.
// `../package/build.sh ios` places both; an app adds this package by its local path.

import PackageDescription

let package = Package(
    name: "WartuiCore",
    // Keep it equal to `ios_minimum` in `../package/build.sh`, which builds the libraries for it.
    platforms: [.iOS(.v17)],
    products: [
        .library(name: "WartuiCore", targets: ["WartuiCore"])
    ],
    targets: [
        .binaryTarget(name: "WartuiCoreFFI", path: "WartuiCoreFFI.xcframework"),
        .target(name: "WartuiCore", dependencies: ["WartuiCoreFFI"]),
        .testTarget(name: "WartuiCoreTests", dependencies: ["WartuiCore"]),
    ],
    swiftLanguageModes: [.v6]
)
