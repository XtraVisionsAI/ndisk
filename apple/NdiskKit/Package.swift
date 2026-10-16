// swift-tools-version:5.10
// NdiskKit：Rust 核心（NdiskFFI.xcframework）的 Swift 封装，供菜单栏 App 与 File Provider 扩展共用。
// 构建前先运行 apple/scripts/build-rust.sh 生成 xcframework 与绑定代码。
import PackageDescription

let package = Package(
    name: "NdiskKit",
    platforms: [.macOS(.v13)],
    products: [
        .library(name: "NdiskKit", targets: ["NdiskKit"]),
    ],
    targets: [
        .binaryTarget(name: "NdiskFFI", path: "Frameworks/NdiskFFI.xcframework"),
        .target(
            name: "NdiskKit",
            dependencies: ["NdiskFFI"],
            linkerSettings: [
                // rustls-native-certs / security-framework 与 tokio 需要的系统框架
                .linkedFramework("Security"),
                .linkedFramework("CoreFoundation"),
                .linkedFramework("SystemConfiguration"),
            ]
        ),
        .testTarget(name: "NdiskKitTests", dependencies: ["NdiskKit"]),
    ],
    swiftLanguageVersions: [.v5]
)
