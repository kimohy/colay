use std::fs;
use std::path::{Path, PathBuf};

use codex_compat::{
    AdapterSelection, CapabilityProbe, CapabilityProbeInput, CodexEventParser, CodexItem,
    CompatEvent, CompatibilityError, CompatibilityRegistry, CompatibilityStatus, ProbeCommandKind,
    ProbeOutput, QuotaErrorKind,
};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[allow(clippy::struct_excessive_bools)]
struct Manifest {
    version: String,
    revision: String,
    adapter: String,
    status: String,
    exec: bool,
    jsonl: bool,
    resume: bool,
    app_server: bool,
    output_schema: bool,
    reasoning_effort: bool,
    usage_events: bool,
    fixture_schema: u32,
    metadata_contract_files: Vec<String>,
    event_contract_files: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct VersionCatalog {
    supported_min: String,
    tested_versions: Vec<String>,
    recommended: String,
    pinned_revision: String,
    versions: Vec<CatalogVersion>,
}

#[derive(Debug, Deserialize)]
struct CatalogVersion {
    version: String,
    status: String,
    fixture: String,
    revision: String,
}

#[derive(Debug, Deserialize)]
struct CompatibilityMatrix {
    schema_version: String,
    generated_from: Vec<String>,
    versions: Vec<MatrixRow>,
}

#[derive(Debug, Deserialize)]
struct MatrixRow {
    version: String,
    exec: String,
    jsonl: String,
    resume: String,
    app_server: String,
    output_schema: String,
    reasoning_effort: String,
    usage: String,
    status: String,
}

fn repository_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn fixture_root(version: &str) -> PathBuf {
    repository_root()
        .join("fixtures/codex/versions")
        .join(version)
}

fn read(path: &Path) -> Result<String, std::io::Error> {
    fs::read_to_string(path)
}

fn probe_input(root: &Path) -> Result<CapabilityProbeInput, std::io::Error> {
    let mut input = CapabilityProbeInput::default();
    for (kind, file) in [
        (ProbeCommandKind::Version, "version-output.txt"),
        (ProbeCommandKind::RootHelp, "root-help.txt"),
        (ProbeCommandKind::ExecHelp, "exec-help.txt"),
        (ProbeCommandKind::ExecResumeHelp, "exec-resume-help.txt"),
        (ProbeCommandKind::AppServerHelp, "app-server-help.txt"),
        (ProbeCommandKind::AppServerSchema, "app-server-schema.json"),
    ] {
        input
            .outputs
            .insert(kind, ProbeOutput::success(read(&root.join(file))?));
    }
    input.generated_schema = Some(read(&root.join("app-server-schema.json"))?);
    Ok(input)
}

#[test]
fn n_and_n_minus_one_contracts() -> Result<(), Box<dyn std::error::Error>> {
    for version in ["0.144.6", "0.144.5"] {
        let root = fixture_root(version);
        let manifest: Manifest = toml::from_str(&read(&root.join("manifest.toml"))?)?;
        assert_eq!(manifest.version, version);
        assert_eq!(manifest.revision.len(), 40);
        assert_eq!(manifest.fixture_schema, 1);
        assert_eq!(manifest.adapter, "v0_144_generic");
        assert_eq!(manifest.status, "supported");
        assert!(
            manifest.exec
                && manifest.jsonl
                && manifest.resume
                && manifest.app_server
                && manifest.output_schema
                && manifest.reasoning_effort
                && manifest.usage_events
        );
        for contract_file in manifest
            .metadata_contract_files
            .iter()
            .chain(&manifest.event_contract_files)
        {
            assert!(
                root.join(contract_file).is_file(),
                "missing {contract_file}"
            );
        }

        let report = CapabilityProbe::default().evaluate(&probe_input(&root)?);
        assert_eq!(report.status, CompatibilityStatus::Compatible);
        assert!(matches!(report.adapter, AdapterSelection::Exact { .. }));

        let success = CodexEventParser.parse_stream(&read(&root.join("jsonl-success.jsonl"))?);
        assert!(success.iter().all(Result::is_ok));
        assert!(success.iter().any(|event| matches!(
            event,
            Ok(CompatEvent::TurnCompleted { usage }) if usage.total_observed_tokens().is_some()
        )));

        let tool = CodexEventParser.parse_stream(&read(&root.join("jsonl-tool-call.jsonl"))?);
        assert!(tool.iter().all(Result::is_ok));
        assert!(tool.iter().any(|event| matches!(
            event,
            Ok(CompatEvent::Item {
                item: CodexItem::Unknown { item_type, raw },
                ..
            }) if item_type == "future_optional_item"
                && raw.pointer("/opaque/kept").and_then(serde_json::Value::as_bool) == Some(true)
        )));

        let resumed = CodexEventParser.parse_stream(&read(&root.join("resume-events.jsonl"))?);
        assert!(resumed.iter().all(Result::is_ok));
        assert!(
            resumed
                .iter()
                .any(|event| matches!(event, Ok(CompatEvent::ThreadStarted { .. })))
        );

        let ordinary_error = CodexEventParser.parse_stream(&read(&root.join("jsonl-error.jsonl"))?);
        assert!(ordinary_error.iter().any(|event| matches!(
            event,
            Ok(CompatEvent::Error { quota: None, .. }
                | CompatEvent::TurnFailed { quota: None, .. })
        )));

        let quota = CodexEventParser.parse_stream(&read(&root.join("quota-error.jsonl"))?);
        assert!(quota.iter().any(|event| matches!(
            event,
            Ok(CompatEvent::Error {
                quota: Some(QuotaErrorKind::MonthlyQuota | QuotaErrorKind::InsufficientQuota),
                ..
            } | CompatEvent::TurnFailed {
                quota: Some(QuotaErrorKind::MonthlyQuota | QuotaErrorKind::InsufficientQuota),
                ..
            })
        )));

        let malformed = CodexEventParser.parse_stream(&read(&root.join("malformed-events.jsonl"))?);
        assert!(
            malformed
                .iter()
                .any(|event| matches!(event, Err(CompatibilityError::MalformedJson { .. })))
        );

        let unknown_lifecycle =
            CodexEventParser.parse_stream(&read(&root.join("unknown-lifecycle.jsonl"))?);
        assert!(
            unknown_lifecycle
                .iter()
                .any(|event| matches!(event, Err(CompatibilityError::UnknownLifecycle { .. })))
        );
    }
    Ok(())
}

#[test]
fn current_and_previous_codex_versions_are_exact() {
    let registry = CompatibilityRegistry::default();
    assert!(matches!(
        registry.select(Some(&semver::Version::new(0, 144, 6))),
        AdapterSelection::Exact { .. }
    ));
    assert!(matches!(
        registry.select(Some(&semver::Version::new(0, 144, 5))),
        AdapterSelection::Exact { .. }
    ));
    assert!(matches!(
        registry.select(Some(&semver::Version::new(0, 144, 4))),
        AdapterSelection::GenericUntested
    ));
}

#[test]
fn committed_machine_readable_matrix_matches_catalog_and_version_manifests()
-> Result<(), Box<dyn std::error::Error>> {
    let root = repository_root();
    let catalog: VersionCatalog =
        toml::from_str(&read(&root.join("compatibility/codex-version.toml"))?)?;
    let matrix: CompatibilityMatrix =
        serde_json::from_str(&read(&root.join("compatibility/codex-matrix.json"))?)?;

    assert_eq!(matrix.schema_version, "1");
    assert_eq!(matrix.generated_from.len(), 2);
    assert_eq!(catalog.supported_min, "0.144.5");
    assert_eq!(catalog.tested_versions, ["0.144.5", "0.144.6"]);
    assert_eq!(catalog.recommended, "0.144.6");
    assert_eq!(
        catalog.pinned_revision,
        "5d1fbf26c43abc65a203928b2e31561cb039e06d"
    );
    assert_eq!(matrix.versions.len(), catalog.tested_versions.len());
    let recommended = catalog
        .versions
        .iter()
        .find(|version| version.version == catalog.recommended)
        .ok_or_else(|| std::io::Error::other("recommended Codex has no registry entry"))?;
    assert_eq!(recommended.revision, catalog.pinned_revision);

    let mut matrix_versions = matrix
        .versions
        .iter()
        .map(|row| row.version.clone())
        .collect::<Vec<_>>();
    matrix_versions.sort();
    let mut tested_versions = catalog.tested_versions.clone();
    tested_versions.sort();
    assert_eq!(matrix_versions, tested_versions);
    let mut registry_versions = CompatibilityRegistry::default()
        .tested_versions()
        .into_iter()
        .map(|version| version.to_string())
        .collect::<Vec<_>>();
    registry_versions.sort();
    assert_eq!(registry_versions, tested_versions);

    for registered in &catalog.versions {
        let manifest: Manifest = toml::from_str(&read(
            &fixture_root(&registered.version).join("manifest.toml"),
        )?)?;
        let row = matrix
            .versions
            .iter()
            .find(|candidate| candidate.version == registered.version)
            .ok_or_else(|| {
                std::io::Error::other(format!("matrix row missing for {}", registered.version))
            })?;
        assert_eq!(registered.status, manifest.status);
        assert_eq!(registered.revision, manifest.revision);
        assert_eq!(
            registered.fixture,
            format!("fixtures/codex/versions/{}", registered.version)
        );
        assert_eq!(row.status, manifest.status);
        assert_eq!(row.exec, pass(manifest.exec));
        assert_eq!(row.jsonl, pass(manifest.jsonl));
        assert_eq!(row.resume, pass(manifest.resume));
        assert_eq!(row.app_server, pass(manifest.app_server));
        assert_eq!(row.output_schema, pass(manifest.output_schema));
        assert_eq!(row.reasoning_effort, pass(manifest.reasoning_effort));
        assert_eq!(row.usage, pass(manifest.usage_events));
    }
    Ok(())
}

const fn pass(value: bool) -> &'static str {
    if value { "pass" } else { "fail" }
}
