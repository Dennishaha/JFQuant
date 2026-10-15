//! Shared training configuration.
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
/// LightGBM hyperparameters used by pipeline and cache retraining runs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TrainingConfig {
    /// Official Python LightGBM parameters; no executable expressions.
    pub parameters: BTreeMap<String, String>,
    /// Early-stopping patience on the validation split; `None` disables it.
    pub early_stopping_rounds: Option<u32>,
}

impl Default for TrainingConfig {
    fn default() -> Self {
        Self {
            parameters: BTreeMap::from([
                ("num_iterations".into(), "100".into()),
                ("learning_rate".into(), "0.08".into()),
                ("num_leaves".into(), "15".into()),
                ("min_data_in_leaf".into(), "10".into()),
                ("seed".into(), "17".into()),
            ]),
            early_stopping_rounds: Some(8),
        }
    }
}
