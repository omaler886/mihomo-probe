//! The probe engine: what gets tested, and how much of it at once.
//!
//! This crate is the Rust counterpart of `mihomo_test/engine.py`'s round
//! pipeline (workstreams/06). It is being built up in batches; so far it holds
//! the concurrency layer only.
//!
//! `probe-engine` sits above `probe-mihomo`/`probe-storage` and below
//! `probe-api`/`probe-scheduler` in the workspace dependency order
//! (workstreams/02). It does no IO of its own -- it decides *when* IO may
//! happen.

pub mod limits;

pub use limits::{Gate, Job, LimitError, Limits, Permit, RoundCtx};
