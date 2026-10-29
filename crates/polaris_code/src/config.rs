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
//! | The set | [`BUILTIN_MODELS`], replaced wholesale by `POLARIS_CODE_MODELS` | every `provider/model` id this run may select |
//! | The default | `POLARIS_CODE_MODEL`, else [`DEFAULT_MODEL`] | the id a run uses when it names none |
//! | Reachability | the registered provider names | an id whose provider is not registered is dropped |
//!
//! Resolution happens once, at launch, before a session exists — so an id
//! nothing can serve is a startup error naming what *was* available, rather
//! than a provider error at the first token.
//!
//! [`LlmProvider`]: polaris::models::llm::LlmProvider

use std::fmt;

/// Default model when `POLARIS_CODE_MODEL` is unset.
pub const DEFAULT_MODEL: &str = "anthropic/claude-sonnet-5";

/// Environment variable naming the model a run defaults to.
pub const MODEL_VAR: &str = "POLARIS_CODE_MODEL";

/// Environment variable declaring the whole selectable set, as a
/// comma- or whitespace-separated list of `provider/model` ids.
///
/// Set it to reach a model this build does not know about; it *replaces*
/// [`BUILTIN_MODELS`] rather than extending it, so an operator can also narrow
/// a session to one model.
pub const MODELS_VAR: &str = "POLARIS_CODE_MODELS";

/// The models a build knows about with no configuration at all.
///
/// Deliberately short. It is a convenience, not an inventory: `POLARIS_CODE_MODELS`
/// is the supported way to name anything else, and the resolver's error says so.
pub const BUILTIN_MODELS: &[&str] = &[
    "anthropic/claude-opus-5",
    "anthropic/claude-sonnet-5",
    "anthropic/claude-sonnet-4-6",
    "anthropic/claude-haiku-4-5",
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
        let default_model = std::env::var(MODEL_VAR)
            .ok()
            .map(|id| id.trim().to_string())
            .filter(|id| !id.is_empty())
            .unwrap_or_else(|| default_model.to_string());
        match std::env::var(MODELS_VAR)
            .ok()
            .filter(|v| !v.trim().is_empty())
        {
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
    ///
    /// # Errors
    ///
    /// [`ModelSelectionError`] when the name matches nothing, matches several
    /// ids, or names an unregistered provider — always carrying the set that
    /// *was* available.
    pub fn resolve(&self, requested: &str) -> Result<String, ModelSelectionError> {
        let requested = requested.trim();
        if self.models.is_empty() {
            return Err(ModelSelectionError::Empty {
                requested: requested.to_string(),
            });
        }
        if let Some(exact) = self.models.iter().find(|id| *id == requested) {
            return Ok(exact.clone());
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
            1 => Ok(matches[0].clone()),
            _ => Err(ModelSelectionError::Ambiguous {
                requested: requested.to_string(),
                matches,
            }),
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
        self.resolve(requested.unwrap_or(&self.default_model))
    }
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
}
