fn main() {
    // libmpv is what makes embedded video possible: one code path on macOS and
    // Windows, rather than platform-specific child-window embedding.
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=LIBMPV_DIR");

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
