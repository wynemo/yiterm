fn main() {
    println!("cargo:rerun-if-changed=assets/icons/yiterm.rc");
    println!("cargo:rerun-if-changed=assets/icons/yiterm.ico");
    // GPUI loads the application's Windows icon from resource ID 1.
    #[cfg(windows)]
    embed_resource::compile_for("assets/icons/yiterm.rc", ["yiterm"], embed_resource::NONE)
        .manifest_required()
        .expect("failed to embed YiTerm application icon");
}
