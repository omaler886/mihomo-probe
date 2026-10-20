//! The in-process round timer, and the reaper for rounds a dead process left open.
//!
//! Ports `server.py:scheduler_loop` and `engine.py:reap_orphan_rounds`. The
//! decision is a pure function over instants the caller supplies: nothing here
//! reads the clock, the ledger, or the process state, so a whole schedule can be
//! replayed in a test with fixed timestamps.
//!
//! The timer is what lets a deployment produce rounds without a systemd unit: it
//! starts one when the previous round is due, reports (and never starts) while a
//! round is in flight, and closes rows left open by a process that died
//! mid-round -- `docker compose up -d --build`, the documented way to deploy, is
//! exactly that.

use std::time::Duration as StdDuration;

use probe_config::ScheduleSection;
use probe_domain::{DomainResult, RoundId};
use time::OffsetDateTime;

/// Python `server.py:scheduler_loop`'s startup grace: with an empty ledger the
/// first round is due five seconds after the process started, and that deadline
/// has to stay put across every poll until it fires.
pub const STARTUP_GRACE: time::Duration = time::Duration::seconds(5);

/// The note Python stamps on a round its process abandoned. Byte for byte the
/// same string, so a reaped row is indistinguishable in the two ledgers.
pub const ORPHAN_NOTE: &str = "orphaned: 进程在轮次中途退出（重建/被杀），本轮无结果";

/// What the timer decided for one poll.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchedulerDecision {
    /// A round is due and nothing is running: start one.
    Start,
    /// `schedule.enabled = false`. Manual and API rounds still work.
    Disabled,
    /// Not due yet.
    NotDue,
    /// A round is already in flight: never start a second one.
    Busy,
}

/// The ledger seam orphan reaping needs.
///
/// Kept separate from `round::Ledger` because reaping runs beside the runner,
/// not inside it: the scheduler only lists open rows and closes one.
pub trait OrphanLedger {
    /// Rounds that were started and never closed, oldest first, with the stored
    /// `started_at` text -- Python `db.open_rounds()`.
    fn open_rounds(&self) -> DomainResult<Vec<(RoundId, String)>>;

    /// Close a round that can never finish, replacing its note.
    fn close_orphan_round(&self, round_id: RoundId, note: &str) -> DomainResult<()>;
}

/// The ledger the service actually runs against.
///
/// The impl lives here, not in `probe-storage`: the trait belongs to the engine,
/// so storage would have to depend on the engine to name it -- and that edge
/// would close a dependency cycle.
impl OrphanLedger for probe_storage::Storage {
    /// `open_round_ids()` lists the ids (oldest first); the stored `started_at`
    /// each one carries comes from `round_by_id()`. That is two statements per
    /// open row instead of one join, and open rows are normally zero or one, so
    /// the round trips cost nothing measurable on a poll that runs every 20s.
    fn open_rounds(&self) -> DomainResult<Vec<(RoundId, String)>> {
        let mut rounds = Vec::new();
        for round_id in self.open_round_ids()? {
            if let Some(row) = self.round_by_id(round_id)? {
                rounds.push((row.round_id, row.started_at));
            }
        }
        Ok(rounds)
    }

    /// Close a reaped row with zero counts and a zero duration, the way an
    /// abandoned round ends -- and the ADR-0005 `inconclusive` flag set.
    ///
    /// `finish_round_timed` is the entry that writes `duration_s`, and an
    /// orphan measured nothing, so `0.0` is the honest value -- the same one
    /// Python's `db.finish_round(round_id, duration_s=0, note=...)` writes, so
    /// counts/note/duration stay byte-identical for a shadow comparison.
    /// The `inconclusive` flag is the one deliberate divergence (Python has no
    /// such column on this path): without it, `previous_alive_count` reads the
    /// reaped row as a trusted baseline of ok=0, and `round_is_suspect`
    /// returns false for `alive_prev <= 0` -- so the very next round, the one
    /// most likely to be measuring through whatever killed this process, runs
    /// with the alive-floor guard disarmed.
    fn close_orphan_round(&self, round_id: RoundId, note: &str) -> DomainResult<()> {
        self.finish_round_full_timed(
            round_id,
            Some(note),
            &probe_storage::RoundGuardSummary {
                total: 0,
                ok: 0,
                failed: 0,
                dropped: 0,
                restored: 0,
                suspect: false,
                inconclusive: true,
            },
            0.0,
        )
    }
}

/// Decide whether this poll starts a round, and when the next one is due.
///
/// Python `server.py:scheduler_loop`, step for step:
///
/// * a disabled schedule clears the panel's `next_run` and starts nothing;
/// * the deadline is the last round's `started_at` plus the interval, or -- with
///   an empty ledger -- the process start plus [STARTUP_GRACE];
/// * a round already in flight is never started, whatever the deadline says;
/// * once `now` reaches the deadline a round starts, and the panel shows the
///   next deadline as `now + interval` until the new row's own `started_at`
///   takes over on the following poll.
///
/// The returned instant IS the `next_run` a status endpoint or panel shows;
/// `None` means the timer is off. Python checks `now >= due and not BUSY` in
/// one expression; reporting [SchedulerDecision::Busy] before the deadline
/// check tells a panel *why* nothing is being started while keeping the same
/// `next_run`.
pub fn decide(
    schedule: &ScheduleSection,
    last_started: Option<OffsetDateTime>,
    process_started: OffsetDateTime,
    now: OffsetDateTime,
    busy: bool,
) -> (SchedulerDecision, Option<OffsetDateTime>) {
    if !schedule.enabled {
        return (SchedulerDecision::Disabled, None);
    }
    let interval = interval_of(schedule);
    // Python holds this deadline in a loop-local (`first_due`), so it survives
    // every poll; anchoring it to the process start produces the same stable
    // value from a pure function.
    let due = match last_started {
        Some(started) => started + interval,
        None => process_started + STARTUP_GRACE,
    };
    if busy {
        return (SchedulerDecision::Busy, Some(due));
    }
    if now >= due {
        return (SchedulerDecision::Start, Some(now + interval));
    }
    (SchedulerDecision::NotDue, Some(due))
}

/// Close the rounds a process that died mid-round left open.
///
/// Only rounds older than `budget` (the caller's `watchdog.round_timeout_minutes`)
/// are reaped: a round that is genuinely running takes minutes, so it is younger
/// than the budget, and the CLI may legitimately run one alongside the service --
/// the two share the ledger but not the in-process busy lock. `running` names the
/// round this process still has in flight: a round the deadline just cancelled
/// legitimately stays open past the budget while it drains in-flight probes and
/// closes its own row, so the reaper must not close it out from under the runner.
/// An unusable stored timestamp is not a licence to close the row: Python reads
/// it as `0` and leaves that round alone, and so does this.
///
/// A failed close is skipped rather than fatal, like Python's reaper: one
/// unwritable row must not leave every other row open forever, and the next poll
/// retries it anyway.
pub fn reap_orphan_rounds(
    ledger: &dyn OrphanLedger,
    now: OffsetDateTime,
    budget: StdDuration,
    running: Option<RoundId>,
) -> DomainResult<Vec<RoundId>> {
    let budget = time_duration(budget);
    let mut closed = Vec::new();
    for (round_id, started_at) in ledger.open_rounds()? {
        if Some(round_id) == running {
            continue;
        }
        let Some(started) = parse_stored_instant(&started_at) else {
            continue;
        };
        if now - started <= budget {
            continue;
        }
        if ledger.close_orphan_round(round_id, ORPHAN_NOTE).is_ok() {
            closed.push(round_id);
        }
    }
    Ok(closed)
}

/// The clamped interval as a `time` duration. Read it through
/// [ScheduleSection::interval] so a hand-edited zero cannot become a busy loop.
fn interval_of(schedule: &ScheduleSection) -> time::Duration {
    time::Duration::seconds(schedule.interval().as_secs() as i64)
}

/// A `std` budget as a `time` duration. A budget past `i64::MAX` seconds is
/// clamped rather than panic: it only ever means "reap nothing".
fn time_duration(budget: StdDuration) -> time::Duration {
    let seconds = i64::try_from(budget.as_secs()).unwrap_or(i64::MAX);
    time::Duration::seconds(seconds) + time::Duration::nanoseconds(i64::from(budget.subsec_nanos()))
}

/// Parse a stored timestamp as UTC.
///
/// Public because the serve loop needs it too: `decide` wants an instant, the
/// ledger stores `RoundSummary::started_at` / `open_rounds()` as text, and
/// re-implementing this at the call site is how a local-time misread sneaks in.
///
/// Python `server.py:_parse` accepts the two formats the two implementations
/// have written over the years and reads anything else as "no timestamp". The
/// stored values are UTC (`db.now()` / `utc_now()`), so reading them as local
/// time would shift every deadline by the host's offset.
///
/// Hand-rolled rather than `OffsetDateTime::parse` because the workspace's
/// `time` dependency enables `formatting` but not `parsing`; the grammar is
/// Python's `strptime` pair -- fixed width, digits only, `T` or space.
pub fn parse_stored_instant(text: &str) -> Option<OffsetDateTime> {
    let stamp = text.trim();
    let bytes = stamp.as_bytes();
    if bytes.len() != 19
        || (bytes[10] != b'T' && bytes[10] != b' ')
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[13] != b':'
        || bytes[16] != b':'
    {
        return None;
    }
    let field = |start: usize, end: usize| -> Option<i64> { stamp.get(start..end)?.parse().ok() };
    let month = u8::try_from(field(5, 7)?).ok()?;
    let day = u8::try_from(field(8, 10)?).ok()?;
    let hour = u8::try_from(field(11, 13)?).ok()?;
    let minute = u8::try_from(field(14, 16)?).ok()?;
    let second = u8::try_from(field(17, 19)?).ok()?;
    let date = time::Date::from_calendar_date(
        i32::try_from(field(0, 4)?).ok()?,
        time::Month::try_from(month).ok()?,
        day,
    )
    .ok()?;
    let time = time::Time::from_hms(hour, minute, second).ok()?;
    Some(date.with_time(time).assume_utc())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Mutex;

    use time::macros::datetime;

    use super::*;

    /// Thirty minutes, the default interval, as the section reports it.
    fn schedule() -> ScheduleSection {
        ScheduleSection::default()
    }

    /// A ledger double: every round row, open or closed, plus the closes made.
    #[derive(Default)]
    struct FakeLedger {
        rounds: Mutex<Vec<FakeRound>>,
    }

    struct FakeRound {
        id: RoundId,
        started_at: String,
        closed: bool,
        note: Option<String>,
    }

    impl FakeLedger {
        fn with(rounds: Vec<FakeRound>) -> Self {
            Self {
                rounds: Mutex::new(rounds),
            }
        }

        fn round(&self, id: RoundId) -> (bool, Option<String>) {
            let rounds = self.rounds.lock().unwrap();
            let round = rounds.iter().find(|round| round.id == id).unwrap();
            (round.closed, round.note.clone())
        }
    }

    impl OrphanLedger for FakeLedger {
        fn open_rounds(&self) -> DomainResult<Vec<(RoundId, String)>> {
            let rounds = self.rounds.lock().unwrap();
            Ok(rounds
                .iter()
                .filter(|round| !round.closed)
                .map(|round| (round.id, round.started_at.clone()))
                .collect())
        }

        fn close_orphan_round(&self, round_id: RoundId, note: &str) -> DomainResult<()> {
            let mut rounds = self.rounds.lock().unwrap();
            let round = rounds
                .iter_mut()
                .find(|round| round.id == round_id)
                .expect("the reaper only closes rounds it was shown");
            assert!(!round.closed, "a closed round must never be closed twice");
            round.closed = true;
            round.note = Some(note.to_string());
            Ok(())
        }
    }

    fn open(id: RoundId, started_at: &str) -> FakeRound {
        FakeRound {
            id,
            started_at: started_at.to_string(),
            closed: false,
            note: None,
        }
    }

    fn closed(id: RoundId, started_at: &str, note: Option<&str>) -> FakeRound {
        FakeRound {
            id,
            started_at: started_at.to_string(),
            closed: true,
            note: note.map(str::to_string),
        }
    }

    /// A disabled schedule starts nothing and clears the panel's next_run.
    #[test]
    fn disabled_schedule_has_no_next_run() {
        let off = ScheduleSection {
            enabled: false,
            ..schedule()
        };
        let started = datetime!(2026-10-20 12:00:00 UTC);
        for busy in [false, true] {
            assert_eq!(
                decide(&off, Some(started), started, started, busy),
                (SchedulerDecision::Disabled, None)
            );
        }
    }

    /// An empty ledger waits out the startup grace, then starts.
    #[test]
    fn empty_ledger_waits_the_startup_grace() {
        let process = datetime!(2026-10-20 12:00:00 UTC);
        assert_eq!(
            decide(
                &schedule(),
                None,
                process,
                process + time::Duration::seconds(4),
                false
            ),
            (SchedulerDecision::NotDue, Some(process + STARTUP_GRACE))
        );
        // The deadline does not drift with the poll time.
        assert_eq!(
            decide(
                &schedule(),
                None,
                process,
                process + time::Duration::seconds(5),
                false
            ),
            (
                SchedulerDecision::Start,
                Some(process + STARTUP_GRACE + time::Duration::minutes(30))
            )
        );
    }

    /// The deadline follows the last round's start, not the wall clock.
    #[test]
    fn due_follows_the_last_round_start() {
        let last = datetime!(2026-10-20 12:00:00 UTC);
        let due = last + time::Duration::minutes(30);
        assert_eq!(
            decide(
                &schedule(),
                Some(last),
                last,
                due - time::Duration::seconds(1),
                false
            ),
            (SchedulerDecision::NotDue, Some(due))
        );
        let fired = decide(&schedule(), Some(last), last, due, false);
        assert_eq!(fired.0, SchedulerDecision::Start);
        assert_eq!(fired.1, Some(due + time::Duration::minutes(30)));
    }

    /// A round in flight is never doubled up, whatever the deadline says.
    #[test]
    fn busy_never_starts_and_still_reports_the_deadline() {
        let last = datetime!(2026-10-20 12:00:00 UTC);
        let due = last + time::Duration::minutes(30);
        assert_eq!(
            decide(&schedule(), Some(last), last, due, true),
            (SchedulerDecision::Busy, Some(due))
        );
        // Not due and busy: the label differs, the deadline does not.
        assert_eq!(
            decide(&schedule(), Some(last), last, last, true),
            (SchedulerDecision::Busy, Some(due))
        );
        let process = datetime!(2026-10-20 12:00:00 UTC);
        assert_eq!(
            decide(&schedule(), None, process, process + STARTUP_GRACE, true),
            (SchedulerDecision::Busy, Some(process + STARTUP_GRACE))
        );
    }

    /// A hand-edited zero is clamped to one minute and cannot busy-loop.
    #[test]
    fn zero_interval_is_clamped_to_one_minute() {
        let zero = ScheduleSection {
            enabled: true,
            interval_minutes: 0,
        };
        assert_eq!(zero.interval().as_secs(), 60);
        let last = datetime!(2026-10-20 12:00:00 UTC);
        assert_eq!(
            decide(
                &zero,
                Some(last),
                last,
                last + time::Duration::seconds(59),
                false
            ),
            (
                SchedulerDecision::NotDue,
                Some(last + time::Duration::minutes(1))
            )
        );
        let fired = decide(
            &zero,
            Some(last),
            last,
            last + time::Duration::minutes(1),
            false,
        );
        assert_eq!(fired.0, SchedulerDecision::Start);
        // The next deadline is a minute away, never "right now".
        assert_eq!(fired.1, Some(last + time::Duration::minutes(2)));
    }

    /// Only rounds past the budget are reaped; the rest keep their row.
    #[test]
    fn reaps_only_rounds_past_the_budget() {
        let ledger = FakeLedger::with(vec![
            open(7, "2026-10-20T11:00:00"),
            open(8, "2026-10-20T12:50:00"),
            closed(9, "2026-10-20T09:00:00", Some("finished")),
        ]);
        let now = datetime!(2026-10-20 12:55:00 UTC);
        let closed_ids =
            reap_orphan_rounds(&ledger, now, StdDuration::from_secs(30 * 60), None).unwrap();
        assert_eq!(closed_ids, vec![7]);
        assert_eq!(ledger.round(7), (true, Some(ORPHAN_NOTE.to_string())));
        assert_eq!(ledger.round(8), (false, None));
        assert_eq!(ledger.round(9), (true, Some("finished".to_string())));
    }

    /// The round this process still has in flight is never reaped out from
    /// under the runner, however far past the budget it drained.
    #[test]
    fn a_running_round_is_never_reaped_even_past_the_budget() {
        let ledger = FakeLedger::with(vec![
            open(31, "2026-10-20T11:00:00"),
            open(32, "2026-10-20T11:30:00"),
        ]);
        let now = datetime!(2026-10-20 12:55:00 UTC);
        let closed_ids =
            reap_orphan_rounds(&ledger, now, StdDuration::from_secs(30 * 60), Some(31)).unwrap();
        assert_eq!(closed_ids, vec![32], "only the foreign orphan closes");
        assert_eq!(
            ledger.round(31),
            (false, None),
            "our own round keeps its row"
        );
        assert_eq!(ledger.round(32), (true, Some(ORPHAN_NOTE.to_string())));
    }

    /// A round exactly at the budget edge is still young enough to be running.
    #[test]
    fn a_round_at_the_budget_edge_is_not_reaped() {
        let ledger = FakeLedger::with(vec![open(11, "2026-10-20T12:25:00")]);
        let now = datetime!(2026-10-20 12:55:00 UTC);
        let closed_ids =
            reap_orphan_rounds(&ledger, now, StdDuration::from_secs(30 * 60), None).unwrap();
        assert!(closed_ids.is_empty());
        assert_eq!(ledger.round(11), (false, None));
    }

    /// Both stored formats are read as UTC; anything else is never auto-closed.
    #[test]
    fn legacy_timestamps_parse_and_unusable_ones_stay_open() {
        let ledger = FakeLedger::with(vec![
            open(20, "2026-10-20 10:00:00"),
            open(21, "not-a-timestamp"),
            open(22, ""),
        ]);
        let now = datetime!(2026-10-20 12:00:00 UTC);
        let closed_ids =
            reap_orphan_rounds(&ledger, now, StdDuration::from_secs(60 * 60), None).unwrap();
        assert_eq!(closed_ids, vec![20]);
        assert_eq!(ledger.round(20), (true, Some(ORPHAN_NOTE.to_string())));
        assert_eq!(ledger.round(21), (false, None));
        assert_eq!(ledger.round(22), (false, None));
    }

    /// The note is the exact Python string, and it is what the row carries.
    #[test]
    fn orphan_note_matches_python() {
        assert_eq!(
            ORPHAN_NOTE,
            "orphaned: 进程在轮次中途退出（重建/被杀），本轮无结果"
        );
    }

    /// A temporary ledger file; the guard removes it and SQLite's sidecars.
    struct TempLedger(PathBuf);

    impl TempLedger {
        fn new() -> Self {
            let unique = format!(
                "probe-scheduler-{}-{}.sqlite",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            );
            Self(std::env::temp_dir().join(unique))
        }
    }

    impl Drop for TempLedger {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
            for suffix in ["-wal", "-shm"] {
                let _ = std::fs::remove_file(format!("{}{suffix}", self.0.display()));
            }
        }
    }

    /// Reaping against a real ledger: the row past the budget is closed, the
    /// younger row and the already closed one keep theirs.
    #[test]
    fn reaps_a_real_ledger_row_past_the_budget() {
        let temp = TempLedger::new();
        let storage = probe_storage::Storage::open(&temp.0).unwrap();
        let budget = StdDuration::from_secs(30 * 60);

        // start_round stamps the real clock and nothing public writes a chosen
        // started_at, so the two open rounds are aged apart by crossing a second
        // boundary. Waiting, instead of fabricating a timestamp, keeps the rows
        // genuine: the reaper then sees exactly what a deployment sees.
        let overdue = storage.start_round("schedule", None).unwrap();
        std::thread::sleep(StdDuration::from_millis(1200));
        let young = storage.start_round("schedule", None).unwrap();
        let finished = storage.start_round("cli", None).unwrap();
        storage.finish_round(finished, Some("finished")).unwrap();

        let started = |round_id| {
            parse_stored_instant(&storage.round_by_id(round_id).unwrap().unwrap().started_at)
                .expect("start_round writes the stored format")
        };
        assert!(
            started(overdue) < started(young),
            "the two open rounds must be aged apart"
        );
        // Exactly the budget after the young round: the older row is past it by
        // the second the two are apart, the young one sits on the edge.
        let now = started(young) + time::Duration::minutes(30);

        let reaped = reap_orphan_rounds(&storage, now, budget, None).unwrap();
        assert_eq!(reaped, vec![overdue]);

        let overdue_row = storage.round_by_id(overdue).unwrap().unwrap();
        assert!(
            overdue_row.finished_at.is_some(),
            "a reaped round is closed"
        );
        assert_eq!(overdue_row.note.as_deref(), Some(ORPHAN_NOTE));
        assert_eq!(overdue_row.total, 0);
        assert!(
            overdue_row.inconclusive,
            "a reaped round is an ADR-0005 inconclusive round: its ok=0 \
             must never become the next round's alive-floor baseline"
        );
        assert_eq!(
            overdue_row.duration_s,
            Some(0.0),
            "an orphan measured nothing, so Python's zero duration is the honest one"
        );
        // The reaped row is skipped as a baseline, exactly like a round that
        // measured nothing.
        assert_eq!(storage.previous_alive_count().unwrap(), 0);
        let young_alive = probe_storage::RoundGuardSummary {
            total: 5,
            ok: 4,
            failed: 1,
            ..Default::default()
        };
        let clean = storage.start_round("schedule", None).unwrap();
        storage
            .finish_round_full_timed(clean, Some("clean"), &young_alive, 30.0)
            .unwrap();
        assert_eq!(storage.previous_alive_count().unwrap(), 4);

        let young_row = storage.round_by_id(young).unwrap().unwrap();
        assert!(
            young_row.finished_at.is_none(),
            "a young round keeps running"
        );
        assert_eq!(young_row.note, None);
        assert_eq!(storage.open_round_ids().unwrap(), vec![young]);

        let finished_row = storage.round_by_id(finished).unwrap().unwrap();
        assert!(finished_row.finished_at.is_some());
        assert_eq!(finished_row.note.as_deref(), Some("finished"));
    }
}
