// swift-tools-version:5.10
// 开发用命令行：不经菜单栏 UI 直接登记挂载 / 注册 domain / 查询状态，供自动化验证使用。
import PackageDescription

let package = Package(
    name: "ndisk-devctl",
    platforms: [.macOS(.v13)],
    dependencies: [.package(path: "../../NdiskKit")],
    targets: [
        .executableTarget(name: "ndisk-devctl", dependencies: ["NdiskKit"], path: "Sources"),
    ]
)
