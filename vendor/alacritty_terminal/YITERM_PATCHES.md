# YiTerm local patch

Based on crates.io `alacritty_terminal` 0.26.0 (Apache-2.0). Source, normalized Cargo manifest, upstream README, and license are retained. The upstream reference-fixture test target is omitted because its external fixture corpus is not vendored.

`src/term/mod.rs`: promote narrow text symbols followed by VS16 to two terminal columns when `unicode-width` recognizes the emoji presentation sequence. Preserve the original selector, create a wide spacer, and reuse terminal input/wrapping to handle insertion, overwrite, and row edges. This works across PTY read boundaries.

Application regression tests: `cargo test --locked --bin yiterm terminal_unicode_` (also run by the macOS and Windows CI jobs).

Upstream unit tests: `cargo test --manifest-path vendor/alacritty_terminal/Cargo.toml --lib --target-dir target`.
