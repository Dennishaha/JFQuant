//! Cargo build-path helpers, independent of cache contents or target-dir env.
use std::path::{Path, PathBuf};

/// Derives the actual host/cross-target/profile deps directory from Cargo OUT_DIR.
pub fn profile_deps(out: &Path) -> Option<PathBuf> {
    if out.file_name()? != "out" || out.parent()?.parent()?.file_name()? != "build" {
        return None;
    }
    Some(out.parent()?.parent()?.parent()?.join("deps"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn build_paths_use_current_target_and_profile_without_scanning_old_caches() {
        for root in [
            "/custom/target/debug",
            "/custom/target/aarch64-apple-darwin/release",
            "/custom/target/x86_64-unknown-linux-gnu/test-profile",
        ] {
            assert_eq!(
                profile_deps(&Path::new(root).join("build/analytics-desktop-hash/out")),
                Some(Path::new(root).join("deps"))
            );
        }
        assert_eq!(
            profile_deps(Path::new(
                "/custom/target/duckdb-download/x86_64-apple-darwin/old"
            )),
            None
        );
    }
}
