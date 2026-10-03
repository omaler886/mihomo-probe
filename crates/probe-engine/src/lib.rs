//! The probe engine: what gets tested, and how much of it at once.
//!
//! This crate is the Rust counterpart of `mihomo_test/engine.py`'s round
//! pipeline (workstreams/06). It is being built up in batches; so far it holds
//! the concurrency gate and the round runner that drives it.
//!
//! `probe-engine` sits above `probe-mihomo`/`probe-storage` and below
//! `probe-api`/`probe-scheduler` in the workspace dependency order
//! (workstreams/02). It does no IO of its own -- it decides *when* IO may
//! happen and *what* a round records.

pub mod collect;
pub mod limits;
pub mod measure;
pub mod round;

pub use collect::{backend_from_env, collect, Collected};

pub use limits::{Gate, Job, LimitError, Limits, Permit, RoundCtx};
pub use measure::{Dialer, TestOne, TestPolicy, TERMINAL_REASONS};
pub use round::{
    run_round, run_round_with_ctx, ControllerPrep, KernelPrep, KernelState, Ledger, NodeTester,
    NodeVerdict, NotPrepared, RoundCounts, RoundOutcome, RoundPlan, RoundSettings, Verdict,
};
