//! The committed JSON Schemas in docs/schema/ must match the report types.
//! Run with UPDATE_SCHEMAS=1 to regenerate them after an intended change.

use std::path::PathBuf;

#[test]
fn committed_schemas_match() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs/schema");
    let update = std::env::var_os("UPDATE_SCHEMAS").is_some();
    for (name, schema) in fagia_core::report::schemas() {
        let path = dir.join(format!("{name}.json"));
        let text = serde_json::to_string_pretty(&schema).unwrap() + "\n";
        if update {
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(&path, &text).unwrap();
            continue;
        }
        let committed = std::fs::read_to_string(&path)
            .unwrap_or_else(|_| panic!("{} missing; run with UPDATE_SCHEMAS=1", path.display()));
        assert!(
            committed == text,
            "{} is stale; run with UPDATE_SCHEMAS=1",
            path.display()
        );
    }
}
