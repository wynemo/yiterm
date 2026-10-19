# YiTerm GPUI patch

Base: crates.io `gpui` 0.2.2 (Apache-2.0).

`src/platform/windows/directx_renderer.rs` creates the window's DirectComposition
target with `topmost = false`. GPUI's opaque swap chain must be below native child
HWNDs so the embedded WebView2 is visible. Upstream uses `true`, which covers the
browser even after its DOM and network requests have loaded successfully.

Keep this change when updating the vendored crate. The Windows browser CI includes
an on-screen pixel check using a real GPUI window and a colored WebView2 child.
