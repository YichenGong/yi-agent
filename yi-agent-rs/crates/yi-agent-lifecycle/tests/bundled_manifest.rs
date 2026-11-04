//! The bundled sidecar manifest is written by packaging and read by the desktop
//! shell. Its contract is: portable across machines, and no sign-time digest baked
//! in at build time.

use yi_agent_lifecycle::types::BundledArtifactManifest;

fn manifest() -> BundledArtifactManifest {
    BundledArtifactManifest {
        build_id: "1123c23c8dde2e9c9b77941acf62ff56".into(),
        display_version: "0.1.13".into(),
        executable: "Contents/MacOS/yi-agent".into(),
        sha256: "a".repeat(64),
        schema_min: 0,
        schema_max: 15,
        handover_version: 1,
        business_protocol_min: 2,
        business_protocol_max: 2,
        capabilities: vec!["control.v1".into(), "lease.v1".into()],
    }
}

#[test]
fn the_manifest_round_trips_through_json() {
    let original = manifest();
    let text = serde_json::to_string(&original).unwrap();
    assert_eq!(
        serde_json::from_str::<BundledArtifactManifest>(&text).unwrap(),
        original
    );
}

#[test]
fn the_executable_path_is_relative_to_the_bundle() {
    // The field is a `String`, so the type cannot enforce this; the property that
    // matters is that every producer writes a relative, forward-slash path.
    let executable = manifest().executable;
    assert!(!executable.starts_with('/'), "{executable}");
    assert!(!executable.contains('\\'), "{executable}");
}

#[test]
fn every_wire_field_survives_the_round_trip() {
    // Pins the field set: dropping one breaks the desktop's verifier silently,
    // because serde would refuse the manifest at runtime rather than at build time.
    let text = serde_json::to_value(manifest()).unwrap();
    for field in [
        "build_id",
        "display_version",
        "executable",
        "sha256",
        "schema_min",
        "schema_max",
        "handover_version",
        "business_protocol_min",
        "business_protocol_max",
        "capabilities",
    ] {
        assert!(text.get(field).is_some(), "missing field {field}: {text}");
    }
}
