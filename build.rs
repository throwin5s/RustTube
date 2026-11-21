fn main() {
    // libmpv is what makes embedded video possible: one code path on macOS and
    // Windows, rather than platform-specific child-window embedding.
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=LIBMPV_DIR");

    // The macOS build can ask AppKit for the real SF Symbols at runtime. The
    // helper is deliberately absent from Windows, where Apple does not license
    // the symbols.
    println!("cargo:rerun-if-changed=src/sf_symbols.m");
    println!("cargo:rerun-if-changed=src/mac_window.m");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        cc::Build::new()
            .file("src/sf_symbols.m")
            .file("src/mac_window.m")
            .flag("-fobjc-arc")
            .compile("rusttube_sf_symbols");
        println!("cargo:rustc-link-lib=framework=AppKit");
        println!("cargo:rustc-link-lib=framework=Foundation");
    }

    // Release builds unpack a prebuilt libmpv and point this at its import
    // library; pkg-config is not available on the Windows runner.
    if let Ok(dir) = std::env::var("LIBMPV_DIR") {
        let dir = dir.trim();
        if !dir.is_empty() {
            println!("cargo:rustc-link-search=native={dir}");
            println!("cargo:rustc-link-lib=dylib=mpv");
            return;
        }
    }

    if pkg_config::Config::new().probe("mpv").is_ok() {
        return;
    }

    println!("cargo:rustc-link-lib=dylib=mpv");
    for path in ["/opt/homebrew/lib", "/usr/local/lib"] {
        if std::path::Path::new(path).join("libmpv.dylib").exists() {
            println!("cargo:rustc-link-search=native={path}");
        }
    }
}
