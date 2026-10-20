//! Reserved producer filenames; unrelated files inside a package keep their own protected owner.
use super::{inventory::Registry, *};
use std::collections::BTreeSet;

fn numbered(name: &str, prefix: &str) -> bool {
    name.strip_prefix(prefix)
        .is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
}
pub(super) fn fold_file(relative: &Path) -> bool {
    let parts: Vec<_> = relative.iter().map(|p| p.to_str().unwrap_or("")).collect();
    matches!(parts.as_slice(), [fold, "model.txt" | "manifest.json" | "metrics.json" | "feature_importance.json"] if numbered(fold, "fold-"))
}
pub(super) fn training(r: &Registry<'_>, root: &Path, name: &str) -> Vec<PathBuf> {
    r.under(&root.join(name))
        .into_iter()
        .filter(|p| {
            let relative = p.strip_prefix(root).unwrap();
            let parts: Vec<_> = relative.iter().map(|p| p.to_str().unwrap_or("")).collect();
            match parts.as_slice() {
                ["sample_masks", file] => file
                    .strip_suffix(".json")
                    .and_then(|n| n.rsplit_once('-'))
                    .is_some_and(|(fold, split)| {
                        numbered(fold, "fold-")
                            && matches!(split, "train" | "validation" | "prediction")
                    }),
                ["factor-cache", file] => [".json", ".bin", ".receipt.json"].iter().any(|suffix| {
                    file.strip_suffix(suffix)
                        .is_some_and(|year| year.len() == 4 && numbered(year, ""))
                }),
                [_] => true,
                _ => false,
            }
        })
        .collect()
}
pub(super) fn market(r: &mut Registry<'_>, root: &Path) -> Result<Vec<PathBuf>> {
    let manifest = root.join("daily_manifest.json");
    let instruments: BTreeSet<String> = if r.contains(&manifest) {
        let value = r.metadata(&manifest)?;
        serde_json::from_value(value["instruments"].clone())?
    } else {
        BTreeSet::new()
    };
    let mut paths: Vec<_> = r
        .under(root)
        .into_iter()
        .filter(|path| {
            let relative = path.strip_prefix(root).unwrap();
            let parts: Vec<_> = relative.iter().map(|p| p.to_str().unwrap_or("")).collect();
            match parts.as_slice() {
                [
                    "daily_manifest.json"
                    | "source_map.json"
                    | "acquisition.json"
                    | "market_view.json"
                    | "market_supplement.json"
                    | "three_factor_returns.json",
                ] => true,
                ["bars", file] => file
                    .strip_suffix(".parquet")
                    .is_some_and(|id| instruments.contains(id)),
                ["adjustments", file] => file
                    .strip_suffix(".json")
                    .is_some_and(|id| instruments.contains(id)),
                ["calendars", "day.txt"] => true,
                ["instruments", file] => file.strip_suffix(".txt").is_some_and(identifier),
                ["features", id, file] => {
                    identifier(id)
                        && file.strip_suffix(".day.bin").is_some_and(|field| {
                            matches!(
                                field,
                                "open"
                                    | "close"
                                    | "high"
                                    | "low"
                                    | "volume"
                                    | "amount"
                                    | "factor"
                                    | "vwap"
                                    | "change"
                            )
                        })
                }
                _ => false,
            }
        })
        .collect();
    for name in [
        "three_factor_build",
        "bigquant_market_factor",
        "bigquant_size_value_factor",
    ] {
        let package = root.join(name);
        let manifest = package.join("manifest.json");
        if !r.contains(&manifest) {
            continue;
        }
        let value = r.metadata(&manifest)?;
        if value["schema_version"] != 1 {
            return Err("storage_unknown_factor_package_manifest".into());
        }
        paths.push(manifest);
        if let Some(files) = value["files"].as_object() {
            for member in files.keys() {
                if !safe_relative(Path::new(member)) {
                    return Err("storage_unsafe_factor_package_member".into());
                }
                paths.push(package.join(member));
            }
        } else if name == "three_factor_build" {
            paths.extend([package.join("inputs.json"), package.join("report.json")]);
            let fundamentals = package.join("fundamentals.json");
            if r.contains(&fundamentals) {
                let value = r.metadata(&fundamentals)?;
                paths.extend(super::inventory::evidence_members(
                    r,
                    &package,
                    "inputs.json",
                    &value,
                )?);
            }
        } else if name == "bigquant_market_factor" {
            paths.extend([
                package.join("evidence.json"),
                package.join("report.json"),
                package.join("three_factor_returns.json"),
            ]);
        } else {
            return Err("storage_factor_package_members_missing".into());
        }
    }
    paths.sort();
    paths.dedup();
    Ok(paths)
}
fn identifier(name: &str) -> bool {
    !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}
