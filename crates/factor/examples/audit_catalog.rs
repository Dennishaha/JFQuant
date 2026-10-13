//! Export the pinned AST and 191 audit records for independent numerical checks.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let registry = analytics_factor::FactorRegistry::default();
    let specs: Vec<_> = registry.all().collect();
    println!(
        "{}",
        serde_json::to_string_pretty(
            &serde_json::json!({"audit":analytics_factor::audit::entries(),"specs":specs})
        )?
    );
    Ok(())
}
