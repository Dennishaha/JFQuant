//! Separate reusable workflow, feature and target files; tasks freeze a resolved template.
//! Why: changing an index must not duplicate or silently reset a reviewed feature order.
use crate::research::backtest::ResearchConfig;
use crate::*;
use analytics_core::market_rules::MarketRulesPreset;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    fs,
    path::{Path, PathBuf},
};

use analytics_core::research_quality::{FactorSet, PriceBasis};

/// Standalone feature selection; no dates, universe or account parameters.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(missing_docs)]
pub struct FactorProfile {
    pub schema_version: u32,
    pub factor_set: FactorSet,
    pub formula_version: String,
    pub price_basis: PriceBasis,
    pub factor_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub factor_refs: Vec<analytics_factor::selection::FactorRef>,
}
/// Standalone instrument selection; dynamic constituents remain an immutable pool version.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(missing_docs)]
pub struct TargetProfile {
    pub schema_version: u32,
    pub pool_version: String,
    pub benchmark: String,
    pub pool_exit_policy: analytics_core::pool::PoolExitPolicy,
}
/// Workflow-only parameters, without embedded feature IDs or target definitions.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(missing_docs)]
pub struct WorkflowDocument {
    /// Explicit allocation settings.
    pub allocation: crate::allocation::AllocationConfig,
    pub schema_version: u32,
    pub id: String,
    pub name: String,
    pub factors: PathBuf,
    pub target: PathBuf,
    pub schedule: analytics_core::schedule::Schedule,
    pub action_policy: analytics_core::actions::ActionPolicy,
    pub event_check_policy: analytics_core::execution::EventCheckPolicy,
    pub quality_policy: analytics_core::research_quality::ResearchQualityPolicy,
    pub top_n: usize,
    pub rebalance_days: usize,
    pub training: crate::research::TrainingConfig,
    pub market_rules: MarketRulesPreset,
    pub initial_cash_cny: f64,
    pub dividend_withholding_rate: f64,
}
impl FactorProfile {
    /// Capture current ordered features and their source contract.
    pub fn from_template(t: &ResearchTemplate) -> Self {
        Self {
            schema_version: if t.config.factor_refs.is_empty() {
                1
            } else {
                2
            },
            factor_set: t.factor_set,
            formula_version: t.formula_version.clone(),
            price_basis: t.price_basis,
            factor_ids: t.config.factor_ids.clone(),
            factor_refs: t.config.factor_refs.clone(),
        }
    }
    /// Apply and validate with the common template rules.
    pub fn apply(&self, t: &mut ResearchTemplate) -> Result<()> {
        if !matches!(self.schema_version, 1 | 2) {
            return Err("unsupported_factor_profile_format".into());
        }
        let mut candidate = t.clone();
        candidate.factor_set = self.factor_set;
        candidate.formula_version = self.formula_version.clone();
        candidate.price_basis = self.price_basis;
        candidate.config.factor_ids = self.factor_ids.clone();
        candidate.config.factor_refs = self.factor_refs.clone();
        candidate.validate()?;
        *t = candidate;
        Ok(())
    }
    /// Load a data-only profile; no expressions or code are executed.
    pub fn read(path: &Path) -> Result<Self> {
        let p: Self = serde_json::from_slice(&fs::read(path)?)?;
        p.apply(&mut ResearchTemplate::default())?;
        Ok(p)
    }
    /// Save a validated reusable profile atomically.
    pub fn write(&self, path: &Path) -> Result<()> {
        self.apply(&mut ResearchTemplate::default())?;
        let mut current = self.clone();
        current.schema_version = 2;
        atomic_json(path, &current)
    }
}
impl TargetProfile {
    /// Capture the selected pool, benchmark and exit policy independently of factors.
    pub fn from_template(t: &ResearchTemplate) -> Self {
        Self {
            schema_version: 1,
            pool_version: t.pool_version.clone(),
            benchmark: t.benchmark.clone(),
            pool_exit_policy: t.pool_exit_policy,
        }
    }
    /// Apply only the target fields, leaving reviewed factors unchanged.
    pub fn apply(&self, t: &mut ResearchTemplate) -> Result<()> {
        if self.schema_version != 1 {
            return Err("unsupported_target_profile_format".into());
        }
        let mut candidate = t.clone();
        candidate.pool_version = self.pool_version.clone();
        candidate.benchmark = self.benchmark.clone();
        candidate.pool_exit_policy = self.pool_exit_policy;
        candidate.validate()?;
        *t = candidate;
        Ok(())
    }
    /// Load a target profile. Actual pool coverage is checked against selected data at execution.
    pub fn read(path: &Path) -> Result<Self> {
        let p: Self = serde_json::from_slice(&fs::read(path)?)?;
        p.apply(&mut ResearchTemplate::default())?;
        Ok(p)
    }
    /// Save a target selection atomically without copying its constituent history.
    pub fn write(&self, path: &Path) -> Result<()> {
        self.apply(&mut ResearchTemplate::default())?;
        atomic_json(path, self)
    }
}
impl WorkflowDocument {
    /// Resolve shared profiles into the single common request used by App and CLI.
    pub fn resolve(self, f: FactorProfile, u: TargetProfile) -> Result<ResearchTemplate> {
        if self.schema_version != 12 {
            return Err("unsupported_workflow_template_format".into());
        }
        let mut t = ResearchTemplate {
            schema_version: 12,
            id: self.id,
            name: self.name,
            schedule: self.schedule,
            action_policy: self.action_policy,
            event_check_policy: self.event_check_policy,
            quality_policy: self.quality_policy,
            config: ResearchConfig {
                allocation: self.allocation,
                top_n: self.top_n,
                rebalance_days: self.rebalance_days,
                training: self.training,
                ..ResearchConfig::default()
            },
            market_rules: self.market_rules,
            initial_cash_cny: self.initial_cash_cny,
            dividend_withholding_rate: self.dividend_withholding_rate,
            ..ResearchTemplate::default()
        };
        // Populate both selections before validating the resolved task.
        t.factor_set = f.factor_set;
        t.formula_version = f.formula_version;
        t.price_basis = f.price_basis;
        t.config.factor_ids = f.factor_ids;
        t.config.factor_refs = f.factor_refs;
        t.pool_version = u.pool_version;
        t.benchmark = u.benchmark;
        t.pool_exit_policy = u.pool_exit_policy;
        if !matches!(f.schema_version, 1 | 2) || u.schema_version != 1 {
            return Err("unsupported profile format".into());
        }
        t.validate()?;
        Ok(t)
    }
    /// Read only the current split format, resolving references relative to the workflow file.
    pub fn read(path: &Path) -> Result<ResearchTemplate> {
        let d: Self = serde_json::from_slice(&fs::read(path)?)?;
        let parent = path.parent().ok_or("workflow file needs parent")?;
        if d.factors.is_absolute() || d.target.is_absolute() {
            return Err("template profile references must be relative for portability".into());
        }
        let f = FactorProfile::read(&parent.join(&d.factors))?;
        let u = TargetProfile::read(&parent.join(&d.target))?;
        d.resolve(f, u)
    }
    /// Persist profiles before atomically publishing their workflow reference.
    pub fn write(t: &ResearchTemplate, path: &Path) -> Result<()> {
        t.validate()?;
        let mut f = FactorProfile::from_template(t);
        f.schema_version = 2;
        let u = TargetProfile::from_template(t);
        let fp = PathBuf::from(format!("factors/{}.json", &signature(&f)?[7..]));
        let up = PathBuf::from(format!("targets/{}.json", &signature(&u)?[7..]));
        let parent = path.parent().ok_or("workflow file needs parent")?;
        fn persist(path: &Path, value: &impl Serialize) -> Result<()> {
            if path.exists() {
                let existing: Value = serde_json::from_slice(&fs::read(path)?)?;
                if existing != serde_json::to_value(value)? {
                    return Err(
                        "content-addressed profile was modified; save as a new profile".into(),
                    );
                }
                Ok(())
            } else {
                atomic_json(path, value)
            }
        }
        persist(&parent.join(&fp), &f)?;
        persist(&parent.join(&up), &u)?;
        let d = Self {
            schema_version: 12,
            id: t.id.clone(),
            name: t.name.clone(),
            factors: fp,
            target: up,
            schedule: t.schedule.clone(),
            action_policy: t.action_policy.clone(),
            event_check_policy: t.event_check_policy,
            quality_policy: t.quality_policy.clone(),
            allocation: t.config.allocation.clone(),
            top_n: t.config.top_n,
            rebalance_days: t.config.rebalance_days,
            training: t.config.training.clone(),
            market_rules: t.market_rules.clone(),
            initial_cash_cny: t.initial_cash_cny,
            dividend_withholding_rate: t.dividend_withholding_rate,
        };
        atomic_json(path, &d)
    }
}
