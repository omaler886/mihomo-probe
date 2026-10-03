//! Where nodes come from: subscriptions in, `Job`s out.
//!
//! This crate is the Rust counterpart of three Python modules that only make
//! sense together:
//!
//! * `core.fingerprint_proxy` / `core.variant_fingerprint` -- node identity
//! * `core.prepare` -- the kernel-facing proxy plus the mapping back
//! * `store.py` + `engine.collect_entries` -- fetching and flattening sources
//!
//! They live in one crate because the identity rule is what makes the rest
//! meaningful: `prepare` may rename a node, strip fields and coerce its port,
//! and the ledger identity has to be computed from the *original* proxy or the
//! same node changes identity between rounds (`engine._orig_fp` documents the
//! round where that happened).
//!
//! ## What is deliberately NOT here yet
//!
//! * **DNS / per-address variants** (`classify_and_expand`) -- that is R4.
//!   Today one node yields one `Job`, keyed on the domain form.
//! * **Chain expansion** (`expand_chains`) -- R6.
//! * **Share-link dialects** (`vless://`, base64 bodies). Python routes those
//!   through Sub-Store rather than parsing them itself, and so does this crate:
//!   ask Sub-Store for `target=ClashMeta` and it hands back Clash proxies.

pub mod fetch;
pub mod identity;
pub mod prepare;
pub mod source;
pub mod subscription;

pub use fetch::{Fetcher, SubStoreClient};
pub use identity::{fingerprint_proxy, variant_fingerprint, DROP_FIELDS, REQUIRED};
pub use prepare::{prepare, DroppedNode, PreparedNode, RawEntry};
pub use source::{collect_entries, FetchedSource, CAT_CHAIN, CAT_DIRECT, CAT_RELAY};
pub use subscription::parse_proxies;

// The source record itself belongs to the config layer (`config.json` owns the
// list); re-exported so a caller does not have to reach into two crates.
pub use probe_config::SourceSpec;
