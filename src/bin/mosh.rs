//! Dedicated console entry point for YiTerm's native Windows Mosh client.
#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    yiterm::ssh::run_mosh_wrapper()
}

#[cfg(not(windows))]
fn main() {
    eprintln!("This helper is only used on Windows; install the system Mosh client");
    std::process::exit(1);
}
