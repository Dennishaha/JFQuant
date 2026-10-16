//! Named, reusable subsets of immutable factor families.
#![allow(missing_docs)]
use crate::{Result, atomic_json, profiles::FactorProfile};
use analytics_core::research_quality::FactorSet;
use analytics_factor::{FactorExpr, FactorRegistry};
use serde::{Deserialize, Serialize};
use std::{fs, path::Path};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FactorGroup {
    pub schema_version: u32,
    pub id: String,
    pub name: String,
    pub description: String,
    pub profile: FactorProfile,
}
impl FactorGroup {
    pub fn builtin(set: FactorSet) -> Self {
        let mut t = crate::ResearchTemplate::default();
        t.select_factor_set(set);
        Self {
            schema_version: 1,
            id: format!("builtin:{}", set.label()),
            name: set.label().into(),
            description: "内置只读组；GTJA191保留完整目录，默认执行178项。".into(),
            profile: FactorProfile::from_template(&t),
        }
    }
    pub fn is_builtin(&self) -> bool {
        self.id.starts_with("builtin:")
    }
    pub fn duplicate(&self) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            name: format!("{} 副本", self.name),
            ..self.clone()
        }
    }
    pub fn validate(&self) -> Result<()> {
        if !matches!(self.schema_version, 1 | 2) || self.name.trim().is_empty() {
            return Err("因子组格式或名称无效".into());
        }
        self.profile
            .apply(&mut crate::ResearchTemplate::default())?;
        let registry = FactorRegistry::resolve(
            self.profile.factor_set,
            &self.profile.factor_refs,
            &self.profile.factor_ids,
        )?;
        for id in &self.profile.factor_ids {
            if registry
                .get(id)
                .is_none_or(|s| matches!(s.expression, FactorExpr::Unsupported { .. }))
            {
                return Err(format!("不可执行因子：{id}").into());
            }
        }
        Ok(())
    }
    pub fn save(&self, root: &Path) -> Result<()> {
        self.validate()?;
        let id = Uuid::parse_str(&self.id).map_err(|_| "内置因子组不可修改；请先复制")?;
        let mut current = self.clone();
        current.schema_version = 2;
        current.profile.schema_version = 2;
        current.profile.factor_refs = analytics_factor::selection::frozen_refs(
            self.profile.factor_set,
            &self.profile.factor_refs,
            &self.profile.factor_ids,
        );
        atomic_json(
            &root.join(format!(".jfquant/factor-groups/{id}.json")),
            &current,
        )
    }
    pub fn delete(&self, root: &Path) -> Result<()> {
        let id = Uuid::parse_str(&self.id).map_err(|_| "内置因子组不可删除")?;
        let path = root.join(format!(".jfquant/factor-groups/{id}.json"));
        let archive = root.join(".jfquant/factor-groups/archived");
        fs::create_dir_all(&archive)?;
        fs::rename(path, archive.join(format!("{id}.json")))?;
        Ok(())
    }
}
pub fn list(root: &Path) -> Result<Vec<FactorGroup>> {
    let mut groups = vec![
        FactorGroup::builtin(FactorSet::Alpha158),
        FactorGroup::builtin(FactorSet::Gtja191),
    ];
    let dir = root.join(".jfquant/factor-groups");
    if dir.exists() {
        let mut paths = fs::read_dir(dir)?
            .map(|e| e.map(|e| e.path()))
            .collect::<std::io::Result<Vec<_>>>()?;
        paths.sort();
        for path in paths {
            if path.extension().is_none_or(|s| s != "json") {
                continue;
            }
            let g: FactorGroup = serde_json::from_slice(&fs::read(&path)?)?;
            g.validate()?;
            let id = Uuid::parse_str(&g.id)?;
            if path.file_stem().and_then(|v| v.to_str()) != Some(id.to_string().as_str()) {
                return Err("因子组ID与文件名不一致".into());
            }
            groups.push(g);
        }
    }
    Ok(groups)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn originals_are_immutable_and_custom_groups_roundtrip() {
        let d = tempfile::tempdir().unwrap();
        let original = FactorGroup::builtin(FactorSet::Alpha158);
        assert!(original.save(d.path()).is_err());
        assert!(original.delete(d.path()).is_err());
        let mut custom = original.duplicate();
        custom.profile.factor_ids.truncate(2);
        custom.save(d.path()).unwrap();
        assert_eq!(
            list(d.path()).unwrap()[2].profile.factor_ids,
            custom.profile.factor_ids
        );
        custom.name = "精选".into();
        custom.save(d.path()).unwrap();
        assert_eq!(list(d.path()).unwrap().len(), 3);
        custom.delete(d.path()).unwrap();
        assert_eq!(list(d.path()).unwrap().len(), 2);
        custom.profile.factor_ids.clear();
        assert!(custom.save(d.path()).is_err());
    }
}
