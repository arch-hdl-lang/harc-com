use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as _;
use std::fs;
use std::io::Write as IoWrite;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

pub const RUNTIME_HEADER_FILENAMES: [&str; 6] = [
    "harc_thread_rt.h",
    "harc_random_rt.h",
    "harc_queue_rt.h",
    "harc_trace_rt.h",
    "harc_log_rt.h",
    "harc_z3_rt.h",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactRole {
    Interface,
    Common,
    Capsule,
    Registry,
    ProbeStub,
    RuntimeHeader,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodegenBackend {
    V1,
    Tbir,
}

impl CodegenBackend {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::V1 => "v1",
            Self::Tbir => "tbir",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CppLayout {
    Common,
    SelfContained,
}

impl CppLayout {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Common => "common",
            Self::SelfContained => "self-contained",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlacementMetrics {
    common_callables: usize,
    capsule_callables: usize,
    capsule_reasons: BTreeMap<String, usize>,
}

impl PlacementMetrics {
    pub fn new(
        common_callables: usize,
        capsule_callables: usize,
        capsule_reasons: BTreeMap<String, usize>,
    ) -> Result<Self, ArtifactError> {
        if capsule_reasons.values().sum::<usize>() != capsule_callables {
            return Err(ArtifactError(
                "common-object capsule placement-reason counts must sum to capsule_callables"
                    .to_string(),
            ));
        }
        Ok(Self {
            common_callables,
            capsule_callables,
            capsule_reasons,
        })
    }

    pub fn common_callables(&self) -> usize {
        self.common_callables
    }

    pub fn capsule_callables(&self) -> usize {
        self.capsule_callables
    }

    pub fn capsule_reasons(&self) -> &BTreeMap<String, usize> {
        &self.capsule_reasons
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestIdentity {
    backend: CodegenBackend,
    layout: CppLayout,
    interface_abi: String,
    build_profile: String,
    placement: PlacementMetrics,
}

impl ManifestIdentity {
    pub fn new(
        backend: CodegenBackend,
        layout: CppLayout,
        interface_abi: impl Into<String>,
        build_profile: impl Into<String>,
        placement: PlacementMetrics,
    ) -> Self {
        Self {
            backend,
            layout,
            interface_abi: interface_abi.into(),
            build_profile: build_profile.into(),
            placement,
        }
    }

    pub fn backend(&self) -> CodegenBackend {
        self.backend
    }

    pub fn layout(&self) -> CppLayout {
        self.layout
    }

    pub fn interface_abi(&self) -> &str {
        &self.interface_abi
    }

    pub fn build_profile(&self) -> &str {
        &self.build_profile
    }

    pub fn placement(&self) -> &PlacementMetrics {
        &self.placement
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactSpec {
    role: ArtifactRole,
    filename: String,
}

impl ArtifactSpec {
    pub fn role(&self) -> ArtifactRole {
        self.role
    }

    pub fn filename(&self) -> &str {
        &self.filename
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestArtifact {
    name: String,
    symbol_stem: String,
    capsule_index: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommonUnitRequest {
    key: String,
}

impl CommonUnitRequest {
    pub fn new(key: impl Into<String>) -> Self {
        Self { key: key.into() }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapsuleRequest {
    key: String,
    test_names: Vec<String>,
}

impl CapsuleRequest {
    pub fn new(key: impl Into<String>, test_names: Vec<String>) -> Self {
        Self {
            key: key.into(),
            test_names,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommonUnitArtifact {
    key: String,
    symbol_stem: String,
    artifact_index: usize,
}

impl CommonUnitArtifact {
    pub fn key(&self) -> &str {
        &self.key
    }

    pub fn symbol_stem(&self) -> &str {
        &self.symbol_stem
    }

    pub fn artifact_index(&self) -> usize {
        self.artifact_index
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapsuleArtifact {
    key: String,
    symbol_stem: String,
    test_names: Vec<String>,
    test_indices: Vec<usize>,
    artifact_index: usize,
}

impl CapsuleArtifact {
    pub fn key(&self) -> &str {
        &self.key
    }

    pub fn symbol_stem(&self) -> &str {
        &self.symbol_stem
    }

    pub fn test_indices(&self) -> &[usize] {
        &self.test_indices
    }

    pub fn test_names(&self) -> &[String] {
        &self.test_names
    }

    pub fn artifact_index(&self) -> usize {
        self.artifact_index
    }
}

impl TestArtifact {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn symbol_stem(&self) -> &str {
        &self.symbol_stem
    }

    pub fn capsule_index(&self) -> usize {
        self.capsule_index
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactError(pub String);

impl std::fmt::Display for ArtifactError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ArtifactError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommonArtifactPlan {
    prefix: String,
    tests: Vec<TestArtifact>,
    common_units: Vec<CommonUnitArtifact>,
    capsules: Vec<CapsuleArtifact>,
    artifacts: Vec<ArtifactSpec>,
    manifest_filename: String,
}

impl CommonArtifactPlan {
    pub fn new(prefix: &str, test_names: &[String]) -> Result<Self, ArtifactError> {
        let common_units = [CommonUnitRequest::new("runtime")];
        let capsules: Vec<CapsuleRequest> = test_names
            .iter()
            .map(|name| CapsuleRequest::new(name.clone(), vec![name.clone()]))
            .collect();
        Self::from_layout(prefix, test_names, &common_units, &capsules)
    }

    pub fn from_layout(
        prefix: &str,
        registry_order: &[String],
        common_unit_requests: &[CommonUnitRequest],
        capsule_requests: &[CapsuleRequest],
    ) -> Result<Self, ArtifactError> {
        validate_prefix(prefix)?;
        if registry_order.is_empty() {
            return Err(ArtifactError(
                "common-object layout requires at least one test".to_string(),
            ));
        }
        if common_unit_requests.is_empty() {
            return Err(ArtifactError(
                "common-object layout requires at least one common implementation unit".to_string(),
            ));
        }
        if capsule_requests.is_empty() {
            return Err(ArtifactError(
                "common-object layout requires at least one capsule".to_string(),
            ));
        }

        let mut tests = Vec::with_capacity(registry_order.len());
        let mut test_lookup = HashMap::new();
        let mut seen_test_stems = HashSet::new();
        for (index, name) in registry_order.iter().enumerate() {
            if test_lookup.insert(name.clone(), index).is_some() {
                return Err(ArtifactError(format!(
                    "duplicate test name `{name}` — common-object layout requires unique test identities"
                )));
            }
            let symbol_stem = sanitize_file_component(name);
            if !seen_test_stems.insert(symbol_stem.clone()) {
                return Err(ArtifactError(format!(
                    "test name `{name}` collides with another common-object symbol after sanitization as `{symbol_stem}`"
                )));
            }
            tests.push(TestArtifact {
                name: name.clone(),
                symbol_stem,
                capsule_index: usize::MAX,
            });
        }

        let mut artifacts = vec![ArtifactSpec {
            role: ArtifactRole::Interface,
            filename: format!("{prefix}suite_api.hpp"),
        }];
        let mut common_units = Vec::with_capacity(common_unit_requests.len());
        let mut seen_common_keys = HashSet::new();
        let mut seen_common_stems = HashSet::new();
        for request in common_unit_requests {
            validate_layout_key("common implementation", &request.key)?;
            if !seen_common_keys.insert(request.key.clone()) {
                return Err(ArtifactError(format!(
                    "duplicate common implementation key `{}`",
                    request.key
                )));
            }
            let symbol_stem = sanitize_file_component(&request.key);
            if !seen_common_stems.insert(symbol_stem.clone()) {
                return Err(ArtifactError(format!(
                    "common implementation key `{}` collides after sanitization as `{symbol_stem}`",
                    request.key
                )));
            }
            let artifact_index = artifacts.len();
            artifacts.push(ArtifactSpec {
                role: ArtifactRole::Common,
                filename: format!("{prefix}{symbol_stem}.cpp"),
            });
            common_units.push(CommonUnitArtifact {
                key: request.key.clone(),
                symbol_stem,
                artifact_index,
            });
        }

        let mut capsules = Vec::with_capacity(capsule_requests.len());
        let mut seen_capsule_keys = HashSet::new();
        let mut seen_capsule_stems = HashSet::new();
        let mut assigned_tests = vec![false; tests.len()];
        for request in capsule_requests {
            validate_layout_key("capsule", &request.key)?;
            if request.test_names.is_empty() {
                return Err(ArtifactError(format!(
                    "common-object capsule `{}` must contain at least one test",
                    request.key
                )));
            }
            if !seen_capsule_keys.insert(request.key.clone()) {
                return Err(ArtifactError(format!(
                    "duplicate common-object capsule key `{}`",
                    request.key
                )));
            }
            let symbol_stem = sanitize_file_component(&request.key);
            if !seen_capsule_stems.insert(symbol_stem.clone()) {
                return Err(ArtifactError(format!(
                    "capsule key `{}` collides after sanitization as `{symbol_stem}`",
                    request.key
                )));
            }
            let capsule_index = capsules.len();
            let mut test_indices = Vec::with_capacity(request.test_names.len());
            for name in &request.test_names {
                let Some(&test_index) = test_lookup.get(name) else {
                    return Err(ArtifactError(format!(
                        "common-object capsule `{}` names unknown test `{name}`",
                        request.key
                    )));
                };
                if std::mem::replace(&mut assigned_tests[test_index], true) {
                    return Err(ArtifactError(format!(
                        "test `{name}` appears in more than one common-object capsule"
                    )));
                }
                tests[test_index].capsule_index = capsule_index;
                test_indices.push(test_index);
            }
            let artifact_index = artifacts.len();
            artifacts.push(ArtifactSpec {
                role: ArtifactRole::Capsule,
                filename: format!("{prefix}test_{symbol_stem}.cpp"),
            });
            capsules.push(CapsuleArtifact {
                key: request.key.clone(),
                symbol_stem,
                test_names: request.test_names.clone(),
                test_indices,
                artifact_index,
            });
        }
        if let Some((index, _)) = assigned_tests
            .iter()
            .enumerate()
            .find(|(_, assigned)| !**assigned)
        {
            return Err(ArtifactError(format!(
                "test `{}` does not belong to any common-object capsule",
                tests[index].name
            )));
        }

        artifacts.push(ArtifactSpec {
            role: ArtifactRole::Registry,
            filename: format!("{prefix}registry.cpp"),
        });

        let mut filenames = HashSet::new();
        for artifact in &artifacts {
            validate_owned_filename(&artifact.filename)?;
            if !filenames.insert(artifact.filename.clone()) {
                return Err(ArtifactError(format!(
                    "common-object artifact filename collision: `{}`",
                    artifact.filename
                )));
            }
        }

        let manifest_filename = format!("{prefix}artifacts.json");
        validate_owned_filename(&manifest_filename)?;

        Ok(Self {
            prefix: prefix.to_string(),
            tests,
            common_units,
            capsules,
            artifacts,
            manifest_filename,
        })
    }

    pub fn tests(&self) -> &[TestArtifact] {
        &self.tests
    }

    pub fn artifacts(&self) -> &[ArtifactSpec] {
        &self.artifacts
    }

    pub fn artifact(&self, index: usize) -> &ArtifactSpec {
        &self.artifacts[index]
    }

    pub fn interface(&self) -> &ArtifactSpec {
        self.artifacts
            .iter()
            .find(|artifact| artifact.role == ArtifactRole::Interface)
            .expect("common artifact plan always has one interface")
    }

    pub fn common_units(&self) -> &[CommonUnitArtifact] {
        &self.common_units
    }

    pub fn common_artifacts(&self) -> impl Iterator<Item = &ArtifactSpec> {
        self.common_units
            .iter()
            .map(|unit| &self.artifacts[unit.artifact_index])
    }

    pub fn capsules(&self) -> &[CapsuleArtifact] {
        &self.capsules
    }

    pub fn capsule_for_test(&self, test_index: usize) -> &CapsuleArtifact {
        &self.capsules[self.tests[test_index].capsule_index]
    }

    pub fn registry(&self) -> &ArtifactSpec {
        self.artifacts
            .iter()
            .find(|artifact| artifact.role == ArtifactRole::Registry)
            .expect("common artifact plan always has one registry")
    }

    pub fn manifest_filename(&self) -> &str {
        &self.manifest_filename
    }

    pub fn prefix(&self) -> &str {
        &self.prefix
    }

    pub fn with_probe_stub(mut self) -> Result<Self, ArtifactError> {
        if self.probe_stub().is_some() {
            return Err(ArtifactError(
                "common-object plan already owns a probe stub".to_string(),
            ));
        }
        let filename = format!("{}probe_stub.sv", self.prefix);
        validate_owned_filename(&filename)?;
        if self
            .artifacts
            .iter()
            .any(|artifact| artifact.filename == filename)
        {
            return Err(ArtifactError(format!(
                "common-object artifact filename collision: `{filename}`"
            )));
        }
        self.artifacts.push(ArtifactSpec {
            role: ArtifactRole::ProbeStub,
            filename,
        });
        Ok(self)
    }

    pub fn with_runtime_headers(mut self) -> Result<Self, ArtifactError> {
        if self
            .artifacts
            .iter()
            .any(|artifact| artifact.role == ArtifactRole::RuntimeHeader)
        {
            return Err(ArtifactError(
                "common-object plan already owns runtime headers".to_string(),
            ));
        }
        for filename in RUNTIME_HEADER_FILENAMES {
            validate_owned_filename(filename)?;
            if self
                .artifacts
                .iter()
                .any(|artifact| artifact.filename == filename)
            {
                return Err(ArtifactError(format!(
                    "common-object artifact filename collision: `{filename}`"
                )));
            }
            self.artifacts.push(ArtifactSpec {
                role: ArtifactRole::RuntimeHeader,
                filename: filename.to_string(),
            });
        }
        Ok(self)
    }

    pub fn probe_stub(&self) -> Option<&ArtifactSpec> {
        self.artifacts
            .iter()
            .find(|artifact| artifact.role == ArtifactRole::ProbeStub)
    }

    pub fn runtime_headers(&self) -> impl Iterator<Item = &ArtifactSpec> {
        self.artifacts
            .iter()
            .filter(|artifact| artifact.role == ArtifactRole::RuntimeHeader)
    }

    pub fn render_manifest(
        &self,
        interface_abi: &str,
        build_profile: &str,
    ) -> Result<String, ArtifactError> {
        if !self.is_legacy_schema_one_layout() {
            return Err(ArtifactError(
                "schema-1 manifests can encode only one `runtime` common unit and one capsule per test"
                    .to_string(),
            ));
        }
        if !is_legacy_fingerprint(interface_abi) || !is_legacy_fingerprint(build_profile) {
            return Err(ArtifactError(
                "common-object manifest fingerprints must be 16 lowercase hexadecimal digits"
                    .to_string(),
            ));
        }
        let manifest = ManifestV1 {
            schema_version: MANIFEST_SCHEMA_V1,
            interface_abi: interface_abi.to_string(),
            build_profile: build_profile.to_string(),
            tests: self.tests.iter().map(|test| test.name.clone()).collect(),
            artifacts: self
                .artifacts
                .iter()
                .map(|artifact| artifact.filename.clone())
                .collect(),
        };
        let mut rendered = serde_json::to_string(&manifest)
            .map_err(|error| ArtifactError(format!("render common-object manifest: {error}")))?;
        rendered.push('\n');
        Ok(rendered)
    }

    pub fn render_manifest_v2(&self, identity: &ManifestIdentity) -> Result<String, ArtifactError> {
        validate_manifest_identity(identity)?;
        if identity.layout != CppLayout::Common {
            return Err(ArtifactError(
                "common-object artifact manifests require the `common` C++ layout".to_string(),
            ));
        }
        let manifest = ManifestV2 {
            schema_version: MANIFEST_SCHEMA_V2,
            backend: identity.backend,
            layout: identity.layout,
            interface_abi: identity.interface_abi.clone(),
            build_profile: identity.build_profile.clone(),
            tests: self.tests.iter().map(|test| test.name.clone()).collect(),
            artifacts: self.manifest_artifacts(),
            placement: identity.placement.clone(),
        };
        let mut rendered = serde_json::to_string(&manifest)
            .map_err(|error| ArtifactError(format!("render common-object manifest: {error}")))?;
        rendered.push('\n');
        Ok(rendered)
    }

    fn manifest_artifacts(&self) -> Vec<ManifestArtifact> {
        let interface = self.interface().filename().to_string();
        let runtime_headers = self
            .runtime_headers()
            .map(|artifact| artifact.filename.clone())
            .collect::<Vec<_>>();
        let linked_sources = self
            .artifacts
            .iter()
            .filter(|artifact| {
                matches!(artifact.role, ArtifactRole::Common | ArtifactRole::Capsule)
            })
            .map(|artifact| artifact.filename.clone())
            .collect::<Vec<_>>();
        self.artifacts
            .iter()
            .enumerate()
            .map(|(index, artifact)| match artifact.role {
                ArtifactRole::Interface => ManifestArtifact {
                    filename: artifact.filename.clone(),
                    role: artifact.role,
                    owner: "suite".to_string(),
                    tests: Vec::new(),
                    dependencies: runtime_headers.clone(),
                },
                ArtifactRole::Common => {
                    let unit = self
                        .common_units
                        .iter()
                        .find(|unit| unit.artifact_index == index)
                        .expect("every common artifact has a planned owner");
                    ManifestArtifact {
                        filename: artifact.filename.clone(),
                        role: artifact.role,
                        owner: unit.key.clone(),
                        tests: Vec::new(),
                        dependencies: vec![interface.clone()],
                    }
                }
                ArtifactRole::Capsule => {
                    let capsule = self
                        .capsules
                        .iter()
                        .find(|capsule| capsule.artifact_index == index)
                        .expect("every capsule artifact has a planned owner");
                    ManifestArtifact {
                        filename: artifact.filename.clone(),
                        role: artifact.role,
                        owner: capsule.key.clone(),
                        tests: capsule.test_names.clone(),
                        dependencies: vec![interface.clone()],
                    }
                }
                ArtifactRole::Registry => {
                    let mut dependencies = vec![interface.clone()];
                    dependencies.extend(linked_sources.iter().cloned());
                    ManifestArtifact {
                        filename: artifact.filename.clone(),
                        role: artifact.role,
                        owner: "registry".to_string(),
                        tests: self.tests.iter().map(|test| test.name.clone()).collect(),
                        dependencies,
                    }
                }
                ArtifactRole::ProbeStub => ManifestArtifact {
                    filename: artifact.filename.clone(),
                    role: artifact.role,
                    owner: "probe_stub".to_string(),
                    tests: self.tests.iter().map(|test| test.name.clone()).collect(),
                    dependencies: Vec::new(),
                },
                ArtifactRole::RuntimeHeader => ManifestArtifact {
                    filename: artifact.filename.clone(),
                    role: artifact.role,
                    owner: "runtime_support".to_string(),
                    tests: Vec::new(),
                    dependencies: Vec::new(),
                },
            })
            .collect()
    }

    fn is_legacy_schema_one_layout(&self) -> bool {
        self.common_units.len() == 1
            && self.common_units[0].key == "runtime"
            && self.capsules.len() == self.tests.len()
            && self.capsules.iter().enumerate().all(|(index, capsule)| {
                capsule.key == self.tests[index].name && capsule.test_indices == [index]
            })
    }

    pub fn begin_publication<'a>(
        &'a self,
        outdir: &'a Path,
        interface_abi: &str,
        build_profile: &str,
    ) -> Result<Publication<'a>, ArtifactError> {
        let manifest_contents = self.render_manifest(interface_abi, build_profile)?;
        self.begin_publication_with_manifest(outdir, manifest_contents)
    }

    pub fn begin_publication_v2<'a>(
        &'a self,
        outdir: &'a Path,
        identity: &ManifestIdentity,
    ) -> Result<Publication<'a>, ArtifactError> {
        let manifest_contents = self.render_manifest_v2(identity)?;
        self.begin_publication_with_manifest(outdir, manifest_contents)
    }

    fn begin_publication_with_manifest<'a>(
        &'a self,
        outdir: &'a Path,
        manifest_contents: String,
    ) -> Result<Publication<'a>, ArtifactError> {
        fs::create_dir_all(outdir).map_err(|error| {
            ArtifactError(format!(
                "create common-object output directory {}: {error}",
                outdir.display()
            ))
        })?;
        let output_lock = fs::File::open(outdir).map_err(|error| {
            ArtifactError(format!(
                "open common-object output directory {} for locking: {error}",
                outdir.display()
            ))
        })?;
        output_lock.lock().map_err(|error| {
            ArtifactError(format!(
                "lock common-object output directory {}: {error}",
                outdir.display()
            ))
        })?;
        let target_manifest = trusted_manifest(manifest_contents.as_bytes(), self.prefix())
            .expect("the plan always renders a trusted manifest");
        let manifest_path = outdir.join(self.manifest_filename());
        let recovery_manifest_path = recovery_manifest_path(&manifest_path)?;
        let pending_journal_path = pending_journal_path(&manifest_path)?;
        let manifest_snapshot = snapshot_path(&manifest_path)?;
        let recovery_snapshot = snapshot_path(&recovery_manifest_path)?;
        let pending_snapshot = snapshot_path(&pending_journal_path)?;
        let previous = match &manifest_snapshot {
            PathSnapshot::Regular(contents) => {
                trusted_manifest(contents, self.prefix()).map(|manifest| PreviousManifest {
                    manifest,
                    contents: contents.clone(),
                    origin: PreviousManifestOrigin::Canonical,
                })
            }
            PathSnapshot::Missing => match &recovery_snapshot {
                PathSnapshot::Regular(contents) => {
                    trusted_manifest(contents, self.prefix()).map(|manifest| PreviousManifest {
                        manifest,
                        contents: contents.clone(),
                        origin: PreviousManifestOrigin::Recovery,
                    })
                }
                PathSnapshot::Missing | PathSnapshot::Other => None,
            },
            PathSnapshot::Other => None,
        };
        let trust_pending = matches!(manifest_snapshot, PathSnapshot::Missing)
            || previous
                .as_ref()
                .is_some_and(|previous| previous.origin == PreviousManifestOrigin::Canonical);
        let mut ownership_manifests = if trust_pending {
            match &pending_snapshot {
                PathSnapshot::Regular(contents) => {
                    parse_pending_journal(contents, self.prefix()).unwrap_or_default()
                }
                PathSnapshot::Missing | PathSnapshot::Other => Vec::new(),
            }
        } else {
            Vec::new()
        };
        if let Some(previous) = &previous {
            push_unique_manifest(&mut ownership_manifests, previous.manifest.clone());
        }
        push_unique_manifest(&mut ownership_manifests, target_manifest);
        let owned_artifacts = ordered_owned_artifacts(&ownership_manifests);
        let pending_journal_contents = render_pending_journal(&ownership_manifests)?;
        Ok(Publication {
            outdir,
            plan: self,
            output_lock,
            manifest_path,
            recovery_manifest_path,
            pending_journal_path,
            manifest_contents,
            manifest_snapshot,
            recovery_snapshot,
            pending_snapshot,
            previous,
            pending_journal_contents,
            owned_artifacts,
            states: (0..self.artifacts.len())
                .map(|_| AtomicU8::new(ARTIFACT_PENDING))
                .collect(),
            staged_paths: Mutex::new((0..self.artifacts.len()).map(|_| None).collect()),
            reused_contents: Mutex::new((0..self.artifacts.len()).map(|_| None).collect()),
            rewritten: AtomicUsize::new(0),
            commit_started: Mutex::new(false),
        })
    }
}

fn validate_layout_key(kind: &str, key: &str) -> Result<(), ArtifactError> {
    if key.is_empty() {
        return Err(ArtifactError(format!(
            "common-object {kind} key must not be empty"
        )));
    }
    Ok(())
}

fn validate_prefix(prefix: &str) -> Result<(), ArtifactError> {
    if prefix.is_empty() {
        return Ok(());
    }
    let mut components = Path::new(prefix).components();
    let valid = !prefix.contains('/')
        && !prefix.contains('\\')
        && matches!(components.next(), Some(Component::Normal(_)))
        && components.next().is_none();
    if !valid {
        return Err(ArtifactError(format!(
            "common-object artifact prefix must be empty or one basename component: `{prefix}`"
        )));
    }
    Ok(())
}

fn validate_owned_filename(filename: &str) -> Result<(), ArtifactError> {
    let path = Path::new(filename);
    let mut components = path.components();
    let valid = matches!(components.next(), Some(Component::Normal(_)))
        && components.next().is_none()
        && !filename.is_empty();
    if !valid {
        return Err(ArtifactError(format!(
            "common-object artifact filename must be one relative path component: `{filename}`"
        )));
    }
    Ok(())
}

pub fn stable_hash_hex(data: &[u8]) -> String {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in data {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

pub fn build_profile_fingerprint(mt: bool, extra: &[String]) -> String {
    let mut canonical = String::new();
    canonical.push_str("harc-cpp-suite-profile-v2\n");
    canonical.push_str(&format!("mt={mt}\n"));
    let mut extras: Vec<&String> = extra.iter().collect();
    extras.sort();
    for entry in extras {
        canonical.push_str(entry);
        canonical.push('\n');
    }
    stable_hash_hex(canonical.as_bytes())
}

const INTERFACE_BEGIN: &str = "// === iface-begin ===";
const INTERFACE_END: &str = "// === iface-end ===";
pub const ABI_ANCHOR_PLACEHOLDER: &str = "harc_suite_abi_ANCHOR";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AbiAnchor {
    digest: String,
    symbol: String,
}

impl AbiAnchor {
    pub fn from_marked_interface(header: &str) -> Result<Self, ArtifactError> {
        Self::from_marked_interface_bytes(header, None)
    }

    pub fn from_marked_interface_with_identity(
        header: &str,
        backend: CodegenBackend,
        layout: CppLayout,
        abi_inputs: &[String],
    ) -> Result<Self, ArtifactError> {
        Self::from_marked_interface_bytes(header, Some((backend, layout, abi_inputs)))
    }

    fn from_marked_interface_bytes(
        header: &str,
        identity: Option<(CodegenBackend, CppLayout, &[String])>,
    ) -> Result<Self, ArtifactError> {
        if header.match_indices(INTERFACE_BEGIN).count() != 1
            || header.match_indices(INTERFACE_END).count() != 1
        {
            return Err(ArtifactError(
                "common-object interface must contain one ABI begin marker and one ABI end marker"
                    .to_string(),
            ));
        }
        let start = header
            .find(INTERFACE_BEGIN)
            .expect("counted one ABI begin marker")
            + INTERFACE_BEGIN.len();
        let end = header
            .find(INTERFACE_END)
            .expect("counted one ABI end marker");
        if end < start {
            return Err(ArtifactError(
                "common-object interface ABI markers are out of order".to_string(),
            ));
        }
        let digest = match identity {
            Some((backend, layout, abi_inputs)) => {
                let mut canonical = String::new();
                canonical.push_str("harc-common-interface-abi-v2\n");
                canonical.push_str(&format!("manifest_schema={MANIFEST_SCHEMA_V2}\n"));
                canonical.push_str(&format!("backend={}\n", backend.as_str()));
                canonical.push_str(&format!("layout={}\n", layout.as_str()));
                let mut abi_inputs = abi_inputs.to_vec();
                abi_inputs.sort();
                for input in abi_inputs {
                    canonical.push_str("abi_input=");
                    canonical.push_str(&input);
                    canonical.push('\n');
                }
                canonical.push_str(&header[start..end]);
                stable_hash_hex(canonical.as_bytes())
            }
            None => stable_hash_hex(header[start..end].as_bytes()),
        };
        let symbol = format!("harc_suite_abi_{digest}");
        Ok(Self { digest, symbol })
    }

    pub fn digest(&self) -> &str {
        &self.digest
    }

    pub fn symbol(&self) -> &str {
        &self.symbol
    }

    pub fn bind_declarations(&self, text: &str) -> Result<String, ArtifactError> {
        if !text.contains(ABI_ANCHOR_PLACEHOLDER) {
            return Err(ArtifactError(
                "common-object artifact is missing the ABI anchor placeholder".to_string(),
            ));
        }
        Ok(text.replace(ABI_ANCHOR_PLACEHOLDER, self.symbol()))
    }

    pub fn bind_definition(&self, text: &str) -> Result<String, ArtifactError> {
        let placeholder = format!("extern const char {ABI_ANCHOR_PLACEHOLDER}[];");
        if text.matches(&placeholder).count() != 1 {
            return Err(ArtifactError(
                "common-object implementation must contain one ABI anchor declaration".to_string(),
            ));
        }
        let definition = format!(
            "extern const char {symbol}[];\nconst char {symbol}[] = \"{digest}\";",
            symbol = self.symbol(),
            digest = self.digest()
        );
        Ok(text
            .replacen(&placeholder, &definition, 1)
            .replace(ABI_ANCHOR_PLACEHOLDER, self.symbol()))
    }
}

pub fn render_registry(plan: &CommonArtifactPlan, profile: &str, anchor: &AbiAnchor) -> String {
    render_registry_impl(plan, profile, anchor, false)
}

/// Render a registry whose dispatch path validates every capsule descriptor
/// against the suite's digest-specific ABI anchor. This opt-in form is for
/// backends whose descriptor ABI includes an `abi_anchor` field. The default
/// renderer implements the v1 byte contract.
pub fn render_registry_with_required_abi(
    plan: &CommonArtifactPlan,
    profile: &str,
    anchor: &AbiAnchor,
) -> String {
    render_registry_impl(plan, profile, anchor, true)
}

fn render_registry_impl(
    plan: &CommonArtifactPlan,
    profile: &str,
    anchor: &AbiAnchor,
    require_abi: bool,
) -> String {
    let mut out = String::new();
    writeln!(out, "// Auto-generated by harc — do not edit.").ok();
    writeln!(
        out,
        "// HARC common-object suite registry + dispatcher (issue #643)."
    )
    .ok();
    writeln!(out, "// harc build-profile: {profile}").ok();
    writeln!(out).ok();
    if require_abi {
        writeln!(out, "#include <cstdio>").ok();
    }
    writeln!(out, "#include <cstring>").ok();
    writeln!(out, "#include <string>").ok();
    writeln!(out, "#include <vector>").ok();
    writeln!(out).ok();
    writeln!(out, "#include \"{}suite_api.hpp\"", plan.prefix()).ok();
    writeln!(out).ok();
    writeln!(out, "extern const char {}[];", anchor.symbol()).ok();
    for test in plan.tests() {
        writeln!(
            out,
            "extern \"C\" const HarcTestDescriptor harc_test_{};",
            test.symbol_stem()
        )
        .ok();
    }
    writeln!(out).ok();
    writeln!(
        out,
        "static const HarcTestDescriptor* const harc_suite_tests[] = {{"
    )
    .ok();
    for test in plan.tests() {
        writeln!(out, "    &harc_test_{},", test.symbol_stem()).ok();
    }
    writeln!(out, "}};").ok();
    writeln!(out).ok();
    if !require_abi {
        writeln!(
            out,
            "[[maybe_unused]] static const char* const harc_abi_ref_registry = {};",
            anchor.symbol()
        )
        .ok();
        writeln!(out).ok();
    }
    writeln!(out, "int main(int argc, char** argv) {{").ok();
    writeln!(
        out,
        "    const char* test_sel = harc_rt::log::harc_select_test(argc, argv);"
    )
    .ok();
    writeln!(
        out,
        "    constexpr size_t harc_suite_test_count = sizeof(harc_suite_tests) / sizeof(harc_suite_tests[0]);"
    )
    .ok();
    if require_abi {
        writeln!(
            out,
            "    for (size_t _i = 0; _i < harc_suite_test_count; ++_i) {{"
        )
        .ok();
        writeln!(
            out,
            "        if (harc_suite_tests[_i]->abi_anchor != {}) {{",
            anchor.symbol()
        )
        .ok();
        writeln!(
            out,
            "            std::fprintf(stderr, \"HARC: common-object ABI mismatch for test '%s'\\n\", harc_suite_tests[_i]->name);"
        )
        .ok();
        writeln!(out, "            return 1;").ok();
        writeln!(out, "        }}").ok();
        writeln!(out, "    }}").ok();
    }
    writeln!(
        out,
        "    if (!test_sel) return harc_suite_tests[0]->run(argc, argv);"
    )
    .ok();
    writeln!(
        out,
        "    for (size_t _i = 0; _i < harc_suite_test_count; ++_i) {{"
    )
    .ok();
    writeln!(
        out,
        "        if (std::strcmp(test_sel, harc_suite_tests[_i]->name) == 0) {{"
    )
    .ok();
    writeln!(
        out,
        "            return harc_suite_tests[_i]->run(argc, argv);"
    )
    .ok();
    writeln!(out, "        }}").ok();
    writeln!(out, "    }}").ok();
    writeln!(out, "    std::string avail;").ok();
    writeln!(
        out,
        "    for (size_t _i = 0; _i < harc_suite_test_count; ++_i) {{"
    )
    .ok();
    writeln!(out, "        if (_i) avail += \", \";").ok();
    writeln!(out, "        avail += harc_suite_tests[_i]->name;").ok();
    writeln!(out, "    }}").ok();
    writeln!(
        out,
        "    harc_rt::log::harc_report_unknown_test(test_sel, avail.c_str());"
    )
    .ok();
    writeln!(out, "    return 1;").ok();
    writeln!(out, "}}").ok();
    writeln!(out).ok();
    out
}

pub fn render_test_descriptor(test: &TestArtifact) -> String {
    format!(
        "extern \"C\" const HarcTestDescriptor harc_test_{stem} = {{\n    \"{name}\",\n    &run_{name},\n}};\n",
        stem = test.symbol_stem(),
        name = test.name()
    )
}

/// Render a descriptor that owns a relocation to the digest-specific ABI
/// anchor. The caller binds [`ABI_ANCHOR_PLACEHOLDER`] after computing the
/// interface digest. The default renderer implements the v1 byte contract.
pub fn render_test_descriptor_with_required_abi(test: &TestArtifact) -> String {
    format!(
        "extern \"C\" const HarcTestDescriptor harc_test_{stem} = {{\n    \"{name}\",\n    &run_{name},\n    {anchor},\n}};\n",
        stem = test.symbol_stem(),
        name = test.name(),
        anchor = ABI_ANCHOR_PLACEHOLDER,
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteStatus {
    Written,
    Reused,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishResult {
    rewritten_artifacts: usize,
    manifest_status: WriteStatus,
    removed: Vec<PathBuf>,
}

impl PublishResult {
    pub fn rewritten_artifacts(&self) -> usize {
        self.rewritten_artifacts
    }

    pub fn manifest_status(&self) -> WriteStatus {
        self.manifest_status
    }

    pub fn removed(&self) -> &[PathBuf] {
        &self.removed
    }
}

const ARTIFACT_PENDING: u8 = 0;
const ARTIFACT_WRITING: u8 = 1;
const ARTIFACT_WRITTEN: u8 = 2;

pub struct Publication<'a> {
    outdir: &'a Path,
    plan: &'a CommonArtifactPlan,
    output_lock: fs::File,
    manifest_path: PathBuf,
    recovery_manifest_path: PathBuf,
    pending_journal_path: PathBuf,
    manifest_contents: String,
    manifest_snapshot: PathSnapshot,
    recovery_snapshot: PathSnapshot,
    pending_snapshot: PathSnapshot,
    previous: Option<PreviousManifest>,
    pending_journal_contents: String,
    owned_artifacts: Vec<String>,
    states: Vec<AtomicU8>,
    staged_paths: Mutex<Vec<Option<PathBuf>>>,
    reused_contents: Mutex<Vec<Option<Vec<u8>>>>,
    rewritten: AtomicUsize,
    commit_started: Mutex<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PathSnapshot {
    Missing,
    Regular(Vec<u8>),
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PreviousManifestOrigin {
    Canonical,
    Recovery,
}

#[derive(Debug, Clone)]
struct PreviousManifest {
    manifest: ArtifactManifest,
    contents: Vec<u8>,
    origin: PreviousManifestOrigin,
}

trait PublicationCommitHooks {
    fn before_artifact_promotion(&self, _index: usize, _path: &Path) -> Result<(), ArtifactError> {
        Ok(())
    }

    fn before_stale_cleanup(&self, _index: usize, _path: &Path) -> Result<(), ArtifactError> {
        Ok(())
    }

    fn before_manifest_publish(&self, _path: &Path) -> Result<(), ArtifactError> {
        Ok(())
    }
}

struct NoPublicationCommitHooks;

impl PublicationCommitHooks for NoPublicationCommitHooks {}

impl Publication<'_> {
    pub fn write(&self, filename: &str, contents: &[u8]) -> Result<WriteStatus, ArtifactError> {
        let index = self
            .plan
            .artifacts
            .iter()
            .position(|artifact| artifact.filename() == filename)
            .ok_or_else(|| {
                ArtifactError(format!(
                    "common-object backend produced unplanned artifact `{filename}`"
                ))
            })?;
        self.states[index]
            .compare_exchange(
                ARTIFACT_PENDING,
                ARTIFACT_WRITING,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .map_err(|_| {
                ArtifactError(format!(
                    "common-object artifact `{filename}` was delivered more than once"
                ))
            })?;

        let path = self.outdir.join(filename);
        match stage_if_changed(&path, contents) {
            Ok((status, staged_path)) => {
                self.staged_paths
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())[index] = staged_path;
                if status == WriteStatus::Reused {
                    self.reused_contents
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())[index] =
                        Some(contents.to_vec());
                }
                if status == WriteStatus::Written {
                    self.rewritten.fetch_add(1, Ordering::Relaxed);
                }
                self.states[index].store(ARTIFACT_WRITTEN, Ordering::Release);
                Ok(status)
            }
            Err(error) => {
                self.states[index].store(ARTIFACT_PENDING, Ordering::Release);
                Err(error)
            }
        }
    }

    pub fn commit(&self) -> Result<PublishResult, ArtifactError> {
        self.commit_with_hooks(&NoPublicationCommitHooks)
    }

    fn commit_with_hooks(
        &self,
        hooks: &dyn PublicationCommitHooks,
    ) -> Result<PublishResult, ArtifactError> {
        let mut commit_started = self
            .commit_started
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if *commit_started {
            return Err(ArtifactError(
                "common-object artifact publication commit was already attempted".to_string(),
            ));
        }
        let missing: Vec<&str> = self
            .plan
            .artifacts
            .iter()
            .zip(&self.states)
            .filter_map(|(artifact, state)| {
                (state.load(Ordering::Acquire) != ARTIFACT_WRITTEN).then_some(artifact.filename())
            })
            .collect();
        if !missing.is_empty() {
            return Err(ArtifactError(format!(
                "common-object publication is incomplete; missing artifacts: {}",
                missing.join(", ")
            )));
        }

        self.revalidate_reused_artifacts()?;

        let current: HashSet<&str> = self
            .plan
            .artifacts
            .iter()
            .map(ArtifactSpec::filename)
            .collect();
        let stale: Vec<PathBuf> = self
            .owned_artifacts
            .iter()
            .filter(|filename| !current.contains(filename.as_str()))
            .map(|filename| self.outdir.join(filename))
            .collect();
        let manifest_status = match &self.manifest_snapshot {
            PathSnapshot::Regular(contents) if contents == self.manifest_contents.as_bytes() => {
                WriteStatus::Reused
            }
            PathSnapshot::Missing | PathSnapshot::Regular(_) | PathSnapshot::Other => {
                WriteStatus::Written
            }
        };
        let has_staged_artifacts = self
            .staged_paths
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .any(Option::is_some);
        let has_transaction_metadata = !matches!(self.recovery_snapshot, PathSnapshot::Missing)
            || !matches!(self.pending_snapshot, PathSnapshot::Missing);
        if !has_staged_artifacts
            && stale.is_empty()
            && manifest_status == WriteStatus::Reused
            && !has_transaction_metadata
        {
            *commit_started = true;
            self.release_output_lock()?;
            return Ok(PublishResult {
                rewritten_artifacts: 0,
                manifest_status,
                removed: Vec::new(),
            });
        }

        let mut staged_manifest = TempPath::new(stage_file(
            &self.manifest_path,
            self.manifest_contents.as_bytes(),
        )?);
        let mut staged_pending_journal = TempPath::new(stage_file(
            &self.pending_journal_path,
            self.pending_journal_contents.as_bytes(),
        )?);
        sync_directory(self.outdir)?;
        *commit_started = true;

        let staged_pending_path = staged_pending_journal.take();
        if let Err(error) = fs::rename(&staged_pending_path, &self.pending_journal_path) {
            let _ = fs::remove_file(&staged_pending_path);
            return Err(ArtifactError(format!(
                "publish common-object pending journal {}: {error}",
                self.pending_journal_path.display()
            )));
        }
        sync_directory(self.outdir)?;

        // The canonical manifest is the trust boundary. Once final artifact
        // paths can change, it must stay absent until the complete new set is
        // durable; recovery metadata retains every possibly owned artifact.
        self.invalidate_canonical_manifest()?;
        sync_directory(self.outdir)?;

        let mut staged_paths = self
            .staged_paths
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut promotion_index = 0;
        for (artifact, staged_path) in self.plan.artifacts.iter().zip(staged_paths.iter_mut()) {
            let Some(path) = staged_path.as_ref() else {
                continue;
            };
            let final_path = self.outdir.join(artifact.filename());
            hooks.before_artifact_promotion(promotion_index, &final_path)?;
            fs::rename(path, &final_path).map_err(|error| {
                ArtifactError(format!(
                    "promote staged common-object artifact {}: {error}",
                    final_path.display()
                ))
            })?;
            *staged_path = None;
            promotion_index += 1;
        }

        let mut removed = Vec::new();
        for (index, path) in stale.into_iter().enumerate() {
            hooks.before_stale_cleanup(index, &path)?;
            match fs::remove_file(&path) {
                Ok(()) => removed.push(path),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(ArtifactError(format!(
                        "remove stale common-object artifact {}: {error}",
                        path.display()
                    )));
                }
            }
        }
        sync_directory(self.outdir)?;

        hooks.before_manifest_publish(&self.manifest_path)?;
        let staged_manifest_path = staged_manifest.take();
        if let Err(error) = fs::rename(&staged_manifest_path, &self.manifest_path) {
            let _ = fs::remove_file(&staged_manifest_path);
            return Err(ArtifactError(format!(
                "publish common-object manifest {}: {error}",
                self.manifest_path.display()
            )));
        }
        sync_directory(self.outdir)?;
        clear_transaction_file(
            &self.recovery_manifest_path,
            "common-object recovery manifest",
        )?;
        clear_transaction_file(&self.pending_journal_path, "common-object pending journal")?;
        sync_directory(self.outdir)?;
        self.release_output_lock()?;
        Ok(PublishResult {
            rewritten_artifacts: self.rewritten.load(Ordering::Relaxed),
            manifest_status,
            removed,
        })
    }

    fn invalidate_canonical_manifest(&self) -> Result<(), ArtifactError> {
        ensure_snapshot_matches(&self.manifest_path, &self.manifest_snapshot)?;
        match &self.manifest_snapshot {
            PathSnapshot::Missing => {
                if let Some(previous) = &self.previous {
                    if previous.origin == PreviousManifestOrigin::Recovery {
                        ensure_regular_contents(&self.recovery_manifest_path, &previous.contents)?;
                    }
                }
            }
            PathSnapshot::Regular(_) => {
                if self
                    .previous
                    .as_ref()
                    .is_some_and(|previous| previous.origin == PreviousManifestOrigin::Canonical)
                {
                    clear_transaction_file(
                        &self.recovery_manifest_path,
                        "common-object recovery manifest",
                    )?;
                    fs::rename(&self.manifest_path, &self.recovery_manifest_path).map_err(
                        |error| {
                            ArtifactError(format!(
                                "invalidate common-object manifest {}: {error}",
                                self.manifest_path.display()
                            ))
                        },
                    )?;
                } else {
                    fs::remove_file(&self.manifest_path).map_err(|error| {
                        ArtifactError(format!(
                            "invalidate untrusted common-object manifest {}: {error}",
                            self.manifest_path.display()
                        ))
                    })?;
                }
            }
            PathSnapshot::Other => {
                return Err(ArtifactError(format!(
                    "common-object manifest path is not a regular file: {}",
                    self.manifest_path.display()
                )));
            }
        }
        Ok(())
    }

    fn revalidate_reused_artifacts(&self) -> Result<(), ArtifactError> {
        let mut staged_paths = self
            .staged_paths
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let reused_contents = self
            .reused_contents
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for (index, expected) in reused_contents.iter().enumerate() {
            let Some(expected) = expected else {
                continue;
            };
            if staged_paths[index].is_some() {
                continue;
            }
            let final_path = self.outdir.join(self.plan.artifact(index).filename());
            if fs::read(&final_path).is_ok_and(|actual| actual.as_slice() == expected.as_slice()) {
                continue;
            }
            staged_paths[index] = Some(stage_file(&final_path, expected)?);
            self.rewritten.fetch_add(1, Ordering::Relaxed);
        }
        Ok(())
    }

    fn release_output_lock(&self) -> Result<(), ArtifactError> {
        self.output_lock.unlock().map_err(|error| {
            ArtifactError(format!(
                "unlock common-object output directory {}: {error}",
                self.outdir.display()
            ))
        })
    }
}

impl Drop for Publication<'_> {
    fn drop(&mut self) {
        let staged_paths = self
            .staged_paths
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for path in staged_paths.iter_mut().filter_map(Option::take) {
            let _ = fs::remove_file(path);
        }
    }
}

struct TempPath {
    path: Option<PathBuf>,
}

impl TempPath {
    fn new(path: PathBuf) -> Self {
        Self { path: Some(path) }
    }

    fn take(&mut self) -> PathBuf {
        self.path
            .take()
            .expect("temporary path can be consumed only once")
    }
}

impl Drop for TempPath {
    fn drop(&mut self) {
        if let Some(path) = self.path.take() {
            let _ = fs::remove_file(path);
        }
    }
}

fn recovery_manifest_path(manifest_path: &Path) -> Result<PathBuf, ArtifactError> {
    manifest_sidecar_path(manifest_path, "previous")
}

fn pending_journal_path(manifest_path: &Path) -> Result<PathBuf, ArtifactError> {
    manifest_sidecar_path(manifest_path, "pending")
}

fn manifest_sidecar_path(manifest_path: &Path, suffix: &str) -> Result<PathBuf, ArtifactError> {
    let parent = manifest_path.parent().ok_or_else(|| {
        ArtifactError(format!(
            "common-object manifest path has no parent: {}",
            manifest_path.display()
        ))
    })?;
    let filename = manifest_path.file_name().ok_or_else(|| {
        ArtifactError(format!(
            "common-object manifest path has no filename: {}",
            manifest_path.display()
        ))
    })?;
    let mut recovery = std::ffi::OsString::from(".");
    recovery.push(filename);
    recovery.push(".");
    recovery.push(suffix);
    Ok(parent.join(recovery))
}

fn snapshot_path(path: &Path) -> Result<PathSnapshot, ArtifactError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => {
            fs::read(path).map(PathSnapshot::Regular).map_err(|error| {
                ArtifactError(format!(
                    "read common-object manifest {}: {error}",
                    path.display()
                ))
            })
        }
        Ok(_) => Ok(PathSnapshot::Other),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(PathSnapshot::Missing),
        Err(error) => Err(ArtifactError(format!(
            "inspect common-object manifest {}: {error}",
            path.display()
        ))),
    }
}

fn trusted_manifest(contents: &[u8], prefix: &str) -> Option<ArtifactManifest> {
    let contents = std::str::from_utf8(contents).ok()?;
    match parse_manifest(contents, prefix) {
        ParsedManifest::Trusted(manifest) => Some(manifest),
        ParsedManifest::UnknownSchema(_) | ParsedManifest::Malformed => None,
    }
}

fn ensure_snapshot_matches(path: &Path, expected: &PathSnapshot) -> Result<(), ArtifactError> {
    let actual = snapshot_path(path)?;
    if &actual == expected {
        Ok(())
    } else {
        Err(ArtifactError(format!(
            "common-object manifest changed during publication: {}",
            path.display()
        )))
    }
}

fn ensure_regular_contents(path: &Path, expected: &[u8]) -> Result<(), ArtifactError> {
    match snapshot_path(path)? {
        PathSnapshot::Regular(actual) if actual == expected => Ok(()),
        _ => Err(ArtifactError(format!(
            "common-object recovery manifest changed during publication: {}",
            path.display()
        ))),
    }
}

fn clear_transaction_file(path: &Path, kind: &str) -> Result<(), ArtifactError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() || metadata.file_type().is_symlink() => {
            fs::remove_file(path).map_err(|error| {
                ArtifactError(format!("remove {kind} {}: {error}", path.display()))
            })
        }
        Ok(_) => Err(ArtifactError(format!(
            "{kind} path is not a file: {}",
            path.display()
        ))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(ArtifactError(format!(
            "inspect {kind} {}: {error}",
            path.display()
        ))),
    }
}

fn sync_directory(path: &Path) -> Result<(), ArtifactError> {
    fs::File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| {
            ArtifactError(format!(
                "sync common-object output directory {}: {error}",
                path.display()
            ))
        })
}

fn stage_if_changed(
    path: &Path,
    contents: &[u8],
) -> Result<(WriteStatus, Option<PathBuf>), ArtifactError> {
    if fs::read(path).is_ok_and(|existing| existing == contents) {
        return Ok((WriteStatus::Reused, None));
    }
    Ok((WriteStatus::Written, Some(stage_file(path, contents)?)))
}

fn stage_file(path: &Path, contents: &[u8]) -> Result<PathBuf, ArtifactError> {
    let parent = path.parent().ok_or_else(|| {
        ArtifactError(format!(
            "common-object artifact path has no parent: {}",
            path.display()
        ))
    })?;
    fs::create_dir_all(parent).map_err(|error| {
        ArtifactError(format!(
            "create artifact directory {}: {error}",
            parent.display()
        ))
    })?;
    let filename = path.file_name().ok_or_else(|| {
        ArtifactError(format!(
            "common-object artifact path has no filename: {}",
            path.display()
        ))
    })?;

    let mut temp_path = None;
    let mut temp_file = None;
    for _ in 0..128 {
        let id = NEXT_TEMP_FILE.fetch_add(1, Ordering::Relaxed);
        let mut temp_name = filename.to_os_string();
        temp_name.push(format!(".tmp.{}.{id}", std::process::id()));
        let candidate = parent.join(temp_name);
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(file) => {
                temp_path = Some(candidate);
                temp_file = Some(file);
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(ArtifactError(format!(
                    "create temporary artifact for {}: {error}",
                    path.display()
                )));
            }
        }
    }
    let temp_path = temp_path.ok_or_else(|| {
        ArtifactError(format!(
            "could not allocate a temporary artifact beside {}",
            path.display()
        ))
    })?;
    let mut temp_file = temp_file.expect("temporary path and file are created together");
    let write_result = temp_file
        .write_all(contents)
        .and_then(|()| temp_file.sync_all());
    drop(temp_file);
    if let Err(error) = write_result {
        let _ = fs::remove_file(&temp_path);
        return Err(ArtifactError(format!(
            "write temporary artifact for {}: {error}",
            path.display()
        )));
    }
    Ok(temp_path)
}

static NEXT_TEMP_FILE: AtomicU64 = AtomicU64::new(0);

pub fn atomic_write_if_changed(path: &Path, contents: &[u8]) -> Result<WriteStatus, ArtifactError> {
    let (status, staged_path) = stage_if_changed(path, contents)?;
    let Some(staged_path) = staged_path else {
        return Ok(status);
    };
    if let Err(error) = fs::rename(&staged_path, path) {
        let _ = fs::remove_file(&staged_path);
        return Err(ArtifactError(format!(
            "replace common-object artifact {}: {error}",
            path.display()
        )));
    }
    Ok(status)
}

const MANIFEST_SCHEMA_V1: u32 = 1;
pub const MANIFEST_SCHEMA_V2: u32 = 2;
const PENDING_JOURNAL_SCHEMA_V1: u32 = 1;
const PENDING_JOURNAL_SCHEMA_V2: u32 = 2;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestV1 {
    schema_version: u32,
    interface_abi: String,
    build_profile: String,
    tests: Vec<String>,
    artifacts: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestArtifact {
    filename: String,
    role: ArtifactRole,
    owner: String,
    tests: Vec<String>,
    dependencies: Vec<String>,
}

impl ManifestArtifact {
    pub fn filename(&self) -> &str {
        &self.filename
    }

    pub fn role(&self) -> ArtifactRole {
        self.role
    }

    pub fn owner(&self) -> &str {
        &self.owner
    }

    pub fn tests(&self) -> &[String] {
        &self.tests
    }

    pub fn dependencies(&self) -> &[String] {
        &self.dependencies
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestV2 {
    schema_version: u32,
    backend: CodegenBackend,
    layout: CppLayout,
    interface_abi: String,
    build_profile: String,
    tests: Vec<String>,
    artifacts: Vec<ManifestArtifact>,
    placement: PlacementMetrics,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ManifestDocument {
    V1(ManifestV1),
    V2(ManifestV2),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactManifest {
    document: ManifestDocument,
    artifact_filenames: Vec<String>,
    typed_artifacts: Vec<ManifestArtifact>,
}

impl ArtifactManifest {
    pub fn schema_version(&self) -> u32 {
        match &self.document {
            ManifestDocument::V1(_) => MANIFEST_SCHEMA_V1,
            ManifestDocument::V2(_) => MANIFEST_SCHEMA_V2,
        }
    }

    pub fn backend(&self) -> Option<CodegenBackend> {
        match &self.document {
            ManifestDocument::V1(_) => None,
            ManifestDocument::V2(manifest) => Some(manifest.backend),
        }
    }

    pub fn layout(&self) -> Option<CppLayout> {
        match &self.document {
            ManifestDocument::V1(_) => None,
            ManifestDocument::V2(manifest) => Some(manifest.layout),
        }
    }

    pub fn interface_abi(&self) -> &str {
        match &self.document {
            ManifestDocument::V1(manifest) => &manifest.interface_abi,
            ManifestDocument::V2(manifest) => &manifest.interface_abi,
        }
    }

    pub fn build_profile(&self) -> &str {
        match &self.document {
            ManifestDocument::V1(manifest) => &manifest.build_profile,
            ManifestDocument::V2(manifest) => &manifest.build_profile,
        }
    }

    pub fn artifacts(&self) -> &[String] {
        &self.artifact_filenames
    }

    pub fn typed_artifacts(&self) -> &[ManifestArtifact] {
        &self.typed_artifacts
    }

    pub fn tests(&self) -> &[String] {
        match &self.document {
            ManifestDocument::V1(manifest) => &manifest.tests,
            ManifestDocument::V2(manifest) => &manifest.tests,
        }
    }

    pub fn placement(&self) -> Option<&PlacementMetrics> {
        match &self.document {
            ManifestDocument::V1(_) => None,
            ManifestDocument::V2(manifest) => Some(&manifest.placement),
        }
    }

    pub fn native_sources(&self) -> impl Iterator<Item = &str> {
        self.typed_artifacts.iter().filter_map(|artifact| {
            matches!(
                artifact.role,
                ArtifactRole::Common | ArtifactRole::Capsule | ArtifactRole::Registry
            )
            .then_some(artifact.filename.as_str())
        })
    }

    pub fn build_inputs(&self) -> impl Iterator<Item = &str> {
        self.typed_artifacts
            .iter()
            .map(|artifact| artifact.filename.as_str())
    }

    pub fn probe_stub(&self) -> Option<&str> {
        self.typed_artifacts
            .iter()
            .find(|artifact| artifact.role == ArtifactRole::ProbeStub)
            .map(|artifact| artifact.filename.as_str())
    }

    fn to_json_value(&self) -> Result<serde_json::Value, ArtifactError> {
        match &self.document {
            ManifestDocument::V1(manifest) => serde_json::to_value(manifest),
            ManifestDocument::V2(manifest) => serde_json::to_value(manifest),
        }
        .map_err(|error| ArtifactError(format!("render common-object ownership entry: {error}")))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PendingJournal {
    schema_version: u32,
    ownership_manifests: Vec<serde_json::Value>,
}

fn push_unique_manifest(manifests: &mut Vec<ArtifactManifest>, manifest: ArtifactManifest) {
    if !manifests.contains(&manifest) {
        manifests.push(manifest);
    }
}

fn ordered_owned_artifacts(manifests: &[ArtifactManifest]) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut artifacts = Vec::new();
    for manifest in manifests {
        for filename in manifest.artifacts() {
            if seen.insert(filename.clone()) {
                artifacts.push(filename.clone());
            }
        }
    }
    artifacts
}

fn render_pending_journal(manifests: &[ArtifactManifest]) -> Result<String, ArtifactError> {
    let journal = PendingJournal {
        schema_version: PENDING_JOURNAL_SCHEMA_V2,
        ownership_manifests: manifests
            .iter()
            .map(ArtifactManifest::to_json_value)
            .collect::<Result<Vec<_>, _>>()?,
    };
    let mut rendered = serde_json::to_string(&journal).map_err(|error| {
        ArtifactError(format!(
            "render common-object pending ownership journal: {error}"
        ))
    })?;
    rendered.push('\n');
    Ok(rendered)
}

fn parse_pending_journal(contents: &[u8], prefix: &str) -> Option<Vec<ArtifactManifest>> {
    let contents = std::str::from_utf8(contents).ok()?;
    let journal: PendingJournal = serde_json::from_str(contents).ok()?;
    if !matches!(
        journal.schema_version,
        PENDING_JOURNAL_SCHEMA_V1 | PENDING_JOURNAL_SCHEMA_V2
    ) || journal.ownership_manifests.is_empty()
    {
        return None;
    }
    journal
        .ownership_manifests
        .into_iter()
        .map(|manifest| {
            let contents = serde_json::to_string(&manifest).ok()?;
            match parse_manifest(&contents, prefix) {
                ParsedManifest::Trusted(manifest) => Some(manifest),
                ParsedManifest::UnknownSchema(_) | ParsedManifest::Malformed => None,
            }
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParsedManifest {
    Trusted(ArtifactManifest),
    UnknownSchema(u64),
    Malformed,
}

#[derive(Deserialize)]
struct ManifestEnvelope {
    schema_version: u64,
}

pub fn parse_manifest(contents: &str, prefix: &str) -> ParsedManifest {
    let envelope: ManifestEnvelope = match serde_json::from_str(contents) {
        Ok(envelope) => envelope,
        Err(_) => return ParsedManifest::Malformed,
    };
    match envelope.schema_version {
        value if value == u64::from(MANIFEST_SCHEMA_V1) => {
            let manifest: ManifestV1 = match serde_json::from_str(contents) {
                Ok(manifest) => manifest,
                Err(_) => return ParsedManifest::Malformed,
            };
            manifest_v1_is_trusted(manifest, prefix)
                .map_or(ParsedManifest::Malformed, ParsedManifest::Trusted)
        }
        value if value == u64::from(MANIFEST_SCHEMA_V2) => {
            let manifest: ManifestV2 = match serde_json::from_str(contents) {
                Ok(manifest) => manifest,
                Err(_) => return ParsedManifest::Malformed,
            };
            manifest_v2_is_trusted(manifest, prefix)
                .map_or(ParsedManifest::Malformed, ParsedManifest::Trusted)
        }
        other => ParsedManifest::UnknownSchema(other),
    }
}

pub fn read_manifest(path: &Path) -> Result<ArtifactManifest, ArtifactError> {
    let filename = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            ArtifactError(format!(
                "common-object manifest path has no UTF-8 filename: {}",
                path.display()
            ))
        })?;
    let prefix = filename.strip_suffix("artifacts.json").ok_or_else(|| {
        ArtifactError(format!(
            "common-object manifest filename must end in `artifacts.json`: {}",
            path.display()
        ))
    })?;
    let contents = fs::read_to_string(path).map_err(|error| {
        ArtifactError(format!(
            "read common-object manifest {}: {error}",
            path.display()
        ))
    })?;
    match parse_manifest(&contents, prefix) {
        ParsedManifest::Trusted(manifest) => Ok(manifest),
        ParsedManifest::UnknownSchema(schema) => Err(ArtifactError(format!(
            "unsupported common-object manifest schema {schema}: {}",
            path.display()
        ))),
        ParsedManifest::Malformed => Err(ArtifactError(format!(
            "malformed or untrusted common-object manifest: {}",
            path.display()
        ))),
    }
}

fn manifest_v1_is_trusted(manifest: ManifestV1, prefix: &str) -> Option<ArtifactManifest> {
    if !is_legacy_fingerprint(&manifest.interface_abi)
        || !is_legacy_fingerprint(&manifest.build_profile)
    {
        return None;
    }
    let expected = match CommonArtifactPlan::new(prefix, &manifest.tests) {
        Ok(plan) => plan,
        Err(_) => return None,
    };
    if manifest.schema_version != MANIFEST_SCHEMA_V1 {
        return None;
    }
    let base_matches = manifest.artifacts.len() == expected.artifacts.len()
        && manifest
            .artifacts
            .iter()
            .zip(&expected.artifacts)
            .all(|(actual, expected)| actual == expected.filename());
    if base_matches {
        let typed_artifacts = expected.manifest_artifacts();
        return Some(ArtifactManifest {
            artifact_filenames: manifest.artifacts.clone(),
            typed_artifacts,
            document: ManifestDocument::V1(manifest),
        });
    }
    let expected_with_probe = match expected.with_probe_stub() {
        Ok(plan) => plan,
        Err(_) => return None,
    };
    let matches_probe = manifest.artifacts.len() == expected_with_probe.artifacts.len()
        && manifest
            .artifacts
            .iter()
            .zip(&expected_with_probe.artifacts)
            .all(|(actual, expected)| actual == expected.filename());
    matches_probe.then(|| ArtifactManifest {
        artifact_filenames: manifest.artifacts.clone(),
        typed_artifacts: expected_with_probe.manifest_artifacts(),
        document: ManifestDocument::V1(manifest),
    })
}

fn manifest_v2_is_trusted(manifest: ManifestV2, prefix: &str) -> Option<ArtifactManifest> {
    if manifest.schema_version != MANIFEST_SCHEMA_V2
        || manifest.layout != CppLayout::Common
        || !is_legacy_fingerprint(&manifest.interface_abi)
        || !is_legacy_fingerprint(&manifest.build_profile)
        || manifest.placement.capsule_reasons.values().sum::<usize>()
            != manifest.placement.capsule_callables
    {
        return None;
    }
    let common_units = manifest
        .artifacts
        .iter()
        .filter(|artifact| artifact.role == ArtifactRole::Common)
        .map(|artifact| CommonUnitRequest::new(artifact.owner.clone()))
        .collect::<Vec<_>>();
    let capsules = manifest
        .artifacts
        .iter()
        .filter(|artifact| artifact.role == ArtifactRole::Capsule)
        .map(|artifact| CapsuleRequest::new(artifact.owner.clone(), artifact.tests.clone()))
        .collect::<Vec<_>>();
    let mut expected =
        CommonArtifactPlan::from_layout(prefix, &manifest.tests, &common_units, &capsules).ok()?;
    let runtime_header_count = manifest
        .artifacts
        .iter()
        .filter(|artifact| artifact.role == ArtifactRole::RuntimeHeader)
        .count();
    if runtime_header_count != 0 {
        if runtime_header_count != RUNTIME_HEADER_FILENAMES.len() {
            return None;
        }
        expected = expected.with_runtime_headers().ok()?;
    }
    let probe_count = manifest
        .artifacts
        .iter()
        .filter(|artifact| artifact.role == ArtifactRole::ProbeStub)
        .count();
    if probe_count > 1 {
        return None;
    }
    if probe_count == 1 {
        expected = expected.with_probe_stub().ok()?;
    }
    let expected_artifacts = expected.manifest_artifacts();
    if manifest.artifacts != expected_artifacts {
        return None;
    }
    let artifact_filenames = manifest
        .artifacts
        .iter()
        .map(|artifact| artifact.filename.clone())
        .collect();
    Some(ArtifactManifest {
        artifact_filenames,
        typed_artifacts: manifest.artifacts.clone(),
        document: ManifestDocument::V2(manifest),
    })
}

fn validate_manifest_identity(identity: &ManifestIdentity) -> Result<(), ArtifactError> {
    if !is_legacy_fingerprint(&identity.interface_abi)
        || !is_legacy_fingerprint(&identity.build_profile)
    {
        return Err(ArtifactError(
            "common-object manifest fingerprints must be 16 lowercase hexadecimal digits"
                .to_string(),
        ));
    }
    if identity.placement.capsule_reasons.values().sum::<usize>()
        != identity.placement.capsule_callables
    {
        return Err(ArtifactError(
            "common-object capsule placement-reason counts must sum to capsule_callables"
                .to_string(),
        ));
    }
    Ok(())
}

fn is_legacy_fingerprint(value: &str) -> bool {
    value.len() == 16
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

pub fn sanitize_file_component(name: &str) -> String {
    let mut out: String = name
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect();
    if out.is_empty() {
        out.push_str("test");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT_TEMP: AtomicUsize = AtomicUsize::new(0);

    fn temp_dir(label: &str) -> std::path::PathBuf {
        let id = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "harc_common_artifacts_{label}_{}_{}",
            std::process::id(),
            id
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("create test directory");
        path
    }

    fn publish_fixture(
        outdir: &std::path::Path,
        plan: &CommonArtifactPlan,
    ) -> Result<PublishResult, ArtifactError> {
        let publication = plan.begin_publication(outdir, "0123456789abcdef", "fedcba9876543210")?;
        for artifact in plan.artifacts() {
            publication.write(artifact.filename(), artifact.filename().as_bytes())?;
        }
        publication.commit()
    }

    fn v2_identity(backend: CodegenBackend) -> ManifestIdentity {
        ManifestIdentity::new(
            backend,
            CppLayout::Common,
            "0123456789abcdef",
            "fedcba9876543210",
            PlacementMetrics::default(),
        )
    }

    fn publish_v2_fixture(
        outdir: &std::path::Path,
        plan: &CommonArtifactPlan,
        backend: CodegenBackend,
    ) -> Result<PublishResult, ArtifactError> {
        let identity = v2_identity(backend);
        let publication = plan.begin_publication_v2(outdir, &identity)?;
        for artifact in plan.artifacts() {
            publication.write(artifact.filename(), artifact.filename().as_bytes())?;
        }
        publication.commit()
    }

    fn deliver_fixture(publication: &Publication<'_>, plan: &CommonArtifactPlan, generation: &str) {
        for artifact in plan.artifacts() {
            publication
                .write(
                    artifact.filename(),
                    format!("{generation}:{}", artifact.filename()).as_bytes(),
                )
                .unwrap_or_else(|error| panic!("deliver {}: {error}", artifact.filename()));
        }
    }

    #[derive(Default)]
    struct FailCommitAt {
        artifact_promotion: Option<usize>,
        stale_cleanup: Option<usize>,
        manifest_publish: bool,
    }

    impl PublicationCommitHooks for FailCommitAt {
        fn before_artifact_promotion(
            &self,
            index: usize,
            path: &Path,
        ) -> Result<(), ArtifactError> {
            if self.artifact_promotion == Some(index) {
                return Err(ArtifactError(format!(
                    "injected artifact promotion failure at {}",
                    path.display()
                )));
            }
            Ok(())
        }

        fn before_stale_cleanup(&self, index: usize, path: &Path) -> Result<(), ArtifactError> {
            if self.stale_cleanup == Some(index) {
                return Err(ArtifactError(format!(
                    "injected stale cleanup failure at {}",
                    path.display()
                )));
            }
            Ok(())
        }

        fn before_manifest_publish(&self, path: &Path) -> Result<(), ArtifactError> {
            if self.manifest_publish {
                return Err(ArtifactError(format!(
                    "injected manifest publication failure at {}",
                    path.display()
                )));
            }
            Ok(())
        }
    }

    #[test]
    fn plan_preserves_the_v1_artifact_contract() {
        let plan = CommonArtifactPlan::new("tb__", &["T1Add".to_string(), "T2Add".to_string()])
            .expect("valid common artifact plan");

        assert_eq!(plan.manifest_filename(), "tb__artifacts.json");
        assert_eq!(
            plan.artifacts()
                .iter()
                .map(|artifact| (artifact.role(), artifact.filename()))
                .collect::<Vec<_>>(),
            vec![
                (ArtifactRole::Interface, "tb__suite_api.hpp"),
                (ArtifactRole::Common, "tb__runtime.cpp"),
                (ArtifactRole::Capsule, "tb__test_T1Add.cpp"),
                (ArtifactRole::Capsule, "tb__test_T2Add.cpp"),
                (ArtifactRole::Registry, "tb__registry.cpp"),
            ]
        );
        assert_eq!(plan.tests()[0].symbol_stem(), "T1Add");
        assert_eq!(plan.tests()[1].symbol_stem(), "T2Add");
        assert_eq!(plan.common_units()[0].key(), "runtime");
        assert_eq!(plan.capsules()[0].test_indices(), &[0]);
        assert_eq!(plan.capsules()[1].test_indices(), &[1]);
        assert_eq!(plan.tests()[0].capsule_index(), 0);
        assert_eq!(plan.tests()[1].capsule_index(), 1);
    }

    #[test]
    fn probe_stub_is_an_opt_in_manifest_owned_artifact() {
        let base = CommonArtifactPlan::new("tb__", &["ProbeTest".to_string()])
            .expect("valid common artifact plan");
        assert!(base.probe_stub().is_none());

        let plan = base.with_probe_stub().expect("add probe artifact");
        let stub = plan.probe_stub().expect("planned probe stub");
        assert_eq!(stub.role(), ArtifactRole::ProbeStub);
        assert_eq!(stub.filename(), "tb__probe_stub.sv");
        let rendered = plan
            .render_manifest("0123456789abcdef", "fedcba9876543210")
            .expect("render manifest with probe artifact");
        let ParsedManifest::Trusted(manifest) = parse_manifest(&rendered, "tb__") else {
            panic!("probe manifest was not trusted: {rendered}");
        };
        assert_eq!(
            manifest.artifacts().last().map(String::as_str),
            Some("tb__probe_stub.sv")
        );
    }

    #[test]
    fn publication_requires_and_stale_removes_the_probe_stub() {
        let outdir = temp_dir("probe_stub_ownership");
        let probed = CommonArtifactPlan::new("tb__", &["ProbeTest".to_string()])
            .expect("valid common artifact plan")
            .with_probe_stub()
            .expect("add probe artifact");
        let incomplete = probed
            .begin_publication(&outdir, "0123456789abcdef", "fedcba9876543210")
            .expect("start publication");
        for artifact in probed
            .artifacts()
            .iter()
            .filter(|artifact| artifact.role() != ArtifactRole::ProbeStub)
        {
            incomplete
                .write(artifact.filename(), artifact.filename().as_bytes())
                .expect("write non-probe artifact");
        }
        let error = incomplete
            .commit()
            .expect_err("missing probe stub must abort publication");
        assert!(error.to_string().contains("missing artifacts"), "{error}");
        drop(incomplete);

        publish_fixture(&outdir, &probed).expect("publish probed generation");
        let stub_path = outdir.join("tb__probe_stub.sv");
        assert!(stub_path.is_file());

        let plain = CommonArtifactPlan::new("tb__", &["ProbeTest".to_string()])
            .expect("valid common artifact plan");
        let result = publish_fixture(&outdir, &plain).expect("publish probe-less generation");
        assert!(result.removed().contains(&stub_path));
        assert!(!stub_path.exists());
        fs::remove_dir_all(&outdir).expect("remove test directory");
    }

    #[test]
    fn generic_plan_supports_keyed_common_units_and_grouped_capsules() {
        let tests = [
            "First".to_string(),
            "Second".to_string(),
            "Third".to_string(),
        ];
        let plan = CommonArtifactPlan::from_layout(
            "tb__",
            &tests,
            &[
                CommonUnitRequest::new("runtime"),
                CommonUnitRequest::new("shared_helpers"),
            ],
            &[
                CapsuleRequest::new("group_a", vec!["First".to_string(), "Third".to_string()]),
                CapsuleRequest::new("group_b", vec!["Second".to_string()]),
            ],
        )
        .expect("generic common artifact plan");

        assert_eq!(
            plan.artifacts()
                .iter()
                .map(ArtifactSpec::filename)
                .collect::<Vec<_>>(),
            vec![
                "tb__suite_api.hpp",
                "tb__runtime.cpp",
                "tb__shared_helpers.cpp",
                "tb__test_group_a.cpp",
                "tb__test_group_b.cpp",
                "tb__registry.cpp",
            ]
        );
        assert_eq!(
            plan.common_units()
                .iter()
                .map(CommonUnitArtifact::key)
                .collect::<Vec<_>>(),
            vec!["runtime", "shared_helpers"]
        );
        assert_eq!(plan.capsules()[0].test_indices(), &[0, 2]);
        assert_eq!(
            plan.capsules()[0].test_names(),
            &["First".to_string(), "Third".to_string()]
        );
        assert_eq!(plan.capsules()[1].test_indices(), &[1]);
        assert_eq!(plan.tests()[0].capsule_index(), 0);
        assert_eq!(plan.tests()[1].capsule_index(), 1);
        assert_eq!(plan.tests()[2].capsule_index(), 0);
        assert!(
            plan.render_manifest("0123456789abcdef", "fedcba9876543210")
                .is_err(),
            "schema 1 must not silently discard grouping or multiple-common-unit metadata"
        );
        let identity = ManifestIdentity::new(
            CodegenBackend::Tbir,
            CppLayout::Common,
            "0123456789abcdef",
            "fedcba9876543210",
            PlacementMetrics::new(2, 3, BTreeMap::from([("test_body".to_string(), 3)]))
                .expect("valid placement metrics"),
        );
        let manifest = plan
            .render_manifest_v2(&identity)
            .expect("schema 2 preserves generic ownership");
        let ParsedManifest::Trusted(manifest) = parse_manifest(&manifest, "tb__") else {
            panic!("schema-two generic manifest was not trusted: {manifest}");
        };
        assert_eq!(manifest.backend(), Some(CodegenBackend::Tbir));
        assert_eq!(manifest.layout(), Some(CppLayout::Common));
        assert_eq!(
            manifest.native_sources().collect::<Vec<_>>(),
            vec![
                "tb__runtime.cpp",
                "tb__shared_helpers.cpp",
                "tb__test_group_a.cpp",
                "tb__test_group_b.cpp",
                "tb__registry.cpp",
            ]
        );
    }

    #[test]
    fn schema_one_manifest_round_trips_with_legacy_bytes() {
        let plan = CommonArtifactPlan::new("tb__", &["T1Add".to_string(), "T2Add".to_string()])
            .expect("valid common artifact plan");
        let rendered = plan
            .render_manifest("0123456789abcdef", "fedcba9876543210")
            .expect("render manifest");
        assert_eq!(
            rendered,
            "{\"schema_version\":1,\"interface_abi\":\"0123456789abcdef\",\"build_profile\":\"fedcba9876543210\",\"tests\":[\"T1Add\",\"T2Add\"],\"artifacts\":[\"tb__suite_api.hpp\",\"tb__runtime.cpp\",\"tb__test_T1Add.cpp\",\"tb__test_T2Add.cpp\",\"tb__registry.cpp\"]}\n"
        );

        let parsed = parse_manifest(&rendered, "tb__");
        let ParsedManifest::Trusted(manifest) = parsed else {
            panic!("schema-one manifest was not trusted: {parsed:?}");
        };
        assert_eq!(manifest.interface_abi(), "0123456789abcdef");
        assert_eq!(manifest.build_profile(), "fedcba9876543210");
        assert_eq!(
            manifest.artifacts(),
            plan.artifacts()
                .iter()
                .map(ArtifactSpec::filename)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn schema_two_manifest_with_typed_artifacts_is_trusted() {
        let manifest = r#"{"schema_version":2,"backend":"tbir","layout":"common","interface_abi":"0123456789abcdef","build_profile":"fedcba9876543210","tests":["A"],"artifacts":[{"filename":"tb__suite_api.hpp","role":"interface","owner":"suite","tests":[],"dependencies":[]},{"filename":"tb__runtime.cpp","role":"common","owner":"runtime","tests":[],"dependencies":["tb__suite_api.hpp"]},{"filename":"tb__test_A.cpp","role":"capsule","owner":"A","tests":["A"],"dependencies":["tb__suite_api.hpp"]},{"filename":"tb__registry.cpp","role":"registry","owner":"registry","tests":["A"],"dependencies":["tb__suite_api.hpp","tb__runtime.cpp","tb__test_A.cpp"]}],"placement":{"common_callables":0,"capsule_callables":1,"capsule_reasons":{"test_body":1}}}"#;
        assert!(
            matches!(parse_manifest(manifest, "tb__"), ParsedManifest::Trusted(_)),
            "schema-two manifests must be accepted by the shared artifact reader"
        );
    }

    #[test]
    fn schema_two_publication_removes_only_prior_manifest_owned_artifacts() {
        let outdir = temp_dir("schema_two_stale_cleanup");
        let old =
            CommonArtifactPlan::new("tb__", &["A".to_string(), "B".to_string()]).expect("old plan");
        publish_fixture(&outdir, &old).expect("publish legacy generation");
        let unrelated = outdir.join("tb__test_Unowned.cpp");
        fs::write(&unrelated, "unowned").expect("write unowned lookalike");

        let new = CommonArtifactPlan::new("tb__", &["A".to_string()]).expect("new plan");
        let result = publish_v2_fixture(&outdir, &new, CodegenBackend::Tbir)
            .expect("publish replacement generation");
        assert!(result.removed().contains(&outdir.join("tb__test_B.cpp")));
        assert!(!outdir.join("tb__test_B.cpp").exists());
        assert!(
            unrelated.is_file(),
            "lookalike not owned by the manifest was deleted"
        );
        let manifest = read_manifest(&outdir.join("tb__artifacts.json")).expect("read v2 manifest");
        assert_eq!(manifest.backend(), Some(CodegenBackend::Tbir));
        assert_eq!(
            manifest.native_sources().collect::<Vec<_>>(),
            vec!["tb__runtime.cpp", "tb__test_A.cpp", "tb__registry.cpp",]
        );
        fs::remove_dir_all(outdir).expect("remove test directory");
    }

    #[test]
    fn plan_rejects_duplicate_names_and_sanitized_collisions() {
        let duplicate = CommonArtifactPlan::new("tb__", &["Same".to_string(), "Same".to_string()])
            .expect_err("duplicate test names must fail");
        assert!(duplicate.to_string().contains("duplicate test name `Same`"));

        let collision = CommonArtifactPlan::new("tb__", &["A-B".to_string(), "A_B".to_string()])
            .expect_err("sanitized filename collisions must fail");
        assert!(collision
            .to_string()
            .contains("after sanitization as `A_B`"));
    }

    #[test]
    fn generic_plan_requires_exactly_one_capsule_owner_per_test() {
        let tests = ["A".to_string(), "B".to_string()];
        let common = [CommonUnitRequest::new("runtime")];

        let duplicate = CommonArtifactPlan::from_layout(
            "tb__",
            &tests,
            &common,
            &[
                CapsuleRequest::new("one", vec!["A".to_string()]),
                CapsuleRequest::new("two", vec!["A".to_string(), "B".to_string()]),
            ],
        )
        .expect_err("duplicate capsule membership must fail");
        assert!(duplicate.to_string().contains("more than one"));

        let missing = CommonArtifactPlan::from_layout(
            "tb__",
            &tests,
            &common,
            &[CapsuleRequest::new("one", vec!["A".to_string()])],
        )
        .expect_err("missing capsule membership must fail");
        assert!(missing.to_string().contains("does not belong to any"));

        let unknown = CommonArtifactPlan::from_layout(
            "tb__",
            &tests,
            &common,
            &[CapsuleRequest::new(
                "one",
                vec!["A".to_string(), "Missing".to_string()],
            )],
        )
        .expect_err("unknown capsule membership must fail");
        assert!(unknown.to_string().contains("unknown test `Missing`"));
    }

    #[test]
    fn generic_plan_rejects_common_capsule_and_cross_role_filename_collisions() {
        let tests = ["A".to_string(), "B".to_string()];
        let common_collision = CommonArtifactPlan::from_layout(
            "tb__",
            &tests,
            &[
                CommonUnitRequest::new("shared-a"),
                CommonUnitRequest::new("shared_a"),
            ],
            &[
                CapsuleRequest::new("a", vec!["A".to_string()]),
                CapsuleRequest::new("b", vec!["B".to_string()]),
            ],
        )
        .expect_err("sanitized common-unit collision must fail");
        assert!(common_collision
            .to_string()
            .contains("common implementation key"));

        let capsule_collision = CommonArtifactPlan::from_layout(
            "tb__",
            &tests,
            &[CommonUnitRequest::new("runtime")],
            &[
                CapsuleRequest::new("group-a", vec!["A".to_string()]),
                CapsuleRequest::new("group_a", vec!["B".to_string()]),
            ],
        )
        .expect_err("sanitized capsule collision must fail");
        assert!(capsule_collision.to_string().contains("capsule key"));

        let cross_role_collision = CommonArtifactPlan::from_layout(
            "tb__",
            &tests,
            &[CommonUnitRequest::new("test_group")],
            &[CapsuleRequest::new(
                "group",
                vec!["A".to_string(), "B".to_string()],
            )],
        )
        .expect_err("cross-role artifact filename collision must fail");
        assert!(cross_role_collision
            .to_string()
            .contains("artifact filename collision"));
    }

    #[test]
    fn plan_accepts_the_empty_legacy_prefix() {
        let plan = CommonArtifactPlan::new("", &["Only".to_string()])
            .expect("the legacy v1 API permits an unprefixed artifact set");
        assert_eq!(plan.interface().filename(), "suite_api.hpp");
        assert_eq!(plan.common_units()[0].artifact_index(), 1);
        assert_eq!(plan.artifact(1).filename(), "runtime.cpp");
        assert_eq!(plan.capsules()[0].artifact_index(), 2);
        assert_eq!(plan.artifact(2).filename(), "test_Only.cpp");
        assert_eq!(plan.registry().filename(), "registry.cpp");
        assert_eq!(plan.manifest_filename(), "artifacts.json");
    }

    #[test]
    fn plan_rejects_unsafe_or_non_basename_prefixes_before_io() {
        for prefix in ["../", "nested/", "/absolute/", r"nested\\", ".", ".."] {
            let error = CommonArtifactPlan::new(prefix, &["Test".to_string()])
                .expect_err("unsafe prefix must fail during planning");
            assert!(
                error
                    .to_string()
                    .contains("prefix must be empty or one basename"),
                "unexpected error for {prefix:?}: {error}"
            );
        }
    }

    #[test]
    fn malformed_unknown_or_non_owned_manifests_are_never_trusted() {
        for manifest in [
            "not json",
            r#"{"schema_version":2,"interface_abi":"0123456789abcdef","build_profile":"fedcba9876543210","tests":["Old"],"artifacts":["tb__suite_api.hpp","tb__runtime.cpp","tb__test_Old.cpp","tb__registry.cpp"]}"#,
            r#"{"schema_version":1,"interface_abi":"0123456789abcdef","build_profile":"fedcba9876543210","tests":["Old"],"artifacts":["../victim"]}"#,
            r#"{"schema_version":1,"interface_abi":"0123456789abcdef","build_profile":"fedcba9876543210","tests":["Old"],"artifacts":["/tmp/victim"]}"#,
            r#"{"schema_version":1,"interface_abi":"0123456789abcdef","build_profile":"fedcba9876543210","tests":["Old"],"artifacts":["tb__suite_api.hpp","tb__runtime.cpp","tb__test_Old.cpp","tb__registry.cpp","tb__unowned.cpp"]}"#,
            r#"{"schema_version":1,"interface_abi":"0123456789abcdef","build_profile":"fedcba9876543210","tests":["Old"],"artifacts":["tb__suite_api.hpp","tb__runtime.cpp","tb__test_Old.cpp","tb__test_Old.cpp"]}"#,
            r#"{"schema_version":1,"interface_abi":"0123456789abcdef","build_profile":"fedcba9876543210","tests":["A-B","A_B"],"artifacts":[]}"#,
            r#"{"schema_version":1,"schema_version":1,"interface_abi":"0123456789abcdef","build_profile":"fedcba9876543210","tests":["Old"],"artifacts":["tb__suite_api.hpp","tb__runtime.cpp","tb__test_Old.cpp","tb__registry.cpp"]}"#,
            r#"{"schema_version":1,"interface_abi":"0123456789abcdef","build_profile":"fedcba9876543210","tests":["Old"],"artifacts":["tb__suite_api.hpp","tb__runtime.cpp","tb__test_Old.cpp","tb__registry.cpp"],"unexpected":true}"#,
            r#"{"schema_version":2,"backend":"tbir","layout":"common","interface_abi":"0123456789abcdef","build_profile":"fedcba9876543210","tests":["Old"],"artifacts":[{"filename":"tb__suite_api.hpp","role":"interface","owner":"suite","tests":[],"dependencies":[]},{"filename":"tb__runtime.cpp","role":"common","owner":"runtime","tests":[],"dependencies":[]},{"filename":"tb__test_Old.cpp","role":"capsule","owner":"Old","tests":["Old"],"dependencies":["tb__suite_api.hpp"]},{"filename":"tb__registry.cpp","role":"registry","owner":"registry","tests":["Old"],"dependencies":["tb__suite_api.hpp","tb__runtime.cpp","tb__test_Old.cpp"]}],"placement":{"common_callables":0,"capsule_callables":1,"capsule_reasons":{"test_body":1}}}"#,
        ] {
            assert!(
                !matches!(parse_manifest(manifest, "tb__"), ParsedManifest::Trusted(_)),
                "unsafe manifest was trusted: {manifest}"
            );
        }
    }

    #[test]
    fn malformed_or_unknown_pending_journals_never_authorize_deletion() {
        for journal in [
            b"not json".as_slice(),
            br#"{"schema_version":2,"ownership_manifests":[]}"#.as_slice(),
            br#"{"schema_version":1,"ownership_manifests":[]}"#.as_slice(),
            br#"{"schema_version":1,"ownership_manifests":[{"schema_version":1,"interface_abi":"0123456789abcdef","build_profile":"fedcba9876543210","tests":["Old"],"artifacts":["../victim.cpp"]}]}"#.as_slice(),
            br#"{"schema_version":1,"ownership_manifests":[{"schema_version":1,"interface_abi":"0123456789abcdef","build_profile":"fedcba9876543210","tests":["Old"],"artifacts":["tb__suite_api.hpp","tb__runtime.cpp","tb__test_Old.cpp","tb__registry.cpp"]}],"unexpected":true}"#.as_slice(),
        ] {
            assert!(
                parse_pending_journal(journal, "tb__").is_none(),
                "unsafe pending journal was trusted: {}",
                String::from_utf8_lossy(journal)
            );
        }
    }

    #[test]
    fn legacy_hash_profile_anchor_and_registry_bytes_are_stable() {
        assert_eq!(stable_hash_hex(b""), "cbf29ce484222325");
        assert_eq!(stable_hash_hex(b"hello"), "a430d84680aabd0b");
        assert_eq!(
            build_profile_fingerprint(
                false,
                &[
                    "top=StableTop".to_string(),
                    "mt=false".to_string(),
                    "coverage=false".to_string(),
                    "waves=".to_string(),
                ]
            ),
            "f11cada50936fa42"
        );

        let header = "prefix\n// === iface-begin ===\nabc\n// === iface-end ===\nsuffix\n";
        let anchor = AbiAnchor::from_marked_interface(header).expect("marked interface");
        assert_eq!(anchor.digest(), stable_hash_hex(b"\nabc\n"));
        assert_eq!(
            anchor.symbol(),
            format!("harc_suite_abi_{}", anchor.digest())
        );
        let v1_identity = AbiAnchor::from_marked_interface_with_identity(
            header,
            CodegenBackend::V1,
            CppLayout::Common,
            &[
                "runtime_abi=runtime-a".to_string(),
                "trace_mode=disabled".to_string(),
            ],
        )
        .expect("v1 common identity");
        let tbir_identity = AbiAnchor::from_marked_interface_with_identity(
            header,
            CodegenBackend::Tbir,
            CppLayout::Common,
            &[
                "runtime_abi=runtime-a".to_string(),
                "trace_mode=disabled".to_string(),
            ],
        )
        .expect("TB-IR common identity");
        assert_ne!(v1_identity.digest(), anchor.digest());
        assert_ne!(v1_identity.digest(), tbir_identity.digest());
        let changed_interface = AbiAnchor::from_marked_interface_with_identity(
            &header.replace("abc", "changed public declaration"),
            CodegenBackend::V1,
            CppLayout::Common,
            &[
                "runtime_abi=runtime-a".to_string(),
                "trace_mode=disabled".to_string(),
            ],
        )
        .expect("complete public interface identity");
        assert_ne!(v1_identity.digest(), changed_interface.digest());
        let trace_identity = AbiAnchor::from_marked_interface_with_identity(
            header,
            CodegenBackend::V1,
            CppLayout::Common,
            &[
                "runtime_abi=runtime-a".to_string(),
                "trace_mode=vcd".to_string(),
            ],
        )
        .expect("trace ABI identity");
        assert_ne!(v1_identity.digest(), trace_identity.digest());
        let runtime_identity = AbiAnchor::from_marked_interface_with_identity(
            header,
            CodegenBackend::V1,
            CppLayout::Common,
            &[
                "runtime_abi=runtime-b".to_string(),
                "trace_mode=disabled".to_string(),
            ],
        )
        .expect("runtime ABI identity");
        assert_ne!(v1_identity.digest(), runtime_identity.digest());

        let plan = CommonArtifactPlan::new("tb__", &["T1Add".to_string(), "T2Add".to_string()])
            .expect("valid common artifact plan");
        assert_eq!(
            render_registry(&plan, "profile", &anchor),
            format!(
                concat!(
                    "// Auto-generated by harc — do not edit.\n",
                    "// HARC common-object suite registry + dispatcher (issue #643).\n",
                    "// harc build-profile: profile\n\n",
                    "#include <cstring>\n",
                    "#include <string>\n",
                    "#include <vector>\n\n",
                    "#include \"tb__suite_api.hpp\"\n\n",
                    "extern const char {symbol}[];\n",
                    "extern \"C\" const HarcTestDescriptor harc_test_T1Add;\n",
                    "extern \"C\" const HarcTestDescriptor harc_test_T2Add;\n\n",
                    "static const HarcTestDescriptor* const harc_suite_tests[] = {{\n",
                    "    &harc_test_T1Add,\n",
                    "    &harc_test_T2Add,\n",
                    "}};\n\n",
                    "[[maybe_unused]] static const char* const harc_abi_ref_registry = {symbol};\n\n",
                    "int main(int argc, char** argv) {{\n",
                    "    const char* test_sel = harc_rt::log::harc_select_test(argc, argv);\n",
                    "    constexpr size_t harc_suite_test_count = sizeof(harc_suite_tests) / sizeof(harc_suite_tests[0]);\n",
                    "    if (!test_sel) return harc_suite_tests[0]->run(argc, argv);\n",
                    "    for (size_t _i = 0; _i < harc_suite_test_count; ++_i) {{\n",
                    "        if (std::strcmp(test_sel, harc_suite_tests[_i]->name) == 0) {{\n",
                    "            return harc_suite_tests[_i]->run(argc, argv);\n",
                    "        }}\n",
                    "    }}\n",
                    "    std::string avail;\n",
                    "    for (size_t _i = 0; _i < harc_suite_test_count; ++_i) {{\n",
                    "        if (_i) avail += \", \";\n",
                    "        avail += harc_suite_tests[_i]->name;\n",
                    "    }}\n",
                    "    harc_rt::log::harc_report_unknown_test(test_sel, avail.c_str());\n",
                    "    return 1;\n",
                    "}}\n\n",
                ),
                symbol = anchor.symbol()
            )
        );

        let required = render_registry_with_required_abi(&plan, "profile", &anchor);
        assert!(required.contains("#include <cstdio>"));
        assert!(required.contains(&format!(
            "harc_suite_tests[_i]->abi_anchor != {}",
            anchor.symbol()
        )));
        assert!(!required.contains("harc_abi_ref_registry"));
        assert_eq!(
            render_test_descriptor_with_required_abi(&plan.tests()[0]),
            concat!(
                "extern \"C\" const HarcTestDescriptor harc_test_T1Add = {\n",
                "    \"T1Add\",\n",
                "    &run_T1Add,\n",
                "    harc_suite_abi_ANCHOR,\n",
                "};\n",
            )
        );
    }

    #[test]
    fn publication_cleans_only_files_owned_by_a_valid_prior_manifest() {
        let outdir = temp_dir("cleanup");
        let old_plan = CommonArtifactPlan::new("tb__", &["Keep".to_string(), "Remove".to_string()])
            .expect("old plan");
        publish_fixture(&outdir, &old_plan).expect("publish old plan");
        let stale = outdir.join("tb__test_Remove.cpp");
        let unmanaged = outdir.join("tb__unmanaged.cpp");
        fs::write(&unmanaged, b"user data").expect("write unmanaged file");

        let new_plan = CommonArtifactPlan::new("tb__", &["Keep".to_string()]).expect("new plan");
        let result = publish_fixture(&outdir, &new_plan).expect("publish new plan");

        assert!(!stale.exists(), "stale owned capsule was not removed");
        assert!(unmanaged.exists(), "unmanaged file was removed");
        assert_eq!(result.removed(), &[stale]);
        let manifest = fs::read_to_string(outdir.join(new_plan.manifest_filename()))
            .expect("read new manifest");
        assert!(matches!(
            parse_manifest(&manifest, "tb__"),
            ParsedManifest::Trusted(_)
        ));
        fs::remove_dir_all(outdir).ok();
    }

    #[test]
    fn incomplete_publication_cannot_replace_the_previous_manifest() {
        let outdir = temp_dir("incomplete");
        let plan = CommonArtifactPlan::new("tb__", &["Only".to_string()]).expect("artifact plan");
        publish_fixture(&outdir, &plan).expect("publish baseline");
        let manifest_path = outdir.join(plan.manifest_filename());
        let baseline = fs::read(&manifest_path).expect("read baseline manifest");
        let interface_path = outdir.join(plan.interface().filename());
        let baseline_interface =
            fs::read(&interface_path).expect("read baseline interface artifact");

        let publication = plan
            .begin_publication(&outdir, "1111111111111111", "2222222222222222")
            .expect("begin replacement");
        publication
            .write(plan.interface().filename(), b"replacement interface bytes")
            .expect("write first artifact");
        let error = publication
            .commit()
            .expect_err("incomplete publication must fail");
        assert!(error.to_string().contains("publication is incomplete"));
        assert_eq!(
            fs::read(&manifest_path).expect("read retained manifest"),
            baseline
        );
        assert_eq!(
            fs::read(&interface_path).expect("read retained interface artifact"),
            baseline_interface,
            "delivering an artifact must stage it until the complete publication commits"
        );
        drop(publication);
        fs::remove_dir_all(outdir).ok();
    }

    #[test]
    fn mid_update_failure_invalidates_the_old_manifest_and_recovers() {
        let outdir = temp_dir("mid_update");
        let plan = CommonArtifactPlan::new("tb__", &["First".to_string(), "Second".to_string()])
            .expect("artifact plan");
        publish_fixture(&outdir, &plan).expect("publish baseline");
        let manifest_path = outdir.join(plan.manifest_filename());
        let recovery_path = recovery_manifest_path(&manifest_path).expect("recovery path");
        let pending_path = pending_journal_path(&manifest_path).expect("pending path");

        let publication = plan
            .begin_publication(&outdir, "1111111111111111", "2222222222222222")
            .expect("begin replacement");
        deliver_fixture(&publication, &plan, "replacement");
        let error = publication
            .commit_with_hooks(&FailCommitAt {
                artifact_promotion: Some(1),
                ..FailCommitAt::default()
            })
            .expect_err("second artifact promotion must fail");
        assert!(error.to_string().contains("injected artifact promotion"));
        assert!(
            !manifest_path.exists(),
            "a canonical old manifest must not describe the mixed artifact set"
        );
        assert!(pending_path.is_file());
        assert!(
            matches!(
                parse_manifest(
                    &fs::read_to_string(&recovery_path).expect("recovery manifest"),
                    plan.prefix()
                ),
                ParsedManifest::Trusted(_)
            ),
            "the prior ownership manifest must remain available for recovery"
        );
        assert_eq!(
            fs::read_to_string(outdir.join(plan.artifact(0).filename())).unwrap(),
            format!("replacement:{}", plan.artifact(0).filename())
        );
        assert_eq!(
            fs::read_to_string(outdir.join(plan.artifact(1).filename())).unwrap(),
            plan.artifact(1).filename(),
            "the injected failure must occur after one promotion"
        );
        drop(publication);

        let recovery = plan
            .begin_publication(&outdir, "1111111111111111", "2222222222222222")
            .expect("begin recovery");
        deliver_fixture(&recovery, &plan, "replacement");
        recovery.commit().expect("complete recovery");
        for artifact in plan.artifacts() {
            assert_eq!(
                fs::read_to_string(outdir.join(artifact.filename())).unwrap(),
                format!("replacement:{}", artifact.filename())
            );
        }
        assert!(manifest_path.is_file());
        assert!(!recovery_path.exists());
        assert!(!pending_path.exists());
        drop(recovery);
        fs::remove_dir_all(outdir).ok();
    }

    #[test]
    fn partial_multi_stale_cleanup_leaves_no_canonical_manifest_and_recovers() {
        let outdir = temp_dir("multi_stale");
        let old_plan = CommonArtifactPlan::new(
            "tb__",
            &[
                "Keep".to_string(),
                "RemoveFirst".to_string(),
                "RemoveSecond".to_string(),
            ],
        )
        .expect("old plan");
        publish_fixture(&outdir, &old_plan).expect("publish baseline");
        let first_stale = outdir.join("tb__test_RemoveFirst.cpp");
        let second_stale = outdir.join("tb__test_RemoveSecond.cpp");

        let new_plan = CommonArtifactPlan::new("tb__", &["Keep".to_string()]).expect("new plan");
        let manifest_path = outdir.join(new_plan.manifest_filename());
        let recovery_path = recovery_manifest_path(&manifest_path).expect("recovery path");
        let pending_path = pending_journal_path(&manifest_path).expect("pending path");
        let publication = new_plan
            .begin_publication(&outdir, "1111111111111111", "2222222222222222")
            .expect("begin replacement");
        deliver_fixture(&publication, &new_plan, "replacement");
        let error = publication
            .commit_with_hooks(&FailCommitAt {
                stale_cleanup: Some(1),
                ..FailCommitAt::default()
            })
            .expect_err("second stale cleanup must fail");
        assert!(error.to_string().contains("injected stale cleanup"));
        assert!(!manifest_path.exists());
        assert!(
            !first_stale.exists(),
            "the first stale artifact was cleaned"
        );
        assert!(
            second_stale.exists(),
            "the second stale artifact remains for recovery"
        );
        assert!(recovery_path.is_file());
        assert!(pending_path.is_file());
        drop(publication);

        let recovery = new_plan
            .begin_publication(&outdir, "1111111111111111", "2222222222222222")
            .expect("begin cleanup recovery");
        deliver_fixture(&recovery, &new_plan, "replacement");
        recovery.commit().expect("finish stale cleanup recovery");
        assert!(!first_stale.exists());
        assert!(!second_stale.exists());
        assert!(manifest_path.is_file());
        assert!(!recovery_path.exists());
        assert!(!pending_path.exists());
        drop(recovery);
        fs::remove_dir_all(outdir).ok();
    }

    #[test]
    fn manifest_publish_failure_leaves_a_complete_untrusted_set_that_recovers() {
        let outdir = temp_dir("manifest_publish");
        let plan = CommonArtifactPlan::new("tb__", &["Only".to_string()]).expect("artifact plan");
        publish_fixture(&outdir, &plan).expect("publish baseline");
        let manifest_path = outdir.join(plan.manifest_filename());
        let recovery_path = recovery_manifest_path(&manifest_path).expect("recovery path");
        let pending_path = pending_journal_path(&manifest_path).expect("pending path");

        let publication = plan
            .begin_publication(&outdir, "1111111111111111", "2222222222222222")
            .expect("begin replacement");
        deliver_fixture(&publication, &plan, "replacement");
        let error = publication
            .commit_with_hooks(&FailCommitAt {
                manifest_publish: true,
                ..FailCommitAt::default()
            })
            .expect_err("manifest publication must fail");
        assert!(error.to_string().contains("injected manifest publication"));
        assert!(!manifest_path.exists());
        assert!(
            recovery_path.is_file(),
            "recovery metadata remains until the new canonical manifest is durable"
        );
        assert!(pending_path.is_file());
        for artifact in plan.artifacts() {
            assert_eq!(
                fs::read_to_string(outdir.join(artifact.filename())).unwrap(),
                format!("replacement:{}", artifact.filename())
            );
        }
        drop(publication);

        let recovery = plan
            .begin_publication(&outdir, "1111111111111111", "2222222222222222")
            .expect("begin manifest recovery");
        deliver_fixture(&recovery, &plan, "replacement");
        recovery.commit().expect("publish recovered manifest");
        assert!(manifest_path.is_file());
        assert!(!recovery_path.exists());
        assert!(!pending_path.exists());
        drop(recovery);
        fs::remove_dir_all(outdir).ok();
    }

    #[test]
    fn competing_publications_hold_one_exclusive_output_lock() {
        let outdir = temp_dir("exclusive_lock");
        let plan = CommonArtifactPlan::new("tb__", &["Only".to_string()]).expect("artifact plan");
        let first = plan
            .begin_publication(&outdir, "0123456789abcdef", "fedcba9876543210")
            .expect("first publication");
        let competing = fs::File::open(&outdir).expect("open competing directory handle");
        assert!(
            competing.try_lock().is_err(),
            "a competing publisher acquired the locked output directory"
        );
        deliver_fixture(&first, &plan, "first");
        first.commit().expect("commit first publication");
        competing
            .try_lock()
            .expect("successful commit releases the output-directory lock");
        competing.unlock().expect("release competing lock");
        drop(first);
        fs::remove_dir_all(outdir).ok();
    }

    #[test]
    fn recovery_journal_owns_new_artifacts_when_the_retry_uses_a_different_plan() {
        let outdir = temp_dir("changed_retry_plan");
        let old_plan = CommonArtifactPlan::new("tb__", &["A".to_string()]).expect("old plan");
        publish_fixture(&outdir, &old_plan).expect("publish baseline");

        let expanded_plan = CommonArtifactPlan::new("tb__", &["A".to_string(), "B".to_string()])
            .expect("expanded plan");
        let failed = expanded_plan
            .begin_publication(&outdir, "1111111111111111", "2222222222222222")
            .expect("begin expanded publication");
        deliver_fixture(&failed, &expanded_plan, "expanded");
        failed
            .commit_with_hooks(&FailCommitAt {
                artifact_promotion: Some(4),
                ..FailCommitAt::default()
            })
            .expect_err("fail after the new B capsule is promoted");
        let introduced = outdir.join("tb__test_B.cpp");
        let pending_path =
            pending_journal_path(&outdir.join(expanded_plan.manifest_filename())).unwrap();
        assert!(
            introduced.is_file(),
            "new capsule was not promoted before failure"
        );
        assert!(pending_path.is_file());
        drop(failed);

        let recovery = old_plan
            .begin_publication(&outdir, "0123456789abcdef", "fedcba9876543210")
            .expect("begin recovery with original plan");
        for artifact in old_plan.artifacts() {
            recovery
                .write(artifact.filename(), artifact.filename().as_bytes())
                .expect("deliver original artifact");
        }
        recovery.commit().expect("recover original plan");
        assert!(
            !introduced.exists(),
            "the interrupted plan's introduced artifact was orphaned"
        );
        assert!(!pending_path.exists());
        drop(recovery);
        fs::remove_dir_all(outdir).ok();
    }

    #[test]
    fn reused_artifacts_are_revalidated_before_manifest_publication() {
        let outdir = temp_dir("reused_revalidation");
        let plan = CommonArtifactPlan::new("tb__", &["Only".to_string()]).expect("artifact plan");
        publish_fixture(&outdir, &plan).expect("publish baseline");
        let interface_path = outdir.join(plan.interface().filename());

        let publication = plan
            .begin_publication(&outdir, "0123456789abcdef", "fedcba9876543210")
            .expect("begin unchanged publication");
        for artifact in plan.artifacts() {
            assert_eq!(
                publication
                    .write(artifact.filename(), artifact.filename().as_bytes())
                    .expect("deliver unchanged artifact"),
                WriteStatus::Reused
            );
        }
        fs::write(&interface_path, b"changed after reuse classification").unwrap();
        let result = publication.commit().expect("repair and publish");
        assert_eq!(result.rewritten_artifacts(), 1);
        assert_eq!(
            fs::read(&interface_path).unwrap(),
            plan.interface().filename().as_bytes()
        );
        drop(publication);
        fs::remove_dir_all(outdir).ok();
    }

    #[test]
    fn publication_rejects_unplanned_and_duplicate_artifacts() {
        let outdir = temp_dir("delivery_contract");
        let plan = CommonArtifactPlan::new("tb__", &["Only".to_string()]).expect("artifact plan");
        let publication = plan
            .begin_publication(&outdir, "0123456789abcdef", "fedcba9876543210")
            .expect("begin publication");
        assert!(publication.write("not-planned.cpp", b"bad").is_err());

        let interface = plan.interface().filename();
        publication
            .write(interface, b"first")
            .expect("first delivery");
        let duplicate = publication
            .write(interface, b"second")
            .expect_err("duplicate delivery must fail");
        assert!(duplicate.to_string().contains("delivered more than once"));
        drop(publication);
        fs::remove_dir_all(outdir).ok();
    }
}
