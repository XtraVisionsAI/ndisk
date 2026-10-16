//! Swift 绑定生成器。由 `apple/scripts/build-rust.sh` 调用：
//! `cargo run -p ndisk-ffi --bin uniffi-bindgen-swift -- <lib> <out> --swift-sources --headers --modulemap ...`
fn main() {
    uniffi::uniffi_bindgen_swift()
}
