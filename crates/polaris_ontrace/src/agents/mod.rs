//! Agent patterns — each module here defines one `polaris::agent::Agent`
//! implementation.
//!
//! An agent is a graph of `#[system]`-annotated functions plus any
//! agent-local resources it needs (typically an `AgentConfig` and an
//! `AgentLlm` handle). Agents are stateless across sessions — durable state
//! lives in `GlobalResource`s exposed by [`crate::resources`].
//!
//! Adding a new pattern is one new file:
//!
//! 1. Define the systems and an `Agent` impl here.
//! 2. Register it on `SessionsAPI` from a matching plugin in
//!    [`crate::plugins`].

pub mod extraction;
pub mod seed;

#[cfg(feature = "oxigraph")]
pub mod inspect;
#[cfg(feature = "oxigraph")]
pub mod oxigraph_extraction;
#[cfg(feature = "oxigraph")]
pub(crate) mod repair;
#[cfg(feature = "oxigraph")]
pub(crate) mod shacl;
