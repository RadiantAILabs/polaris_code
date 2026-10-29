//! Registration, snapshot access, digest, and admissibility behavior for
//! descriptive block metadata on the composition catalogue.

use polaris::graph::{Graph, NodeId};
use polaris::system::plugin::Version;
use polaris_cosmos::composition::{
    BlockBuildError, BlockDescription, BlockFactory, CompositionCatalogue,
    CompositionCatalogueError, Effect, FactKey, StablePortId, StablePortKey,
};
use polaris_cosmos::contracts::{
    CompositionBlockId, CompositionBlockKey, CompositionCatalogId, ConfigurationSchemaId,
    ConfigurationSchemaKey, ResolvedConfigurationRef,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Marker;

async fn emit_marker() -> Marker {
    Marker
}

struct NoopBlock;

impl BlockFactory for NoopBlock {
    fn build(
        &self,
        graph: &mut Graph,
        _config: &ResolvedConfigurationRef,
    ) -> Result<NodeId, BlockBuildError> {
        Ok(graph.add_system_node(emit_marker))
    }
}

fn version() -> Version {
    Version::new(1, 0, 0)
}

fn block_key(id: &str) -> CompositionBlockKey {
    CompositionBlockKey::new(CompositionBlockId::new(id).unwrap(), version())
}

fn schema() -> ConfigurationSchemaKey {
    ConfigurationSchemaKey::new(
        ConfigurationSchemaId::new("tests.block-description.schema").unwrap(),
        version(),
    )
}

fn port(id: &str) -> StablePortKey {
    StablePortKey::new(StablePortId::new(id).unwrap(), version())
}

fn fact(id: &str) -> FactKey {
    FactKey::new(id).unwrap()
}

fn marker_port() -> StablePortKey {
    port("tests.block-description.marker")
}

fn description(example: &str) -> BlockDescription {
    BlockDescription::new("emits a marker value", Vec::new(), marker_port(), example).unwrap()
}

fn catalogue(id: &str) -> CompositionCatalogue {
    CompositionCatalogue::new(CompositionCatalogId::new(id).unwrap(), version())
}

#[test]
fn describing_an_unregistered_block_is_refused() {
    let mut catalogue = catalogue("tests.block-description.unregistered");
    let key = block_key("tests.block-description.absent");
    let error = catalogue
        .describe_block(&key, description("absent example"))
        .map(|_| ())
        .unwrap_err();
    assert_eq!(error, CompositionCatalogueError::UnknownBlock(key));
}

#[test]
fn describing_a_block_twice_is_refused() {
    let mut catalogue = catalogue("tests.block-description.duplicate");
    let key = block_key("tests.block-description.double-described");
    catalogue
        .register_block(
            key.clone(),
            std::sync::Arc::new(NoopBlock),
            schema(),
            Effect::ReadOnly,
        )
        .unwrap();
    catalogue
        .describe_block(&key, description("first"))
        .unwrap();
    let error = catalogue
        .describe_block(&key, description("second"))
        .map(|_| ())
        .unwrap_err();
    assert_eq!(error, CompositionCatalogueError::DuplicateDescription(key));
}

#[test]
fn a_block_registered_without_a_description_still_seals_and_looks_up_as_none() {
    let mut catalogue = catalogue("tests.block-description.undescribed");
    let key = block_key("tests.block-description.no-description");
    catalogue
        .register_block(
            key.clone(),
            std::sync::Arc::new(NoopBlock),
            schema(),
            Effect::ReadOnly,
        )
        .unwrap();
    let snapshot = catalogue.seal().unwrap();
    assert!(snapshot.block(&key).is_some());
    assert!(snapshot.block_description(&key).is_none());
}

#[test]
fn a_sealed_snapshot_exposes_a_registered_description() {
    let mut catalogue = catalogue("tests.block-description.described");
    let key = block_key("tests.block-description.described-block");
    catalogue
        .register_block(
            key.clone(),
            std::sync::Arc::new(NoopBlock),
            schema(),
            Effect::ReadOnly,
        )
        .unwrap();
    catalogue
        .describe_block(
            &key,
            BlockDescription::new(
                "emits a marker value",
                Vec::new(),
                marker_port(),
                "configured with no inputs, used as a plan's first step",
            )
            .unwrap()
            .requires(fact("tests.block-description.needs-x")),
        )
        .unwrap();
    let snapshot = catalogue.seal().unwrap();
    let described = snapshot.block_description(&key).unwrap();
    assert_eq!(described.summary(), "emits a marker value");
    assert!(described.inputs().is_empty());
    assert_eq!(described.output(), &marker_port());
    assert_eq!(
        described.example(),
        "configured with no inputs, used as a plan's first step"
    );
    assert!(
        described
            .required_facts()
            .contains(&fact("tests.block-description.needs-x"))
    );
}

#[test]
fn block_description_rejects_an_empty_summary_or_example() {
    assert!(BlockDescription::new("", Vec::new(), marker_port(), "example").is_err());
    assert!(BlockDescription::new("summary", Vec::new(), marker_port(), "  ").is_err());
}

#[test]
fn describing_a_wired_block_names_its_input_port() {
    let mut catalogue = catalogue("tests.block-description.wired");
    let producer_key = block_key("tests.block-description.wired-producer");
    let consumer_key = block_key("tests.block-description.wired-consumer");
    catalogue
        .register_block(
            producer_key.clone(),
            std::sync::Arc::new(NoopBlock),
            schema(),
            Effect::ReadOnly,
        )
        .unwrap();
    catalogue
        .register_block(
            consumer_key.clone(),
            std::sync::Arc::new(NoopBlock),
            schema(),
            Effect::ReadOnly,
        )
        .unwrap();
    catalogue
        .describe_block(
            &producer_key,
            description("produces the marker other blocks read"),
        )
        .unwrap();
    catalogue
        .describe_block(
            &consumer_key,
            BlockDescription::new(
                "reads a wired marker and re-emits one",
                vec![marker_port()],
                marker_port(),
                "wired downstream of the producer in a Seq",
            )
            .unwrap(),
        )
        .unwrap();
    let snapshot = catalogue.seal().unwrap();
    let consumer = snapshot.block_description(&consumer_key).unwrap();
    assert_eq!(consumer.inputs(), &[marker_port()]);
}

#[test]
fn admissible_blocks_lists_only_described_blocks_whose_facts_are_satisfied() {
    let mut catalogue = catalogue("tests.block-description.admissible");
    let free_key = block_key("tests.block-description.free");
    let gated_key = block_key("tests.block-description.gated");
    let doubly_gated_key = block_key("tests.block-description.doubly-gated");
    let undescribed_key = block_key("tests.block-description.undescribed-block");

    for key in [&free_key, &gated_key, &doubly_gated_key, &undescribed_key] {
        catalogue
            .register_block(
                key.clone(),
                std::sync::Arc::new(NoopBlock),
                schema(),
                Effect::ReadOnly,
            )
            .unwrap();
    }
    catalogue
        .describe_block(&free_key, description("needs nothing"))
        .unwrap();
    catalogue
        .describe_block(
            &gated_key,
            description("needs fact-a").requires(fact("tests.block-description.fact-a")),
        )
        .unwrap();
    catalogue
        .describe_block(
            &doubly_gated_key,
            description("needs fact-a and fact-b")
                .requires(fact("tests.block-description.fact-a"))
                .requires(fact("tests.block-description.fact-b")),
        )
        .unwrap();
    // undescribed_key is registered but deliberately never described.

    let snapshot = catalogue.seal().unwrap();

    let with_no_facts: Vec<_> = snapshot
        .admissible_blocks(|_| false)
        .into_iter()
        .map(|(key, _)| key.clone())
        .collect();
    assert_eq!(with_no_facts, vec![free_key.clone()]);

    let with_fact_a: Vec<_> = snapshot
        .admissible_blocks(|f| *f == fact("tests.block-description.fact-a"))
        .into_iter()
        .map(|(key, _)| key.clone())
        .collect();
    assert_eq!(with_fact_a, vec![free_key.clone(), gated_key.clone()]);

    let with_both_facts: Vec<_> = snapshot
        .admissible_blocks(|f| {
            *f == fact("tests.block-description.fact-a")
                || *f == fact("tests.block-description.fact-b")
        })
        .into_iter()
        .map(|(key, _)| key.clone())
        .collect();
    assert_eq!(
        with_both_facts,
        vec![
            doubly_gated_key.clone(),
            free_key.clone(),
            gated_key.clone()
        ]
    );
    assert!(!with_both_facts.contains(&undescribed_key));
}

#[test]
fn a_blocks_description_participates_in_the_catalogue_digest() {
    let mut undescribed = catalogue("tests.block-description.digest");
    let key = block_key("tests.block-description.digest-block");
    undescribed
        .register_block(
            key.clone(),
            std::sync::Arc::new(NoopBlock),
            schema(),
            Effect::ReadOnly,
        )
        .unwrap();
    let undescribed_snapshot = undescribed.seal().unwrap();

    let mut described = catalogue("tests.block-description.digest");
    described
        .register_block(
            key.clone(),
            std::sync::Arc::new(NoopBlock),
            schema(),
            Effect::ReadOnly,
        )
        .unwrap();
    described
        .describe_block(&key, description("now described"))
        .unwrap();
    let described_snapshot = described.seal().unwrap();

    assert_ne!(undescribed_snapshot.digest(), described_snapshot.digest());
}
