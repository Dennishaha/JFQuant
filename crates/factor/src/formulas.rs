//! Shared compiled-in catalogue format; semantic versions remain independent.
use serde::Deserialize;
#[derive(Deserialize)]
pub(crate) struct Catalogue {
    pub schema_version: u32,
    pub formula_version: String,
    pub features: Vec<Definition>,
}
#[derive(Deserialize)]
pub(crate) struct Definition {
    pub id: String,
    pub name: String,
    pub formula: String,
    pub syntax: String,
    pub expression: Option<super::alpha158::Expr>,
    /// Explicit reviewed execution text; `formula` always preserves the source.
    #[serde(deserialize_with = "required_option")]
    pub execution_formula: Option<String>,
}
pub(crate) fn required_option<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::deserialize(deserializer)
}
pub(crate) fn catalogue(source: &str, version: &str) -> Catalogue {
    let c: Catalogue = serde_json::from_str(source).expect("compiled builtin catalogue");
    assert_eq!(c.schema_version, 2);
    assert_eq!(c.formula_version, version);
    c
}
pub(crate) fn gtja() -> Catalogue {
    catalogue(
        include_str!("../catalogs/gtja191.json"),
        super::FORMULA_VERSION,
    )
}
