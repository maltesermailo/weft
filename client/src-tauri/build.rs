fn main() {
    // libwebrtc's Objective-C classes rely on categories (e.g.
    // `NSString(stringForAbslStringView)`) that the linker dead-strips unless the
    // *final* binary is linked with `-ObjC`. `webrtc-sys` emits this flag for
    // itself, but a dependency's link-args don't propagate to the top-level
    // binary — so without this, LiveKit's video encoder factory crashes at
    // startup with "unrecognized selector". (macOS-only.)
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-link-arg=-ObjC");
    }

    tauri_build::build()
}
