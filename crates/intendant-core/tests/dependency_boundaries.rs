#[test]
fn core_manifest_does_not_depend_on_an_http_client() {
    let manifest: toml::Value =
        toml::from_str(include_str!("../Cargo.toml")).expect("intendant-core Cargo.toml parses");
    let dependencies = manifest
        .get("dependencies")
        .and_then(toml::Value::as_table)
        .expect("intendant-core has a dependencies table");

    assert!(
        !dependencies.contains_key("reqwest"),
        "intendant-core is shared by non-networking leaf crates; HTTP clients belong in their consumers"
    );
}
