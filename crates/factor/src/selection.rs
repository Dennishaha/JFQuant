//! Frozen, ordered references to immutable builtin definitions and custom revisions.
#![allow(missing_docs)]
use crate::{
    FactorError, FactorExpr, FactorRegistry, FactorSpec, Result,
    operators::{CompiledExpression, compile_formula},
};
use analytics_core::research_quality::FactorSet;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CustomFactorDefinition {
    pub schema_version: u32,
    pub id: String,
    pub name: String,
    pub description: String,
    pub formula: String,
    pub revision: u32,
    pub compiled: CompiledExpression,
    pub definition_hash: String,
}
fn invalid(s: impl Into<String>) -> FactorError {
    FactorError::InvalidRequest(s.into())
}
pub fn hash(value: &impl Serialize) -> Result<String> {
    let bytes = serde_json::to_vec(value).map_err(|e| invalid(e.to_string()))?;
    Ok(format!(
        "sha256:{:x}",
        analytics_core::hash::Hex(Sha256::digest(bytes))
    ))
}
impl CustomFactorDefinition {
    pub fn new(
        id: String,
        name: String,
        description: String,
        formula: String,
        revision: u32,
    ) -> Result<Self> {
        let compiled = compile_formula(&formula).map_err(|e| invalid(e.to_string()))?;
        let mut d = Self {
            schema_version: 1,
            id,
            name,
            description,
            formula,
            revision,
            compiled,
            definition_hash: String::new(),
        };
        d.definition_hash = d.expected_hash()?;
        d.validate()?;
        Ok(d)
    }
    fn expected_hash(&self) -> Result<String> {
        hash(&(
            self.schema_version,
            &self.id,
            &self.name,
            &self.description,
            &self.formula,
            self.revision,
            &self.compiled,
        ))
    }
    pub fn validate(&self) -> Result<()> {
        if self.schema_version != 1
            || self.revision == 0
            || self.name.trim().is_empty()
            || !self.id.starts_with("custom_")
            || self.id.len() > 96
            || !self
                .id
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'_')
        {
            return Err(invalid("invalid custom factor identity/revision"));
        }
        let compiled = compile_formula(&self.formula).map_err(|e| invalid(e.to_string()))?;
        if serde_json::to_value(&compiled).unwrap() != serde_json::to_value(&self.compiled).unwrap()
            || self.definition_hash != self.expected_hash()?
        {
            return Err(invalid(
                "custom factor definition/AST/operator version was changed",
            ));
        }
        Ok(())
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "source", rename_all = "snake_case", deny_unknown_fields)]
pub enum FactorRef {
    Builtin {
        factor_set: FactorSet,
        id: String,
        formula_version: String,
    },
    Custom {
        definition: Box<CustomFactorDefinition>,
    },
}
impl FactorRef {
    pub fn builtin(set: FactorSet, id: impl Into<String>) -> Self {
        Self::Builtin {
            factor_set: set,
            id: id.into(),
            formula_version: crate::alpha158::version(set).into(),
        }
    }
    pub fn id(&self) -> &str {
        match self {
            Self::Builtin { id, .. } => id,
            Self::Custom { definition } => &definition.id,
        }
    }
    pub fn spec(&self) -> Result<FactorSpec> {
        match self {
            Self::Builtin {
                factor_set,
                id,
                formula_version,
            } => {
                let registry = FactorRegistry::for_set(*factor_set);
                let spec = registry
                    .get(id)
                    .ok_or_else(|| invalid(format!("unknown builtin factor {id}")))?;
                if &spec.version != formula_version
                    || matches!(spec.expression, FactorExpr::Unsupported { .. })
                {
                    return Err(invalid(format!("unsupported builtin factor/version {id}")));
                }
                Ok(spec.clone())
            }
            Self::Custom { definition } => {
                definition.validate()?;
                Ok(FactorSpec {
                    id: definition.id.clone(),
                    expression: FactorExpr::Unified(Box::new(
                        definition.compiled.expression.clone(),
                    )),
                    lookback: definition.compiled.lookback,
                    requires_recursive_state: definition.compiled.requires_recursive_state,
                    input_columns: definition.compiled.inputs.clone(),
                    version: definition.definition_hash.clone(),
                    formula: definition.formula.clone(),
                })
            }
        }
    }
}
impl FactorRegistry {
    pub fn resolve(set: FactorSet, refs: &[FactorRef], ids: &[String]) -> Result<Self> {
        if refs.is_empty() {
            return Ok(Self::for_set(set));
        }
        if refs.len() != ids.len() || refs.iter().zip(ids).any(|(r, id)| r.id() != id) {
            return Err(invalid("factor references and feature order differ"));
        }
        let mut specs = BTreeMap::new();
        let (mut gtja, mut qlib) = (None, None);
        for r in refs {
            let s = match r {
                FactorRef::Builtin {
                    factor_set,
                    id,
                    formula_version,
                } => {
                    let cached = match factor_set {
                        FactorSet::Gtja191 => &mut gtja,
                        FactorSet::Alpha158 => &mut qlib,
                    };
                    let registry = cached.get_or_insert_with(|| Self::for_set(*factor_set));
                    let spec = registry
                        .get(id)
                        .ok_or_else(|| invalid(format!("unknown builtin factor {id}")))?;
                    if &spec.version != formula_version
                        || matches!(spec.expression, FactorExpr::Unsupported { .. })
                    {
                        return Err(invalid(format!("unsupported builtin factor/version {id}")));
                    }
                    spec.clone()
                }
                _ => r.spec()?,
            };
            if specs.insert(s.id.clone(), s).is_some() {
                return Err(invalid("duplicate factor references"));
            }
        }
        Ok(Self { specs })
    }
    /// Full ordered source, AST, input and operator-version receipts.
    pub fn manifest_definitions(&self, ids: &[String]) -> Result<serde_json::Value> {
        let mut out = Vec::new();
        for id in ids {
            let spec = self
                .get(id)
                .ok_or_else(|| invalid(format!("unknown factor {id}")))?;
            let compiled =
                crate::operators::Expression::from_legacy(&spec.expression)?.compile()?;
            out.push(serde_json::json!({"spec":spec,"compiled":compiled}));
        }
        Ok(serde_json::Value::Array(out))
    }
    /// Preserve legacy single-family versions; mixed/custom selections include ordered definitions.
    pub fn selection_version(&self, ids: &[String]) -> Result<String> {
        let selected = ids
            .iter()
            .map(|id| {
                self.get(id)
                    .ok_or_else(|| invalid(format!("unknown factor {id}")))
            })
            .collect::<Result<Vec<_>>>()?;
        let versions: BTreeSet<_> = selected.iter().map(|s| s.version.as_str()).collect();
        if versions.len() == 1 {
            return Ok(versions.first().unwrap().to_string());
        }
        Ok(format!("mixed-v1:{}", hash(&selected)?))
    }
}
pub fn frozen_refs(set: FactorSet, refs: &[FactorRef], ids: &[String]) -> Vec<FactorRef> {
    if refs.is_empty() {
        ids.iter()
            .map(|id| FactorRef::builtin(set, id.clone()))
            .collect()
    } else {
        refs.to_vec()
    }
}
pub fn warmup_sessions(set: FactorSet, refs: &[FactorRef]) -> usize {
    if refs.is_empty() {
        return set.warmup_sessions();
    }
    refs.iter()
        .map(|r| match r {
            FactorRef::Builtin { factor_set, .. } => factor_set.warmup_sessions(),
            FactorRef::Custom { definition } => (definition.compiled.lookback as usize).max(
                if definition.compiled.requires_recursive_state {
                    756
                } else {
                    1
                },
            ),
        })
        .max()
        .unwrap_or(1)
}
