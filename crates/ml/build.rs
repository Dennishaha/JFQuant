fn main() {
    if std::env::var_os("CARGO_FEATURE_LIGHTGBM_NATIVE").is_some() {
        if let Some(dir) = std::env::var_os("LIGHTGBM_LIB_DIR") {
            println!("cargo:rustc-link-search=native={}", dir.to_string_lossy());
        }
        println!("cargo:rustc-link-lib=dylib=_lightgbm");
        println!("cargo:rerun-if-env-changed=LIGHTGBM_LIB_DIR");
    }
}
