fn main() {
    println!("cargo:rerun-if-changed=assets/icon.ico");
    // Embed the icon in the Windows executable (also works when cross-compiling).
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        winresource::WindowsResource::new()
            .set_icon("assets/icon.ico")
            .compile()
            .expect("failed to embed Windows icon");
    }
}
