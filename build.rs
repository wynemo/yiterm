fn main() {
    println!("cargo:rerun-if-changed=native/ScreenCapture.swift");
    println!("cargo:rerun-if-changed=native/ScreenUnlock.swift");
    println!("cargo:rerun-if-changed=native/VirtualDesktopCapture.swift");
    println!("cargo:rerun-if-changed=native/VirtualDisplay.m");
    #[cfg(target_os = "macos")]
    {
        let output = std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap())
            .join("libyiterm_capture.a");
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
                "-emit-library",
                "-static",
                "-module-name",
                "YiTermCapture",
                "-swift-version",
                "5",
                "native/ScreenCapture.swift",
                "native/ScreenUnlock.swift",
                "native/VirtualDesktopCapture.swift",
                "-o",
            ])
            .arg(&output)
            .args(["-target", &swift_target])
            .status()
            .expect("macOS screen capture requires Xcode Command Line Tools (swiftc)");
        assert!(
            status.success(),
            "failed to compile macOS screen capture library"
        );
        let object = output.with_file_name("VirtualDisplay.o");
        let compiled = std::process::Command::new("xcrun")
            .args(["clang", "-O2", "-fobjc-arc", "-fblocks", "-mmacosx-version-min=13.0", "-arch", arch,
                   "-c", "native/VirtualDisplay.m", "-o"])
            .arg(&object).status().expect("cannot compile virtual display support");
        assert!(compiled.success(), "failed to compile virtual display support");
        let archived = std::process::Command::new("xcrun")
            .args(["ar", "-q"]).arg(&output).arg(&object).status()
            .expect("cannot archive virtual display support");
        assert!(archived.success(), "failed to archive virtual display support");
        let indexed = std::process::Command::new("xcrun")
            .arg("ranlib").arg(&output).status().expect("cannot index capture library");
        assert!(indexed.success(), "failed to index capture library");
        println!(
            "cargo:rustc-link-search=native={}",
            output.parent().unwrap().display()
        );
        println!("cargo:rustc-link-lib=static=yiterm_capture");
        // Swift object files carry framework/runtime autolink directives. Supply
        // the toolchain search path for compatibility libraries and system Swift.
        let compiler = std::process::Command::new("xcrun")
            .args(["--find", "swiftc"])
            .output()
            .expect("cannot locate swiftc");
        assert!(compiler.status.success(), "cannot locate swiftc");
        let compiler = std::path::PathBuf::from(String::from_utf8(compiler.stdout).unwrap().trim());
        let runtime = compiler
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("lib/swift/macosx");
        println!("cargo:rustc-link-search=native={}", runtime.display());
        println!("cargo:rustc-link-search=native=/usr/lib/swift");
        println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/lib/swift");
    }
    println!("cargo:rerun-if-changed=assets/icons/yiterm.rc");
    println!("cargo:rerun-if-changed=assets/icons/yiterm.ico");
    // GPUI loads the application's Windows icon from resource ID 1.
    #[cfg(windows)]
    embed_resource::compile_for("assets/icons/yiterm.rc", ["yiterm"], embed_resource::NONE)
        .manifest_required()
        .expect("failed to embed YiTerm application icon");
}
