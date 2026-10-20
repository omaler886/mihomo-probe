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
//! [`wire::DnsAnswer`] exposes CNAME targets and conservative positive/negative
//! TTLs for caller-owned caches. This crate does not persist cache entries,
//! expand probe candidates, or choose the engine's any/all aggregation policy.
//! The legacy IP-only wrappers delegate to the same validated response path.

pub mod geo;
pub mod resolver;
pub mod wire;

pub use geo::{GeoClient, GeoRow, BATCH_RETRY_PAUSE_S, BATCH_TIMEOUT_S, BATCH_URL};
pub use resolver::{DohResolver, ViewConfig, DEFAULT_ECS_PREFIX, DEFAULT_TIMEOUT_S};
pub use wire::{build_query, parse_ips, parse_response, DnsAnswer, WireError, TYPE_A, TYPE_AAAA};
