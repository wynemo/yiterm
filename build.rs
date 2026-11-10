fn main() {
    println!("cargo:rerun-if-changed=native/ScreenCapture.swift");
    #[cfg(target_os = "macos")]
    {
        let output = std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap())
            .join("yiterm-screen-capture");
        let arch = match std::env::var("CARGO_CFG_TARGET_ARCH").unwrap().as_str() {
            "aarch64" => "arm64",
            "x86_64" => "x86_64",
            _ => panic!("unsupported macOS capture architecture"),
        };
        let swift_target = format!("{arch}-apple-macosx13.0");
        let status = std::process::Command::new("xcrun")
            .args([
                "swiftc",
                "-O",
                "-swift-version",
                "5",
                "native/ScreenCapture.swift",
                "-o",
            ])
            .arg(&output)
            .args(["-target", &swift_target])
            .status()
            .expect("macOS screen capture requires Xcode Command Line Tools (swiftc)");
        assert!(
            status.success(),
            "failed to compile macOS screen capture helper"
        );
    }
    println!("cargo:rerun-if-changed=assets/icons/yiterm.rc");
    println!("cargo:rerun-if-changed=assets/icons/yiterm.ico");
    // GPUI loads the application's Windows icon from resource ID 1.
    #[cfg(windows)]
    embed_resource::compile_for("assets/icons/yiterm.rc", ["yiterm"], embed_resource::NONE)
        .manifest_required()
        .expect("failed to embed YiTerm application icon");
}
