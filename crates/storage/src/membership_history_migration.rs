//! Explicit one-time v1 draft conversion. Never invoked by current readers.
use crate::membership_history::{Request, invalid};
use serde_json::{Value, json};
use std::io;
/// Convert only the declared v1 request. Missing proofs remain empty and require review.
/// The caller writes a new draft, preserving the original and old task records.
pub fn migrate_v1(mut value: Value) -> io::Result<Request> {
    if value["schema_version"] != 1 || value["history"]["schema_version"] != 1 {
        return Err(invalid("migration requires explicit history request v1"));
    }
    fn references(v: &mut Value) {
        match v {
            Value::Object(map) => {
                if map.contains_key("document") && map.contains_key("excerpt") {
                    map.insert("transcription".into(), Value::Null);
                }
                for child in map.values_mut() {
                    references(child);
                }
            }
            Value::Array(items) => {
                for child in items {
                    references(child);
                }
            }
            _ => {}
        }
    }
    references(&mut value);
    value["schema_version"] = json!(2);
    value["history"]["schema_version"] = json!(2);
    value["transcriptions"] = json!([]);
    for d in value["documents"]
        .as_array_mut()
        .ok_or_else(|| invalid("documents missing"))?
    {
        d["derived_from"] = Value::Null;
    }
    for a in value["history"]["anchors"]
        .as_array_mut()
        .ok_or_else(|| invalid("anchors missing"))?
    {
        a["revision"] = json!(1);
        a["supersedes"] = Value::Null;
        a["correction"] = json!([]);
        a["securities"] = json!([]);
    }
    for e in value["history"]["events"]
        .as_array_mut()
        .ok_or_else(|| invalid("events missing"))?
    {
        e["correction"] = json!([]);
        e["securities"] = json!([]);
    }
    for r in value["history"]["reviews"]
        .as_array_mut()
        .ok_or_else(|| invalid("reviews missing"))?
    {
        r["outcome"] = json!({"outcome":"coverage"});
    }
    serde_json::from_value(value).map_err(invalid)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn migration_preserves_facts_but_does_not_invent_proofs_or_approval() {
        let input = json!({"schema_version":1,"history":{"schema_version":1,"official_code":"746059","index_name":"Index","identity":[],"requested":{"start":"2021-09-17","end":"2021-09-17"},"anchors":[{"id":"a","at":{"date":"2021-09-17","boundary":"unspecified"},"known_at":null,"members":["SH600000"],"citations":[{"document":"source","excerpt":"Member"}]}],"events":[],"reviews":[]},"documents":[]});
        let r = migrate_v1(input.clone()).unwrap();
        assert_eq!(input["schema_version"], 1);
        assert!(r.transcriptions.is_empty());
        assert!(r.history.anchors[0].securities.is_empty());
        assert!(r.history.anchors[0].known_at.is_none());
        assert_eq!(r.schema_version, 2);
        assert!(migrate_v1(serde_json::to_value(r).unwrap()).is_err());
        assert!(serde_json::from_value::<Request>(input).is_err());
    }
}
