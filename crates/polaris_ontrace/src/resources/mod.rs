//! Shared resources accessed by agents and plugins.
//!
//! Grouped by storage scope:
//!
//! - [`kg::KnowledgeGraph`] and [`aliases::Aliases`] are
//!   [`GlobalResource`](polaris::system::resource::GlobalResource)s — one
//!   instance per server, shared across every session.
//! - [`trace_line::TraceLine`] is a
//!   [`LocalResource`](polaris::system::resource::LocalResource) — inserted
//!   per-turn into the session context by [`crate::ExtractionPlugin`].
//!
//! Add a new resource by dropping a file in this module and exposing it via
//! the appropriate scope trait. Agents and plugins consume resources by
//! type, not by path, so wiring is automatic once registered on the server.

pub mod aliases;
pub mod kg;
pub mod ontology_gap;
pub mod sightings;
pub mod spend;
pub mod trace_line;
