//! Which models this host can reach, and how a named one is resolved.
//!
//! The set is host configuration, not a runtime discovery: `polaris-ai`'s
//! [`ModelRegistry`](polaris::models::ModelRegistry) knows which *providers*
//! are registered, and an [`LlmProvider`] has no "list your models" method, so
//! asking the provider is not available to a consumer of the framework. The
//! source of truth is therefore declared here and overridable per run:
//!
//! | Layer | Source | Meaning |
//! |-------|--------|---------|
//! | The set | the provider's built-in ids ([`BUILTIN_MODELS`], [`BUILTIN_OPENAI_MODELS`]), replaced wholesale by `POLARIS_CODE_MODELS` | every `provider/model` id this run may select |
//! | The default | `POLARIS_CODE_MODEL`, else the provider's default ([`DEFAULT_MODEL`], [`DEFAULT_OPENAI_MODEL`], [`DEFAULT_BEDROCK_MODEL`]) | the id a run uses when it names none |
//! | Reachability | the registered provider names | an id whose provider is not registered is dropped |
//!
//! Resolution happens once, at launch, before a session exists — so an id
//! nothing can serve is a startup error naming what *was* available, rather
//! than a provider error at the first token.
//!
//! A bare family name such as `sonnet` resolves to the newest model of that
//! family in the set; [`ResolvedModel::note`] says which one it chose.
//!
//! [`LlmProvider`]: polaris::models::llm::LlmProvider

use std::fmt;

/// Default model when `POLARIS_CODE_MODEL` is unset.
pub const DEFAULT_MODEL: &str = "anthropic/claude-sonnet-5-5";

/// Environment variable naming the model a run defaults to.
pub const MODEL_VAR: &str = "POLARIS_CODE_MODEL";

/// Environment variable declaring the whole selectable set, as a
/// comma- or whitespace-separated list of `provider/model` ids.
///
/// Set it to reach a model this build does not know about; it *replaces*
/// [`BUILTIN_MODELS`] rather than extending it, so an operator can also narrow
/// a session to one model.
pub const MODELS_VAR: &str = "POLARIS_CODE_MODELS";

/// The Anthropic models a build knows about with no configuration at all.
///
/// Deliberately short. It is a convenience, not an inventory: `POLARIS_CODE_MODELS`
/// is the supported way to name anything else, and the resolver's error says so.
/// Newest first; every id is one the framework's built-in price table
/// recognises except `claude-haiku-5-5`, which that table does not list yet,
/// so `--max-cost-usd` on it needs the live price table.
pub const BUILTIN_MODELS: &[&str] = &[
    "anthropic/claude-opus-5-5",
    "anthropic/claude-sonnet-5-5",
    "anthropic/claude-haiku-5-5",
    "anthropic/claude-fable-5-1",
    "anthropic/claude-opus-5",
    "anthropic/claude-sonnet-5",
    "anthropic/claude-sonnet-4-6",
    "anthropic/claude-haiku-4-5",
];

/// Default model when `--provider openai` is selected and neither `--model`
/// nor `POLARIS_CODE_MODEL` names one.
pub const DEFAULT_OPENAI_MODEL: &str = "openai/gpt-5.5";

/// The `OpenAI` models a build knows about with no configuration at all: the
/// ids the framework's built-in price table lists for the `OpenAI` provider.
///
/// A server that speaks the same API (Ollama, LM Studio, vLLM, a gateway)
/// serves ids this list cannot know, so `POLARIS_CODE_MODELS` declares them as
/// `openai/<id>`.
pub const BUILTIN_OPENAI_MODELS: &[&str] = &[
    "openai/gpt-5.5",
    "openai/gpt-5.4",
    "openai/gpt-5.4-mini",
    "openai/gpt-6-luna",
];

/// Default model when `--provider bedrock` is selected and neither `--model`
/// nor `POLARIS_CODE_MODEL` names one.
///
/// Bedrock has no small built-in set analogous to [`BUILTIN_MODELS`]: regional
/// inference-profile ids (`us.anthropic....`, `global.anthropic....`) vary by
/// account and region in a way a baked-in handful would misrepresent, so this
/// is the only default and `POLARIS_CODE_MODELS` is the supported way to
/// declare more.
pub const DEFAULT_BEDROCK_MODEL: &str = "aws.bedrock/global.anthropic.claude-sonnet-5";

/// Why a requested model could not be used.
///
/// Every variant renders the set that *was* selectable, because the operator's
/// next action is to pick from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelSelectionError {
    /// The requested id named a provider this run has not registered.
    UnknownProvider {
        /// What the operator asked for.
        requested: String,
        /// The provider prefix that is not registered.
        provider: String,
        /// The providers that are.
        providers: Vec<String>,
        /// The selectable ids.
        available: Vec<String>,
    },
    /// The requested name matched nothing in the selectable set.
    Unknown {
        /// What the operator asked for.
        requested: String,
        /// The selectable ids.
        available: Vec<String>,
    },
    /// A bare (unprefixed) name matched more than one selectable id.
    Ambiguous {
        /// What the operator asked for.
        requested: String,
        /// The ids it matched.
        matches: Vec<String>,
    },
    /// Nothing at all is selectable, so no name could have worked.
    Empty {
        /// What the operator asked for.
        requested: String,
    },
}

/// Render `ids` as a comma-separated list of backticked names.
fn listed(ids: &[String]) -> String {
    ids.iter()
        .map(|id| format!("`{id}`"))
        .collect::<Vec<_>>()
        .join(", ")
}

impl fmt::Display for ModelSelectionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownProvider {
                requested,
                provider,
                providers,
                available,
            } => write!(
                f,
                "model `{requested}` names provider `{provider}`, which is not configured \
                 (configured: {}); available models are {}",
                listed(providers),
                listed(available)
            ),
            Self::Unknown {
                requested,
                available,
            } => write!(
                f,
                "unknown model `{requested}`; available models are {} \
                 (set {MODELS_VAR} to declare others)",
                listed(available)
            ),
            Self::Ambiguous { requested, matches } => write!(
                f,
                "model `{requested}` is ambiguous: it matches {}; name one exactly",
                listed(matches)
            ),
            Self::Empty { requested } => write!(
                f,
                "model `{requested}` cannot be used: no provider in this run offers any \
                 declared model (set {MODELS_VAR}, and check the provider plugins)"
            ),
        }
    }
}

impl std::error::Error for ModelSelectionError {}

/// The `provider/model` ids one run may select between, and the one it
/// defaults to.
///
/// A value, not a registry: it is built once at launch, handed to the pieces
/// that need it ([`SessionSettings`](crate::terminal::SessionSettings) for the
/// `/model` chooser, the composition root for the launch flag), and never
/// mutated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelCatalog {
    default_model: String,
    models: Vec<String>,
}

impl ModelCatalog {
    /// Build a catalog from an explicit default and an explicit set.
    ///
    /// `default_model` is always selectable, even when `declared` omits it —
    /// a run must be able to use the model it starts on. Duplicates are
    /// dropped, declaration order is kept.
    #[must_use]
    pub fn new<I, S>(default_model: impl Into<String>, declared: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let default_model = default_model.into();
        let mut models = vec![default_model.clone()];
        for id in declared {
            let id = id.into();
            if !id.is_empty() && !models.contains(&id) {
                models.push(id);
            }
        }
        Self {
            default_model,
            models,
        }
    }

    /// A catalog offering exactly one model.
    ///
    /// The honest catalog for a session whose host wired no selection surface:
    /// the configured model is the only thing it can run on.
    #[must_use]
    pub fn pinned(model_id: impl Into<String>) -> Self {
        Self::new(model_id, Vec::<String>::new())
    }

    /// Build the catalog this process is configured with.
    ///
    /// Reads `POLARIS_CODE_MODEL` for the default and `POLARIS_CODE_MODELS`
    /// for the set, falling back to [`DEFAULT_MODEL`] and [`BUILTIN_MODELS`].
    #[must_use]
    pub fn from_env() -> Self {
        Self::from_env_with_default(DEFAULT_MODEL, BUILTIN_MODELS)
    }

    /// [`Self::from_env`], generalized to a caller-supplied default and
    /// built-in set — the seam `--provider bedrock` uses to fall back to
    /// [`DEFAULT_BEDROCK_MODEL`] (and no built-ins) instead of the Anthropic
    /// ones, while still honoring `POLARIS_CODE_MODEL` / `POLARIS_CODE_MODELS`
    /// exactly like the Anthropic path.
    #[must_use]
    pub fn from_env_with_default(default_model: &str, builtin: &[&str]) -> Self {
        Self::from_lookup(default_model, builtin, &|name| std::env::var(name).ok())
    }

    /// [`Self::from_env_with_default`] over an explicit source of variables
    /// instead of the process environment, so the same rules can be applied
    /// to any other source — and checked without touching the real one.
    #[must_use]
    pub fn from_lookup(
        default_model: &str,
        builtin: &[&str],
        lookup: &dyn Fn(&str) -> Option<String>,
    ) -> Self {
        let default_model = lookup(MODEL_VAR)
            .map(|id| id.trim().to_string())
            .filter(|id| !id.is_empty())
            .unwrap_or_else(|| default_model.to_string());
        match lookup(MODELS_VAR).filter(|v| !v.trim().is_empty()) {
            Some(declared) => Self::new(default_model, parse_declared(&declared)),
            None => Self::new(default_model, builtin.iter().map(|id| (*id).to_string())),
        }
    }

    /// The same catalog, less every model whose provider is not in `providers`.
    ///
    /// This is what makes the set "reachable" rather than merely "declared".
    /// The default model is filtered like any other: a run whose default is
    /// unreachable has a configuration error, and [`Self::select`] reports it
    /// rather than the catalog hiding it.
    #[must_use]
    pub fn restricted_to_providers<S: AsRef<str>>(&self, providers: &[S]) -> Self {
        let reachable = |id: &String| {
            id.split_once('/')
                .is_some_and(|(provider, _)| providers.iter().any(|p| p.as_ref() == provider))
        };
        Self {
            default_model: self.default_model.clone(),
            models: self
                .models
                .iter()
                .filter(|id| reachable(id))
                .cloned()
                .collect(),
        }
    }

    /// Every selectable `provider/model` id, in declaration order.
    #[must_use]
    pub fn models(&self) -> &[String] {
        &self.models
    }

    /// The distinct provider prefixes of [`Self::models`], in first-seen order.
    #[must_use]
    pub fn providers(&self) -> Vec<String> {
        let mut providers: Vec<String> = Vec::new();
        for id in &self.models {
            if let Some((provider, _)) = id.split_once('/')
                && !providers.iter().any(|known| known == provider)
            {
                providers.push(provider.to_string());
            }
        }
        providers
    }

    /// The id a run uses when it names none.
    #[must_use]
    pub fn default_model(&self) -> &str {
        &self.default_model
    }

    /// Whether `model_id` is selectable exactly as written.
    #[must_use]
    pub fn contains(&self, model_id: &str) -> bool {
        self.models.iter().any(|id| id == model_id)
    }

    /// Resolve what an operator typed to a selectable id.
    ///
    /// A name containing `/` must match an id exactly. A bare name matches on
    /// the model half: exactly first, then by substring, which is what makes
    /// `--model opus` and `/model haiku` work without spelling a full id.
    /// When a bare name matches several versions of one model family, the
    /// newest is taken; [`Self::resolve_noting`] also says so.
    ///
    /// # Errors
    ///
    /// [`ModelSelectionError`] when the name matches nothing, matches several
    /// unrelated ids, or names an unregistered provider — always carrying the
    /// set that *was* available.
    pub fn resolve(&self, requested: &str) -> Result<String, ModelSelectionError> {
        self.resolve_noting(requested).map(|resolved| resolved.id)
    }

    /// [`Self::resolve`], keeping what the resolver had to decide.
    ///
    /// # Errors
    ///
    /// [`ModelSelectionError`], as [`Self::resolve`].
    pub fn resolve_noting(&self, requested: &str) -> Result<ResolvedModel, ModelSelectionError> {
        let requested = requested.trim();
        if self.models.is_empty() {
            return Err(ModelSelectionError::Empty {
                requested: requested.to_string(),
            });
        }
        let chosen = |id: &str| ResolvedModel {
            id: id.to_string(),
            requested: requested.to_string(),
            matched: Vec::new(),
        };
        if let Some(exact) = self.models.iter().find(|id| *id == requested) {
            return Ok(chosen(exact));
        }
        if let Some((provider, _)) = requested.split_once('/') {
            let providers = self.providers();
            if !providers.iter().any(|known| known == provider) {
                return Err(ModelSelectionError::UnknownProvider {
                    requested: requested.to_string(),
                    provider: provider.to_string(),
                    providers,
                    available: self.models.clone(),
                });
            }
            return Err(ModelSelectionError::Unknown {
                requested: requested.to_string(),
                available: self.models.clone(),
            });
        }
        if requested.is_empty() {
            return Err(ModelSelectionError::Unknown {
                requested: String::new(),
                available: self.models.clone(),
            });
        }
        let model_half = |id: &str| {
            id.split_once('/')
                .map_or(id, |(_, model)| model)
                .to_string()
        };
        let exact: Vec<String> = self
            .models
            .iter()
            .filter(|id| model_half(id) == requested)
            .cloned()
            .collect();
        let matches = if exact.is_empty() {
            self.models
                .iter()
                .filter(|id| model_half(id).contains(requested))
                .cloned()
                .collect::<Vec<_>>()
        } else {
            exact
        };
        match matches.len() {
            0 => Err(ModelSelectionError::Unknown {
                requested: requested.to_string(),
                available: self.models.clone(),
            }),
            1 => Ok(chosen(&matches[0])),
            _ => match newest_of_one_family(&matches) {
                Some(newest_first) => Ok(ResolvedModel {
                    id: newest_first[0].clone(),
                    requested: requested.to_string(),
                    matched: newest_first,
                }),
                None => Err(ModelSelectionError::Ambiguous {
                    requested: requested.to_string(),
                    matches,
                }),
            },
        }
    }

    /// Resolve an optionally-named model, falling back to the default.
    ///
    /// The launch decision in one call: an explicitly named model wins over
    /// the default, and the default itself is resolved through the same set,
    /// so a `POLARIS_CODE_MODEL` nothing can serve fails the same way a bad
    /// `--model` does instead of surviving to the first token.
    ///
    /// # Errors
    ///
    /// [`ModelSelectionError`], as [`Self::resolve`].
    pub fn select(&self, requested: Option<&str>) -> Result<String, ModelSelectionError> {
        self.select_noting(requested).map(|resolved| resolved.id)
    }

    /// [`Self::select`], keeping what the resolver had to decide.
    ///
    /// # Errors
    ///
    /// [`ModelSelectionError`], as [`Self::resolve`].
    pub fn select_noting(
        &self,
        requested: Option<&str>,
    ) -> Result<ResolvedModel, ModelSelectionError> {
        self.resolve_noting(requested.unwrap_or(&self.default_model))
    }
}

/// A name resolved to a selectable id, and whether the resolver had to choose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedModel {
    /// The selectable `provider/model` id.
    pub id: String,
    /// What the operator typed.
    pub requested: String,
    /// Every id the name matched, newest first, when it matched several
    /// versions of one family and the newest was taken; empty when the name
    /// identified one id outright.
    pub matched: Vec<String>,
}

impl ResolvedModel {
    /// One sentence saying which model a family name chose, or `None` when
    /// the name needed no choosing.
    #[must_use]
    pub fn note(&self) -> Option<String> {
        (!self.matched.is_empty()).then(|| {
            format!(
                "`{}` matches {} models; using the newest, `{}`",
                self.requested,
                self.matched.len(),
                self.id
            )
        })
    }
}

/// `ids` ordered newest first, when they are versions of one model family.
///
/// `None` when they are not — a different provider, family or variant among
/// them (`gpt-5.4` and `gpt-5.4-mini`), an id with no version in it, or two
/// at the same newest version — because then nothing says which one was meant.
fn newest_of_one_family(ids: &[String]) -> Option<Vec<String>> {
    let mut releases = ids
        .iter()
        .map(|id| release(id).map(|release| (release, id)))
        .collect::<Option<Vec<_>>>()?;
    let ((provider, family, _), _) = releases.first()?;
    if releases
        .iter()
        .any(|((other_provider, other_family, _), _)| {
            other_provider != provider || other_family != family
        })
    {
        return None;
    }
    releases.sort_by(|((_, _, a), _), ((_, _, b), _)| b.cmp(a));
    if releases.len() > 1 && releases[0].0.2 == releases[1].0.2 {
        return None;
    }
    Some(releases.into_iter().map(|(_, id)| id.clone()).collect())
}

/// Where an id sits in its model family: its provider, the family's name, and
/// its version.
///
/// `anthropic/claude-sonnet-5-5` is provider `anthropic`, family
/// `claude-sonnet`, version `[5, 5]`. A snapshot date (`-20250929`) belongs to
/// neither the family nor the version, so a dated id sorts with its family; any
/// other word (`-mini`) is part of the family, since `gpt-5.4-mini` is a
/// different model rather than a newer `gpt-5.4`. `None` when the id carries
/// no version at all.
fn release(id: &str) -> Option<(&str, String, Vec<u64>)> {
    let (provider, model) = id.split_once('/')?;
    let mut family = Vec::new();
    let mut version = Vec::new();
    for segment in model.split('-') {
        match version_parts(segment) {
            Some(parts) => version.extend(parts),
            None if is_snapshot_date(segment) => {}
            None => family.push(segment),
        }
    }
    (!version.is_empty()).then(|| (provider, family.join("-"), version))
}

/// The numbers in a version segment such as `5` or `5.4`; `None` for anything
/// else. Each number has at most four digits, which is what keeps a snapshot
/// date out of the version.
fn version_parts(segment: &str) -> Option<Vec<u64>> {
    segment
        .split('.')
        .map(|part| {
            let digits = !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit());
            (digits && part.len() <= 4).then(|| part.parse().ok())?
        })
        .collect()
}

/// A snapshot date such as `20250929`: only digits, and too many to be a version.
fn is_snapshot_date(segment: &str) -> bool {
    segment.len() >= 6 && segment.bytes().all(|b| b.is_ascii_digit())
}

/// Split a `POLARIS_CODE_MODELS` value on commas and whitespace.
fn parse_declared(value: &str) -> Vec<String> {
    value
        .split([',', ' ', '\t', '\n'])
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(ToString::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog() -> ModelCatalog {
        ModelCatalog::new("fake/alpha", ["fake/alpha", "fake/beta", "other/gamma"])
    }

    #[test]
    fn the_default_is_always_selectable_and_listed_first() {
        let catalog = ModelCatalog::new("fake/alpha", ["fake/beta"]);
        assert_eq!(catalog.models(), ["fake/alpha", "fake/beta"]);
        assert_eq!(catalog.default_model(), "fake/alpha");
    }

    #[test]
    fn an_explicit_name_wins_over_the_default() {
        assert_eq!(catalog().select(Some("fake/beta")).unwrap(), "fake/beta");
        assert_eq!(catalog().select(None).unwrap(), "fake/alpha");
    }

    #[test]
    fn a_bare_name_resolves_when_it_is_unambiguous() {
        assert_eq!(catalog().resolve("beta").unwrap(), "fake/beta");
        assert_eq!(catalog().resolve("gamma").unwrap(), "other/gamma");
    }

    #[test]
    fn an_ambiguous_bare_name_names_its_candidates() {
        let catalog = ModelCatalog::new("fake/sonnet-a", ["fake/sonnet-a", "fake/sonnet-b"]);
        let err = catalog.resolve("sonnet").unwrap_err();
        assert!(
            matches!(&err, ModelSelectionError::Ambiguous { matches, .. } if matches.len() == 2),
            "{err}"
        );
        assert!(err.to_string().contains("`fake/sonnet-b`"), "{err}");
    }

    #[test]
    fn an_unreachable_provider_is_dropped_from_the_set() {
        let restricted = catalog().restricted_to_providers(&["fake"]);
        assert_eq!(restricted.models(), ["fake/alpha", "fake/beta"]);
        let err = restricted.resolve("other/gamma").unwrap_err();
        assert!(
            matches!(err, ModelSelectionError::UnknownProvider { .. }),
            "{err}"
        );
    }

    #[test]
    fn an_unreachable_default_fails_at_selection_rather_than_silently() {
        let catalog =
            ModelCatalog::new("absent/alpha", ["fake/beta"]).restricted_to_providers(&["fake"]);
        let err = catalog.select(None).unwrap_err();
        assert!(
            matches!(err, ModelSelectionError::UnknownProvider { .. }),
            "{err}"
        );
    }

    #[test]
    fn declarations_split_on_commas_and_whitespace() {
        assert_eq!(
            parse_declared(" a/one, a/two\na/three "),
            ["a/one", "a/two", "a/three"]
        );
    }

    #[test]
    fn a_pinned_catalog_offers_exactly_its_model() {
        let catalog = ModelCatalog::pinned("fake/only");
        assert_eq!(catalog.models(), ["fake/only"]);
        assert!(catalog.resolve("fake/other").is_err());
    }

    fn anthropic() -> ModelCatalog {
        ModelCatalog::new(DEFAULT_MODEL, BUILTIN_MODELS.iter().copied())
    }

    fn openai() -> ModelCatalog {
        ModelCatalog::new(DEFAULT_OPENAI_MODEL, BUILTIN_OPENAI_MODELS.iter().copied())
    }

    #[test]
    fn a_family_name_resolves_to_the_newest_model_of_the_family() {
        let catalog = anthropic();
        for (name, expected) in [
            ("sonnet", "anthropic/claude-sonnet-5-5"),
            ("opus", "anthropic/claude-opus-5-5"),
            ("haiku", "anthropic/claude-haiku-5-5"),
            ("fable", "anthropic/claude-fable-5-1"),
        ] {
            assert_eq!(catalog.resolve(name).unwrap(), expected, "{name}");
        }
    }

    #[test]
    fn a_family_name_says_which_model_it_chose() {
        let resolved = anthropic().resolve_noting("sonnet").unwrap();
        assert_eq!(resolved.id, "anthropic/claude-sonnet-5-5");
        assert_eq!(
            resolved.matched,
            [
                "anthropic/claude-sonnet-5-5",
                "anthropic/claude-sonnet-5",
                "anthropic/claude-sonnet-4-6"
            ]
        );
        let note = resolved.note().expect("a choice is worth saying");
        assert!(note.contains("`sonnet`"), "{note}");
        assert!(note.contains("3 models"), "{note}");
        assert!(note.contains("`anthropic/claude-sonnet-5-5`"), "{note}");
    }

    #[test]
    fn a_name_that_needed_no_choosing_has_no_note() {
        let catalog = anthropic();
        for name in ["fable", "claude-sonnet-5", "anthropic/claude-sonnet-4-6"] {
            let resolved = catalog.resolve_noting(name).unwrap();
            assert_eq!(resolved.note(), None, "{name}");
        }
    }

    #[test]
    fn an_exact_model_name_beats_the_newer_models_that_contain_it() {
        assert_eq!(
            anthropic().resolve("claude-sonnet-5").unwrap(),
            "anthropic/claude-sonnet-5"
        );
        assert_eq!(
            anthropic().resolve("claude-haiku-4-5").unwrap(),
            "anthropic/claude-haiku-4-5"
        );
    }

    #[test]
    fn names_that_span_several_families_stay_ambiguous() {
        for name in ["claude", "5"] {
            let err = anthropic().resolve(name).unwrap_err();
            assert!(
                matches!(err, ModelSelectionError::Ambiguous { .. }),
                "{name}: {err}"
            );
        }
    }

    /// `gpt-5.4-mini` is a different model from `gpt-5.4`, not a newer one, so
    /// a name that matches both must not be settled by version alone.
    #[test]
    fn a_variant_is_not_a_newer_version_of_its_base_model() {
        let err = openai().resolve("gpt").unwrap_err();
        assert!(
            matches!(err, ModelSelectionError::Ambiguous { .. }),
            "{err}"
        );
        assert_eq!(openai().resolve("mini").unwrap(), "openai/gpt-5.4-mini");
        assert_eq!(openai().resolve("gpt-5.4").unwrap(), "openai/gpt-5.4");
    }

    #[test]
    fn a_dated_snapshot_sorts_with_its_family() {
        let catalog = ModelCatalog::new(
            "p/claude-sonnet-4-5-20250929",
            ["p/claude-sonnet-4-5-20250929", "p/claude-sonnet-5-5"],
        );
        assert_eq!(catalog.resolve("sonnet").unwrap(), "p/claude-sonnet-5-5");
    }

    #[test]
    fn a_version_written_before_the_family_word_still_orders() {
        let catalog = ModelCatalog::new(
            "p/claude-3-5-haiku",
            ["p/claude-3-5-haiku", "p/claude-haiku-4-5"],
        );
        assert_eq!(catalog.resolve("haiku").unwrap(), "p/claude-haiku-4-5");
    }

    #[test]
    fn two_newest_candidates_at_one_version_stay_ambiguous() {
        let catalog = ModelCatalog::new(
            "p/claude-sonnet-5-5",
            ["p/claude-sonnet-5-5", "p/claude-sonnet-5-5-20260101"],
        );
        let err = catalog.resolve("sonnet").unwrap_err();
        assert!(
            matches!(err, ModelSelectionError::Ambiguous { .. }),
            "{err}"
        );
    }

    #[test]
    fn the_same_family_at_two_providers_stays_ambiguous() {
        let catalog = ModelCatalog::new(
            "a/claude-sonnet-5-5",
            ["a/claude-sonnet-5-5", "b/claude-sonnet-5-5"],
        );
        let err = catalog.resolve("sonnet").unwrap_err();
        assert!(
            matches!(err, ModelSelectionError::Ambiguous { .. }),
            "{err}"
        );
    }

    #[test]
    fn the_defaults_are_members_of_their_own_sets() {
        assert!(BUILTIN_MODELS.contains(&DEFAULT_MODEL));
        assert!(BUILTIN_OPENAI_MODELS.contains(&DEFAULT_OPENAI_MODEL));
        for id in BUILTIN_MODELS {
            assert!(id.starts_with("anthropic/"), "{id}");
        }
        for id in BUILTIN_OPENAI_MODELS {
            assert!(id.starts_with("openai/"), "{id}");
        }
    }

    #[test]
    fn a_lookup_replaces_the_process_environment() {
        let env = |name: &str| match name {
            MODEL_VAR => Some(" openai/gpt-5.4 ".to_string()),
            MODELS_VAR => Some("openai/gpt-5.4, openai/local-model".to_string()),
            _ => None,
        };
        let catalog = ModelCatalog::from_lookup(DEFAULT_OPENAI_MODEL, BUILTIN_OPENAI_MODELS, &env);
        assert_eq!(catalog.default_model(), "openai/gpt-5.4");
        assert_eq!(catalog.models(), ["openai/gpt-5.4", "openai/local-model"]);

        let none = |_: &str| -> Option<String> { None };
        let catalog = ModelCatalog::from_lookup(DEFAULT_OPENAI_MODEL, BUILTIN_OPENAI_MODELS, &none);
        assert_eq!(catalog.default_model(), DEFAULT_OPENAI_MODEL);
        assert_eq!(catalog.models(), BUILTIN_OPENAI_MODELS);
    }
}
