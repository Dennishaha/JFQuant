//! Workspace data admission and artifact interpretation.
use super::*;
use crate::RunStatus;
impl Workspace {
    /// Register a current-format strict research result as read-only.
    pub fn import_run(&self, path: &Path) -> Result<RunRecord> {
        let summary: Value = serde_json::from_slice(&fs::read(path.join("summary.json"))?)?;
        crate::research::backtest::validate_result(&summary)?;
        let mut run = self.begin(
            Uuid::new_v4(),
            "历史研究（只读）",
            json!({"source":path,"summary":summary}),
        )?;
        run.read_only = true;
        self.finish(&mut run, RunStatus::Imported, None)?;
        Ok(run)
    }
    /// Load saved artifacts, rejecting unsupported research result formats.
    pub fn results(&self, run: &RunRecord) -> Result<Value> {
        if let Some((result, _)) = crate::storage_management::historical_view(self, run.id)? {
            return Ok(result);
        }
        if run.request["request"]["operation"] == "storage" {
            let outcome: crate::WorkflowOutcome =
                serde_json::from_slice(&fs::read(self.resolve(&run.output).join("outcome.json"))?)?;
            return Ok(outcome.details);
        }
        if run.read_only {
            crate::research::backtest::validate_result(&run.request["summary"])?;
            return Ok(run.request["summary"].clone());
        }
        let dir = self.resolve(&run.output);
        let workbench = dir.join("artifacts.json");
        if workbench.exists() {
            return Ok(serde_json::from_slice(&fs::read(workbench)?)?);
        }
        if dir.join("input-check.json").is_file()
            && dir.join("outcome.json").is_file()
            && !dir.join("training.json").exists()
            && !dir.join("summary.json").exists()
        {
            return Ok(serde_json::from_slice(&fs::read(
                dir.join("outcome.json"),
            )?)?);
        }
        let training = dir.join("training.json");
        let training_value = if training.exists() {
            {
                let artifact = serde_json::from_slice::<Value>(&fs::read(&training)?)?;
                if artifact["schema_version"] == 5 {
                    crate::research::backtest::independent::catalogue(&dir)?;
                } else {
                    crate::research::backtest::validate_training_artifact(&artifact)?;
                }
                Some(artifact)
            }
        } else {
            None
        };
        let p = dir.join("summary.json");
        if p.exists() {
            let summary = serde_json::from_slice(&fs::read(p)?)?;
            crate::research::backtest::validate_result(&summary)?;
            Ok(summary)
        } else {
            if let Some(artifact) = training_value.as_ref()
                && artifact["schema_version"] == 5
            {
                return Ok(artifact.clone());
            }
            if matches!(
                run.request["request"]["operation"].as_str(),
                Some("train_research")
            ) && let Some(artifact) = training_value.as_ref()
            {
                return Ok(artifact.clone());
            }
            let p = dir.join("execution_readiness.json");
            if p.exists() {
                let mut readiness: Value = serde_json::from_slice(&fs::read(p)?)?;
                if let Some(t) = &training_value {
                    readiness["training_artifact"] = t.clone();
                }
                let prediction = dir.join("prediction_manifest.json");
                if prediction.is_file() {
                    readiness["prediction_artifact"] =
                        serde_json::from_slice(&fs::read(prediction)?)?;
                }
                Ok(readiness)
            } else {
                let payload = dir.join("payload.json");
                if payload.exists() {
                    return Ok(serde_json::from_slice(&fs::read(payload)?)?);
                }
                let p = dir.join("outcome.json");
                if p.exists() {
                    Ok(serde_json::from_slice(&fs::read(p)?)?)
                } else {
                    Ok(training_value.unwrap_or(Value::Null))
                }
            }
        }
    }
}
