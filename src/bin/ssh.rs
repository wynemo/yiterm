//! Dedicated console entry point for YiTerm's Windows SSH client.
#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    yiterm::ssh::run_wrapper()
}

// Unix retains its existing OpenSSH exec integration.
#[cfg(not(windows))]
fn main() {
    eprintln!("This helper is only used on Windows");
    std::process::exit(1);
}
