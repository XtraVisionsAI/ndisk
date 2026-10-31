// swift-tools-version:5.10
// NdiskKit：Rust 核心（NdiskFFI.xcframework）的 Swift 封装，供菜单栏 App 与 File Provider 扩展共用。
// 构建前先运行 apple/scripts/build-rust.sh 生成 xcframework 与绑定代码。
import PackageDescription

let package = Package(
    name: "NdiskKit",
    platforms: [.macOS(.v13)],
    products: [
        // 动态库：Rust 核心只链接进 NdiskKit.framework 一份，嵌入 App 的 Frameworks，
        // File Provider 扩展经 rpath 共用，而不是 App 与扩展各静态链接一份。
        .library(name: "NdiskKit", type: .dynamic, targets: ["NdiskKit"]),
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
                // 只导出本模块的 Swift 符号：Rust 核心的 C 接口只被 Generated/ 绑定在框架内部调用，
                // 不导出它们，-dead_strip 才能剔除未用到的 Rust 代码（Xcode 链接包框架时不会自动加）；
                // -x 不把本地符号写进产物（符号化用单独生成的 dSYM）。
                .unsafeFlags([
                    "-Xlinker", "-exported_symbol", "-Xlinker", "_$s8NdiskKit*",
                    "-Xlinker", "-dead_strip",
                    "-Xlinker", "-x",
                ]),
            ]
        ),
        .testTarget(name: "NdiskKitTests", dependencies: ["NdiskKit"]),
    ],
    swiftLanguageVersions: [.v5]
)
