//! Read models shared by control requests and background desktop readers.
use crate::{DataVersion, Result, RunRecord, Workspace};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::VecDeque,
    fs,
    io::{BufRead, BufReader},
    path::Path,
};
use uuid::Uuid;

/// Workspace catalog and durable task list; contains no native handles.
#[derive(Debug, Serialize, Deserialize)]
pub struct WorkspaceView {
    /// Verified runtime index versions and builtin seeds.
    pub index_versions: Vec<crate::indices::IndexVersion>,
    /// Verified registered input versions.
    pub datasets: Vec<DataVersion>,
    /// Current active-generation records.
    pub runs: Vec<RunRecord>,
}
/// Immutable display snapshot, loaded off the rendering thread.
#[derive(Debug, Serialize, Deserialize)]
pub struct RunView {
    /// Parsed result or the validation error displayed to the user.
    pub result: std::result::Result<Value, String>,
    /// Last 200 persisted progress records.
    pub logs: Vec<Value>,
    /// Date, strategy NAV and benchmark NAV.
    pub nav: Vec<(String, f64, f64)>,
}
fn workspace(root: &Path) -> Result<Workspace> {
    let root = root.canonicalize()?;
    if !root.join(".jfquant/workspace.json").is_file() {
        return Err("workspace not found".into());
    }
    Ok(Workspace { root })
}
/// Load catalog and records without creating or recovering a workspace.
pub fn overview(root: &Path) -> Result<WorkspaceView> {
    let w = workspace(root)?;
    Ok(WorkspaceView {
        index_versions: w.indices()?,
        datasets: w.datasets()?,
        runs: w.runs()?,
    })
}
/// Validate one result and read chart/log data without mutating task state.
pub fn run(root: &Path, id: Uuid) -> Result<RunView> {
    let w = workspace(root)?;
    let record = w
        .runs()?
        .into_iter()
        .find(|r| r.id == id)
        .ok_or("run not found")?;
    let result = w.results(&record).map_err(|e| e.to_string());
    let dir = w.resolve(&record.output);
    let mut logs = VecDeque::new();
    if let Ok(file) = fs::File::open(dir.join("events.jsonl")) {
        for line in BufReader::new(file).lines() {
            if let Ok(value) = serde_json::from_str(&line?) {
                if logs.len() == 200 {
                    logs.pop_front();
                }
                logs.push_back(value);
            }
        }
    }
    let artifact_dir = if record.read_only {
        record.request["source"]
            .as_str()
            .map(std::path::PathBuf::from)
    } else {
        Some(dir)
    };
    let nav = artifact_dir
        .filter(|_| result.is_ok())
        .and_then(|p| fs::read_to_string(p.join("nav.csv")).ok())
        .map(|text| {
            text.lines()
                .skip(1)
                .filter_map(|line| {
                    let c: Vec<_> = line.split(',').collect();
                    if c.len() != 3 {
                        return None;
                    }
                    let a = c[1].parse::<f64>().ok()?;
                    let b = c[2].parse::<f64>().ok()?;
                    (a.is_finite() && b.is_finite()).then(|| (c[0].to_owned(), a, b))
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(RunView {
        result,
        logs: logs.into_iter().collect(),
        nav,
    })
}
