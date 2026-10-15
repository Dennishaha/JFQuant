//! DuckDB build path validation.
use std::path::PathBuf;
/// Validate an explicit native library directory.
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
