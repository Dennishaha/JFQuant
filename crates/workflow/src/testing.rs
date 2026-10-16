//! Test-facing wrapper for the production synthetic demo.
use crate::*;

/// Construct small complete synthetic inputs.
pub fn synthetic(w: &Workspace) -> (DataVersion, DataVersion, ResearchTemplate) {
    crate::demo::create(w).unwrap()
}
