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
pub mod network;
pub mod preflight;
pub mod publish;
pub mod round;
pub mod round_lock;
pub mod scheduler;
pub mod verify;

pub use collect::{backend_from_env, collect, Collected};

pub use limits::{Gate, Job, LimitError, Limits, Permit, RoundCtx, RoundDeadline};
pub use measure::{Dialer, TestOne, TestPolicy, TERMINAL_REASONS};
pub use preflight::{make_testable, KernelValidator, MihomoValidator, PreflightVerdict, MAX_PRUNE};
pub use publish::{
    publish_export, publish_round, push_round_exports, rollback_export, select_source_exports,
    ExportDrop, ExportDropReason, PublishDecision, PublishError, PublishOutcome, PublishPolicy,
    PublishRequest, PublishRoundOutcome, PublishRoundRequest, PushRecord, SkipReason, SourceExport,
    SourcePublication,
};
pub use round::{
    run_round, run_round_with_ctx, ControllerPrep, KernelPrep, KernelState, Ledger, NodeTester,
    NodeVerdict, NotPrepared, RoundCounts, RoundOutcome, RoundPlan, RoundSettings, Verdict,
};
pub use round_lock::RoundLock;
pub use scheduler::{
    decide, parse_stored_instant, reap_orphan_rounds, OrphanLedger, SchedulerDecision, ORPHAN_NOTE,
    STARTUP_GRACE,
};
