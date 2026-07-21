//! Emits the MSVC delay-load linker flags WinFsp needs, but only when the
//! `mount-winfsp` feature is on and we're targeting Windows. Replicated from
//! `winfsp::build::winfsp_link_delayload` so the crate needs no winfsp
//! build-dependency (keeping default builds on non-WinFsp machines clean).
fn main() {
    let feature_on = std::env::var_os("CARGO_FEATURE_MOUNT_WINFSP").is_some();
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if !feature_on || target_os != "windows" {
        return;
    }
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    let dll = match arch.as_str() {
        "x86_64" => "winfsp-x64.dll",
        "x86" => "winfsp-x86.dll",
        "aarch64" => "winfsp-a64.dll",
        _ => return,
    };
    // MSVC toolchain: link the delay-import helper and delay-load the WinFsp DLL
    // so it is resolved at runtime (via FspLoad) rather than at process start.
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        println!("cargo:rustc-link-lib=dylib=delayimp");
        println!("cargo:rustc-link-arg=/DELAYLOAD:{dll}");
    }
}
