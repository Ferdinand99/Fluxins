//! Payloads of gateway events added with threads, forums, saved messages,
//! recent mentions, relationships and account-level social events. The older
//! event payloads live in [`crate::models`].
//!
//! Every type is lenient like the rest of the crate: `null` counts as a
//! missing field, unknown fields are ignored (or kept in `extra`), and list
//! elements that do not parse are skipped.

pub mod relationships;
pub mod saved;
pub mod threads;
pub mod user;

pub use relationships::*;
pub use saved::*;
pub use threads::*;
pub use user::*;
