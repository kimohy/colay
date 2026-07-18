//! Vendor-neutral domain contracts for the local coding-agent orchestrator.
//!
//! This crate intentionally contains no provider SDK or persistence details. Persisted
//! structures carry their own schema version and paths that address repository content
//! use [`RepoPath`] instead of unrestricted operating-system paths.

pub mod assessment;
pub mod checkpoint;
pub mod event;
pub mod evidence;
pub mod handover;
pub mod ids;
pub mod integrity;
pub mod path;
pub mod provider;
pub mod routing;
pub mod schema;
pub mod state;
pub mod task;
pub mod usage;
pub mod verification;

pub use assessment::*;
pub use checkpoint::*;
pub use event::*;
pub use evidence::*;
pub use handover::*;
pub use ids::*;
pub use integrity::*;
pub use path::*;
pub use provider::*;
pub use routing::*;
pub use schema::*;
pub use state::*;
pub use task::*;
pub use usage::*;
pub use verification::*;
