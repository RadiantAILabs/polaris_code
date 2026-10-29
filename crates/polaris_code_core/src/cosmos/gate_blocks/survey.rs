//! Read-only marker-survey blocks for repository audits.
//!
//! A survey plan is deliberately separate from the older closed-vocabulary
//! question path: its marker and bounded scope are explicit plan data, so it
//! can audit a task-specific set of files without turning those values into
//! ambient host state.

use std::collections::BTreeSet;
use std::fmt;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use polaris::graph::{Graph, NodeId};
use polaris::system::param::{Out, Res};
use polaris::system::system;
use polaris::system::system::SystemError;
use polaris_cosmos::composition::{
    BlockBuildError, BlockDescription, BlockFactory, CompositionCatalogue,
    CompositionCatalogueError, Effect,
};
use polaris_cosmos::contracts::{
    Assessment, AssessmentConclusion, CompositionBlockId, CompositionBlockKey, ConfigurationEntry,
    ConfigurationSchemaId, ConfigurationSchemaKey, ConfigurationValue, ResolvedConfiguration,
    ResolvedConfigurationRef, Status, StopReason,
};

use super::super::assess::{conclude_sites, site_assessment};
use super::super::block_cards;
use super::super::payload::{CodingWorkspace, EditSite};
use super::behavior_version;

fn describe(
    summary: &str,
    inputs: Vec<polaris_cosmos::composition::StablePortKey>,
    output: polaris_cosmos::composition::StablePortKey,
    example: &str,
) -> BlockDescription {
    BlockDescription::new(summary, inputs, output, example)
        .expect("block description is well-formed")
}

const MAX_SITES_LIMIT: usize = 10_000;
const MAX_FILES_LIMIT: usize = 1_000;
const MAX_FILE_BYTES: usize = 1_048_576;

fn exec(error: impl fmt::Display) -> SystemError {
    SystemError::ExecutionError(error.to_string())
}

fn block_key(id: &str) -> CompositionBlockKey {
    CompositionBlockKey::new(
        CompositionBlockId::new(id).expect("block ID is a valid stable name"),
        behavior_version(),
    )
}

fn schema_key(id: &str) -> ConfigurationSchemaKey {
    ConfigurationSchemaKey::new(
        ConfigurationSchemaId::new(id).expect("schema ID is a valid stable name"),
        behavior_version(),
    )
}

fn map(config: &ResolvedConfigurationRef) -> Result<&[ConfigurationEntry], BlockBuildError> {
    match config.resolved() {
        ResolvedConfiguration::Inline(ConfigurationValue::Map(entries)) => Ok(entries),
        ResolvedConfiguration::Inline(_) => {
            Err(BlockBuildError::new("expected a configuration map"))
        }
        ResolvedConfiguration::ContentAddressed(_) => Err(BlockBuildError::new(
            "content-addressed configuration is not supported",
        )),
    }
}

fn only_keys(entries: &[ConfigurationEntry], allowed: &[&str]) -> Result<(), BlockBuildError> {
    if entries.iter().any(|entry| !allowed.contains(&entry.key())) {
        return Err(BlockBuildError::new(
            "configuration contains an unknown field",
        ));
    }
    Ok(())
}

fn required<'a>(
    entries: &'a [ConfigurationEntry],
    key: &str,
) -> Result<&'a ConfigurationValue, BlockBuildError> {
    entries
        .iter()
        .find(|entry| entry.key() == key)
        .map(ConfigurationEntry::value)
        .ok_or_else(|| BlockBuildError::new(format!("configuration is missing '{key}'")))
}

fn optional<'a>(entries: &'a [ConfigurationEntry], key: &str) -> Option<&'a ConfigurationValue> {
    entries
        .iter()
        .find(|entry| entry.key() == key)
        .map(ConfigurationEntry::value)
}

fn string(value: &ConfigurationValue) -> Result<&str, BlockBuildError> {
    match value {
        ConfigurationValue::String(value) if !value.is_empty() => Ok(value),
        ConfigurationValue::String(_) => Err(BlockBuildError::new("strings must not be empty")),
        _ => Err(BlockBuildError::new("expected a non-empty string")),
    }
}

fn string_list(value: &ConfigurationValue) -> Result<Vec<String>, BlockBuildError> {
    match value {
        ConfigurationValue::List(values) if !values.is_empty() => values
            .iter()
            .map(string)
            .map(|value| value.map(str::to_owned))
            .collect(),
        ConfigurationValue::List(_) => Err(BlockBuildError::new("paths must not be empty")),
        _ => Err(BlockBuildError::new("expected a non-empty list of paths")),
    }
}

/// The bounded tree region a marker scan examines.
#[derive(Debug, Clone)]
enum ScanScope {
    /// Every non-hidden, non-target file under the sandbox root.
    Root,
    /// Exactly these sandbox-relative files.
    Paths(Vec<String>),
}

/// The per-instance marker-scan configuration.
#[derive(Debug, Clone)]
struct ScanMarkersConfig {
    scope: ScanScope,
    marker: String,
    max_sites: usize,
}

impl ScanMarkersConfig {
    fn decode(config: &ResolvedConfigurationRef) -> Result<Self, BlockBuildError> {
        let entries = map(config)?;
        only_keys(entries, &["paths", "root", "marker", "max_sites"])?;
        let marker = string(required(entries, "marker")?)?.to_owned();
        let max_sites = match required(entries, "max_sites")? {
            ConfigurationValue::Unsigned(value) => usize::try_from(*value)
                .map_err(|_| BlockBuildError::new("max_sites does not fit in usize"))?,
            _ => {
                return Err(BlockBuildError::new(
                    "max_sites must be an unsigned integer",
                ));
            }
        };
        if max_sites == 0 || max_sites > MAX_SITES_LIMIT {
            return Err(BlockBuildError::new(format!(
                "max_sites must be in 1..={MAX_SITES_LIMIT}"
            )));
        }
        let scope = match (optional(entries, "paths"), optional(entries, "root")) {
            (Some(paths), None) => ScanScope::Paths(string_list(paths)?),
            (None, Some(ConfigurationValue::Bool(true))) => ScanScope::Root,
            (None, Some(ConfigurationValue::Bool(false))) => {
                return Err(BlockBuildError::new("root must be true when supplied"));
            }
            (None, Some(_)) => return Err(BlockBuildError::new("root must be a boolean")),
            (Some(_), Some(_)) => {
                return Err(BlockBuildError::new("supply exactly one of paths or root"));
            }
            (None, None) => {
                return Err(BlockBuildError::new(
                    "configuration is missing paths or root",
                ));
            }
        };
        Ok(Self {
            scope,
            marker,
            max_sites,
        })
    }
}

fn scan_error(detail: impl fmt::Display) -> SystemError {
    exec(format!("marker scan: {detail}"))
}

fn read_bounded(path: &Path, label: &str) -> Result<String, SystemError> {
    let metadata = std::fs::metadata(path)
        .map_err(|error| scan_error(format!("cannot inspect {label}: {error}")))?;
    if !metadata.is_file() {
        return Err(scan_error(format!("{label} is not a regular file")));
    }
    if metadata.len() > MAX_FILE_BYTES as u64 {
        return Err(scan_error(format!(
            "{label} exceeds the {MAX_FILE_BYTES}-byte per-file limit"
        )));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    std::fs::File::open(path)
        .map_err(|error| scan_error(format!("cannot open {label}: {error}")))?
        .take((MAX_FILE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| scan_error(format!("cannot read {label}: {error}")))?;
    if bytes.len() > MAX_FILE_BYTES {
        return Err(scan_error(format!(
            "{label} exceeds the {MAX_FILE_BYTES}-byte per-file limit"
        )));
    }
    String::from_utf8(bytes).map_err(|error| scan_error(format!("{label} is not UTF-8: {error}")))
}

fn add_sites(
    contents: &str,
    path: String,
    marker: &str,
    max_sites: usize,
    sites: &mut Vec<EditSite>,
) -> bool {
    for (index, line) in contents.lines().enumerate() {
        if line.contains(marker) {
            sites.push(EditSite::new(
                path.clone(),
                index + 1,
                Status::Indeterminate,
            ));
            if sites.len() == max_sites {
                return true;
            }
        }
    }
    false
}

fn enforce_file_budget(files_scanned: &mut usize) -> Result<(), SystemError> {
    if *files_scanned == MAX_FILES_LIMIT {
        return Err(scan_error(format!(
            "examining more than {MAX_FILES_LIMIT} files is not permitted"
        )));
    }
    *files_scanned += 1;
    Ok(())
}

fn relative_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn scan_paths(
    workspace: &CodingWorkspace,
    paths: &[String],
    marker: &str,
    max_sites: usize,
) -> Result<Vec<EditSite>, SystemError> {
    let mut files_scanned = 0;
    let mut seen = BTreeSet::<PathBuf>::new();
    let mut sites = Vec::new();
    for path in paths {
        let resolved = workspace
            .config()
            .resolve_path(path)
            .ok_or_else(|| scan_error(format!("{path} escapes the workspace")))?;
        if !seen.insert(resolved.clone()) {
            continue;
        }
        enforce_file_budget(&mut files_scanned)?;
        let contents = read_bounded(&resolved, path)?;
        if add_sites(&contents, path.clone(), marker, max_sites, &mut sites) {
            break;
        }
    }
    Ok(sites)
}

fn scan_root(
    workspace: &CodingWorkspace,
    marker: &str,
    max_sites: usize,
) -> Result<Vec<EditSite>, SystemError> {
    let root = workspace
        .config()
        .working_dir
        .canonicalize()
        .map_err(|error| scan_error(format!("cannot resolve workspace root: {error}")))?;
    let mut files_scanned = 0;
    let mut sites = Vec::new();
    for entry in walkdir::WalkDir::new(&root)
        .follow_links(false)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|entry| {
            if entry.depth() == 0 {
                return true;
            }
            let name = entry.file_name().to_string_lossy();
            !name.starts_with('.') && name != "target"
        })
    {
        let entry = entry.map_err(|error| scan_error(format!("cannot walk workspace: {error}")))?;
        if entry.file_type().is_symlink() || !entry.file_type().is_file() {
            continue;
        }
        enforce_file_budget(&mut files_scanned)?;
        let path = relative_path(&root, entry.path());
        let contents = read_bounded(entry.path(), &path)?;
        if add_sites(&contents, path, marker, max_sites, &mut sites) {
            break;
        }
    }
    Ok(sites)
}

#[system]
async fn scan_markers(
    workspace: Res<CodingWorkspace>,
    config: Out<ScanMarkersConfig>,
) -> Result<Vec<EditSite>, SystemError> {
    let mut sites = match &config.scope {
        ScanScope::Root => scan_root(&workspace, &config.marker, config.max_sites)?,
        ScanScope::Paths(paths) => scan_paths(&workspace, paths, &config.marker, config.max_sites)?,
    };
    sites.sort_by(|left, right| left.path.cmp(&right.path).then(left.line.cmp(&right.line)));
    sites.truncate(config.max_sites);
    Ok(sites)
}

#[system]
async fn assess_sites(sites: Out<Vec<EditSite>>) -> Result<Vec<Assessment<EditSite>>, SystemError> {
    sites
        .iter()
        .cloned()
        .map(site_assessment)
        .collect::<Result<Vec<_>, _>>()
        .map_err(exec)
}

#[system]
async fn answer_site_audit(
    assessments: Out<Vec<Assessment<EditSite>>>,
) -> Result<AssessmentConclusion, SystemError> {
    conclude_sites(
        assessments.iter().cloned().collect(),
        Vec::new(),
        StopReason::GoalMet,
    )
    .map_err(exec)
}

/// Scans a bounded task-specific file scope for a literal marker.
///
/// Configuration is a strict map containing `marker`, `max_sites`, and exactly
/// one of `paths` (a non-empty list of sandbox-relative paths) or `root: true`.
/// The output is one indeterminate [`EditSite`] per marker occurrence; the next
/// block turns those scan facts into the assessment ledger.
pub struct ScanMarkersBlock;

impl BlockFactory for ScanMarkersBlock {
    fn build(
        &self,
        graph: &mut Graph,
        config: &ResolvedConfigurationRef,
    ) -> Result<NodeId, BlockBuildError> {
        let config = ScanMarkersConfig::decode(config)?;
        graph.add_system_node(move || {
            let config = config.clone();
            async move { config }
        });
        Ok(graph.add_system_node(scan_markers))
    }
}

/// Returns [`ScanMarkersBlock`]'s registered key.
#[must_use]
pub fn scan_markers_key() -> CompositionBlockKey {
    block_key("code.scan-markers")
}

/// Returns [`ScanMarkersBlock`]'s configuration schema key.
#[must_use]
pub fn scan_markers_schema() -> ConfigurationSchemaKey {
    schema_key("polaris-code.gate-blocks.scan-markers-config")
}

/// Assesses each marker scan site without changing the workspace.
pub struct AssessSitesBlock;

impl BlockFactory for AssessSitesBlock {
    fn build(
        &self,
        graph: &mut Graph,
        _config: &ResolvedConfigurationRef,
    ) -> Result<NodeId, BlockBuildError> {
        Ok(graph.add_system_node(assess_sites))
    }
}

/// Returns [`AssessSitesBlock`]'s registered key.
#[must_use]
pub fn assess_sites_key() -> CompositionBlockKey {
    block_key("code.assess-sites")
}

/// Concludes a marker-site assessment ledger in the coding contract.
pub struct AnswerSiteAuditBlock;

impl BlockFactory for AnswerSiteAuditBlock {
    fn build(
        &self,
        graph: &mut Graph,
        _config: &ResolvedConfigurationRef,
    ) -> Result<NodeId, BlockBuildError> {
        Ok(graph.add_system_node(answer_site_audit))
    }
}

/// Returns [`AnswerSiteAuditBlock`]'s registered key.
#[must_use]
pub fn answer_site_audit_key() -> CompositionBlockKey {
    block_key("code.answer-site-audit")
}

/// Every block key this module registers.
#[must_use]
pub fn keys() -> Vec<CompositionBlockKey> {
    vec![
        scan_markers_key(),
        assess_sites_key(),
        answer_site_audit_key(),
    ]
}

/// Registers every marker-survey block in the sealed coding catalogue.
pub(super) fn register_all(
    catalogue: &mut CompositionCatalogue,
) -> Result<(), CompositionCatalogueError> {
    catalogue.register_block(
        scan_markers_key(),
        Arc::new(ScanMarkersBlock),
        scan_markers_schema(),
        Effect::ReadOnly,
    )?;
    catalogue.describe_block(
        &scan_markers_key(),
        describe(
            "Scans a bounded, configured file scope for a literal marker and reports one \
             indeterminate edit site per occurrence.",
            Vec::new(),
            block_cards::marker_scan_sites_port(),
            "first step of a marker-audit survey plan, configured with a marker and either \
             paths or root",
        ),
    )?;
    let schema = super::no_config_schema();
    catalogue.register_block(
        assess_sites_key(),
        Arc::new(AssessSitesBlock),
        schema.clone(),
        Effect::ReadOnly,
    )?;
    catalogue.describe_block(
        &assess_sites_key(),
        describe(
            "Assesses each marker-scan edit site without changing the workspace.",
            Vec::new(),
            block_cards::site_audit_assessments_port(),
            "wired after code.scan-markers to turn its sites into an assessment ledger",
        ),
    )?;
    catalogue.register_block(
        answer_site_audit_key(),
        Arc::new(AnswerSiteAuditBlock),
        schema,
        Effect::ReadOnly,
    )?;
    catalogue.describe_block(
        &answer_site_audit_key(),
        describe(
            "Concludes a marker-site assessment ledger in the coding contract.",
            Vec::new(),
            block_cards::assessment_conclusion_port(),
            "final step of a marker-audit survey plan, after code.assess-sites",
        ),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cosmos::payload::CodingWorkspace;
    use crate::cosmos::task::{CodingIntent, CodingTask};
    use polaris::graph::GraphExecutor;
    use polaris::system::param::SystemContext;

    fn scan_config(entries: Vec<ConfigurationEntry>) -> ResolvedConfigurationRef {
        ResolvedConfigurationRef::inline(
            scan_markers_schema(),
            ConfigurationValue::map(entries).expect("canonical configuration"),
        )
        .expect("resolved configuration")
    }

    #[test]
    fn scan_configuration_requires_one_bounded_scope() {
        let config = scan_config(vec![
            ConfigurationEntry::new("marker", ConfigurationValue::String("TODO".to_owned()))
                .expect("entry"),
            ConfigurationEntry::new("max_sites", ConfigurationValue::Unsigned(3)).expect("entry"),
        ]);
        assert!(ScanMarkersConfig::decode(&config).is_err());
    }

    #[test]
    fn marker_scan_refuses_a_file_that_exceeds_its_byte_budget() {
        let dir = tempfile::tempdir().expect("temporary workspace");
        std::fs::write(dir.path().join("large.rs"), vec![b'x'; MAX_FILE_BYTES + 1])
            .expect("fixture");
        let task = CodingTask::from_statement(
            "large-marker-audit",
            dir.path(),
            "audit markers",
            CodingIntent::Answer,
            crate::cosmos::task::SuccessCheck::stated_edits(),
        )
        .expect("task");
        let workspace = CodingWorkspace::for_task(&task);
        let result = scan_paths(&workspace, &["large.rs".to_owned()], "TODO", 1);
        assert!(result.is_err(), "large files must not be read into a scan");
    }

    #[test]
    fn marker_scan_refuses_a_tree_that_exceeds_its_file_budget() {
        let dir = tempfile::tempdir().expect("temporary workspace");
        for index in 0..=MAX_FILES_LIMIT {
            std::fs::write(dir.path().join(format!("file-{index:04}.rs")), "// clear\n")
                .expect("fixture");
        }
        let task = CodingTask::from_statement(
            "large-tree-marker-audit",
            dir.path(),
            "audit markers",
            CodingIntent::Answer,
            crate::cosmos::task::SuccessCheck::stated_edits(),
        )
        .expect("task");
        let workspace = CodingWorkspace::for_task(&task);
        let result = scan_root(&workspace, "TODO", 1);
        assert!(result.is_err(), "over-budget trees must be refused");
    }

    #[test]
    fn marker_scan_stops_after_reaching_the_requested_site_limit() {
        let dir = tempfile::tempdir().expect("temporary workspace");
        std::fs::write(dir.path().join("a-first.rs"), "// TODO\n").expect("fixture");
        std::fs::write(
            dir.path().join("z-large.rs"),
            vec![b'x'; MAX_FILE_BYTES + 1],
        )
        .expect("fixture");
        let task = CodingTask::from_statement(
            "short-marker-audit",
            dir.path(),
            "audit markers",
            CodingIntent::Answer,
            crate::cosmos::task::SuccessCheck::stated_edits(),
        )
        .expect("task");
        let workspace = CodingWorkspace::for_task(&task);
        let sites = scan_root(&workspace, "TODO", 1).expect("first site ends the scan");
        assert_eq!(sites.len(), 1);
        assert_eq!(sites[0].path, "a-first.rs");
    }

    #[tokio::test]
    async fn marker_scan_assessment_and_answer_leave_the_tree_unchanged() {
        let dir = tempfile::tempdir().expect("temporary workspace");
        std::fs::write(dir.path().join("alpha.rs"), "// TODO: alpha\n").expect("fixture");
        std::fs::write(dir.path().join("beta.rs"), "// TODO: beta\n").expect("fixture");
        let task = CodingTask::from_statement(
            "marker-audit",
            dir.path(),
            "audit markers",
            CodingIntent::Answer,
            crate::cosmos::task::SuccessCheck::stated_edits(),
        )
        .expect("task");
        let workspace = CodingWorkspace::for_task(&task);
        let config = scan_config(vec![
            ConfigurationEntry::new("marker", ConfigurationValue::String("TODO".to_owned()))
                .expect("entry"),
            ConfigurationEntry::new("max_sites", ConfigurationValue::Unsigned(2)).expect("entry"),
            ConfigurationEntry::new(
                "paths",
                ConfigurationValue::List(vec![
                    ConfigurationValue::String("alpha.rs".to_owned()),
                    ConfigurationValue::String("beta.rs".to_owned()),
                ]),
            )
            .expect("entry"),
        ]);
        let mut graph = Graph::new();
        ScanMarkersBlock
            .build(&mut graph, &config)
            .expect("scan block builds");
        graph.add_system_node(assess_sites);
        graph.add_system_node(answer_site_audit);
        let mut context = SystemContext::new();
        context.insert(workspace);
        let result = GraphExecutor::new()
            .execute(&graph, &mut context, None, None)
            .await
            .expect("survey executes");
        assert_eq!(
            result
                .output::<AssessmentConclusion>()
                .expect("survey concludes")
                .conclusion_kind(),
            polaris_cosmos::contracts::ConclusionKind::Unresolved
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("alpha.rs")).expect("read"),
            "// TODO: alpha\n"
        );
    }
}
