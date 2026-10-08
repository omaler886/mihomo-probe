//! DNS-over-HTTPS with EDNS Client Subnet (workstreams/05, R4).
//!
//! Geo-DNS hands different addresses to CN and overseas resolvers, and
//! round-robin may return several per query. Probing only whatever the kernel
//! happens to resolve would sample one address per domain: a node behind five
//! addresses where one is dead flips between alive and dead for no reason,
//! and the addresses CN clients actually get are never tested at all.
//!
//! Layering: [`wire`] is pure packet code; [`resolver::DohResolver`] moves
//! the packets over HTTP; [`geo::GeoClient`] batches the entry-IP country
//! lookups (the other once-per-round external query the entry filter needs).
//! This crate is transport only: it holds no cache and no ledger handle.
//!
//! ## What is deliberately NOT here yet
//!
//! * **the caches and the expansion.** `domain_views` (6h TTL) and `ip_geo`
//!   belong to the caller, and `engine.classify_and_expand` is what turns a
//!   domain into one test per resolved address. Neither is wired up: nothing
//!   depends on `probe-dns` today (`probe-engine` does not list it), so a
//!   round still tests one address per server. That wiring is the rest of R4;
//!   `probe-source`'s crate doc names the same gap (`classify_and_expand`).
//! * **positive/negative TTL and CNAME chains.** The Python parser discards
//!   both; recording them needs the cache that lives with the caller.

pub mod geo;
pub mod resolver;
pub mod wire;

pub use geo::{GeoClient, GeoRow, BATCH_RETRY_PAUSE_S, BATCH_TIMEOUT_S, BATCH_URL};
pub use resolver::{DohResolver, ViewConfig, DEFAULT_ECS_PREFIX, DEFAULT_TIMEOUT_S};
pub use wire::{build_query, parse_ips, WireError, TYPE_A, TYPE_AAAA};
