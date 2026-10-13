//! Shared native-library discovery for the ML and desktop build scripts.
use std::{
    env,
    path::{Path, PathBuf},
    process::Command,
};

/// Target-specific LightGBM filename.
pub fn lightgbm_filename(target_os: &str) -> &'static str {
    match target_os {
        "macos" => "lib_lightgbm.dylib",
        "windows" => "lib_lightgbm.dll",
        _ => "lib_lightgbm.so",
    }
}

/// Validates an explicit directory; empty environment values mean unset.
pub fn configured_dir(
    value: Option<&std::ffi::OsStr>,
    filename: &str,
) -> Result<Option<PathBuf>, String> {
    let Some(value) = value.filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    let path = PathBuf::from(value);
    if !path.join(filename).is_file() {
        return Err(format!("{} does not contain {filename}", path.display()));
    }
    path.canonicalize()
        .map(Some)
        .map_err(|error| error.to_string())
}

/// Uses the same explicit / brew / standard / user-prefix search in both crates.
pub fn lightgbm_dir(target_os: &str) -> Option<PathBuf> {
    let filename = lightgbm_filename(target_os);
    if let Some(dir) = configured_dir(env::var_os("LIGHTGBM_LIB_DIR").as_deref(), filename)
        .unwrap_or_else(|error| panic!("invalid LIGHTGBM_LIB_DIR: {error}"))
    {
        return Some(dir);
    }
    let brewed = Command::new("brew")
        .args(["--prefix", "lightgbm"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| PathBuf::from(String::from_utf8_lossy(&output.stdout).trim()).join("lib"));
    let home = env::var_os("HOME").map(PathBuf::from);
    candidates(brewed.as_deref(), home.as_deref())
        .into_iter()
        .find(|path| path.join(filename).is_file())
        .and_then(|path| path.canonicalize().ok())
}

fn candidates(brewed: Option<&Path>, home: Option<&Path>) -> Vec<PathBuf> {
    let mut paths: Vec<_> = brewed.into_iter().map(Path::to_path_buf).collect();
    paths.extend([
        PathBuf::from("/opt/homebrew/opt/lightgbm/lib"),
        PathBuf::from("/usr/local/opt/lightgbm/lib"),
    ]);
    if let Some(home) = home {
        paths.push(home.join("homebrew/opt/lightgbm/lib"));
        paths.push(home.join(".linuxbrew/opt/lightgbm/lib"));
    }
    paths
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_paths_reject_wrong_library_and_accept_empty_or_valid_configuration() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            configured_dir(Some(std::ffi::OsStr::new("")), "libduckdb.dylib").unwrap(),
            None
        );
        assert!(configured_dir(Some(dir.path().as_os_str()), "libduckdb.dylib").is_err());
        std::fs::write(dir.path().join("libduckdb.dylib"), []).unwrap();
        assert_eq!(
            configured_dir(Some(dir.path().as_os_str()), "libduckdb.dylib").unwrap(),
            Some(dir.path().canonicalize().unwrap())
        );
    }
    #[test]
    fn custom_homebrew_prefix_precedes_generic_locations_without_personal_paths() {
        let paths = candidates(
            Some(Path::new("/custom/brew/lib")),
            Some(Path::new("/test/user")),
        );
        assert_eq!(paths[0], Path::new("/custom/brew/lib"));
        assert!(paths.contains(&PathBuf::from("/test/user/homebrew/opt/lightgbm/lib")));
    }
}
