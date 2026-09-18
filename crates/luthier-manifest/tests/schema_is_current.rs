//! Guards the committed JSON Schema against drift.
//!
//! `schemas/package-v1.json` is generated from the Rust types and committed so
//! that editors can offer completion and contributors get feedback before CI
//! runs. If the types change and the schema is not regenerated, the published
//! schema starts describing something that no longer exists — so this test
//! fails instead.

use std::path::PathBuf;

fn workspace_root() -> PathBuf {
  PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    .parent()
    .and_then(|p| p.parent())
    .expect("crates/luthier-manifest sits two levels below the workspace root")
    .to_path_buf()
}

#[test]
fn the_committed_schema_matches_the_types() {
  let path = workspace_root().join("schemas/package-v1.json");
  let committed = std::fs::read_to_string(&path).unwrap_or_else(|e| {
    panic!(
      "cannot read {}: {e}\nRegenerate it with `cargo run -p luthier-registry-tool -- schema`",
      path.display()
    )
  });

  assert_eq!(
    committed,
    luthier_manifest::json_schema_text(),
    "\n{} is out of date.\nRegenerate it with:\n  \
         cargo run -p luthier-registry-tool -- schema > schemas/package-v1.json\n",
    path.display()
  );
}
