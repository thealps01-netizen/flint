#![forbid(unsafe_code)]
//! Flint search engine core: folding, query parsing, the in-memory index,
//! matching and ranking. No OS calls and no I/O.

pub mod fold;
pub mod fuzzy;
pub mod index;
pub mod owner;
pub mod query;
pub mod rank;
pub mod search;
pub mod typo;

pub use index::{
    EntryId, EntryInput, ImageError, Index, OWNER_NOBODY, OWNER_NONE, Owner, Restriction,
    visible_to,
};
pub use owner::{ANY_DRIVE, OwnerRule};
pub use query::{Query, Term, parse};
pub use rank::Boosts;
pub use search::{Hit, SearchOptions, SearchResult, search};
