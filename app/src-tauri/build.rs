fn main() {
    // When the WinFsp virtual-drive feature is on (Windows/MSVC), emit the
    // delay-load flags for the FINAL binary so winfsp-x64.dll is resolved at
    // runtime (via winfsp_init) instead of at process start. These `link-arg`s
    // don't propagate from winfsp-sys, so the linked crate must re-emit them.
    if std::env::var_os("CARGO_FEATURE_VFS_WINFSP").is_some()
        && std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows")
        && std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc")
    {
        let dll = match std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
            Ok("x86_64") => Some("winfsp-x64.dll"),
            Ok("x86") => Some("winfsp-x86.dll"),
            Ok("aarch64") => Some("winfsp-a64.dll"),
            _ => None,
        };
        if let Some(dll) = dll {
            println!("cargo:rustc-link-lib=dylib=delayimp");
            println!("cargo:rustc-link-arg=/DELAYLOAD:{dll}");
        }
    }
    tauri_build::build()
}
