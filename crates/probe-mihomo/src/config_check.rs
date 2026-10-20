//! Native config validation. Only a completed kernel check can pass or reject
//! a configuration; unavailable validators and interrupted checks are Degraded.
//! Kernel output is bounded but sensitive: use it in memory for culprit lookup,
//! never in logs, status payloads or error messages.

use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone, PartialEq, Eq)]
pub enum ConfigCheck {
    /// The kernel accepted the config; carries bounded, sensitive diagnostics.
    Passed(String),
    /// A completed kernel run rejected it; carries bounded, sensitive diagnostics.
    Failed(String),
    /// Validation was unavailable or incomplete. Never a pass or rejection.
    Degraded(String),
}

impl std::fmt::Debug for ConfigCheck {
    /// Format verdicts without leaking kernel diagnostics into incidental logs.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let verdict = match self {
            Self::Passed(_) => "Passed",
            Self::Failed(_) => "Failed",
            Self::Degraded(_) => "Degraded",
        };
        formatter.debug_tuple(verdict).field(&"[redacted]").finish()
    }
}

impl ConfigCheck {
    /// Return true only for a completed, successful native validation.
    pub fn is_ok(&self) -> bool {
        matches!(self, ConfigCheck::Passed(_))
    }
}

const KERNEL_TIMEOUT: Duration = Duration::from_secs(120);
const DOCKER_PROBE_TIMEOUT: Duration = Duration::from_secs(3);
const DOCKER_PROBE_TTL: Duration = Duration::from_secs(60);
const POLL_INTERVAL: Duration = Duration::from_millis(10);
const PIPE_POLL_INTERVAL: Duration = Duration::from_millis(1);
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(1);
const MAX_PIPE_BYTES: usize = 64 * 1024;
const MAX_OUTPUT_BYTES: usize = 2 * MAX_PIPE_BYTES;
const TRUNCATED: &str = "\n[validator output truncated]";
const VALIDATOR_IMAGE: &str = "metacubex/mihomo:v1.19.32";

/// Return an available Docker CLI, caching only bounded daemon probes.
fn docker_cli() -> Option<PathBuf> {
    static CACHE: Mutex<Option<(Instant, Option<PathBuf>)>> = Mutex::new(None);
    // A concurrent check must not queue behind a slow daemon probe.
    let mut cache = CACHE.try_lock().ok()?;
    if let Some((at, path)) = cache.as_ref() {
        if at.elapsed() < DOCKER_PROBE_TTL {
            return path.clone();
        }
    }
    let found = find_docker().filter(|cli| {
        let mut command = Command::new(cli);
        command.args(["version", "--format", "{{.Server.Version}}"]);
        matches!(run_process(command, DOCKER_PROBE_TIMEOUT), Ok((status, _)) if status.success())
    });
    *cache = Some((Instant::now(), found.clone()));
    found
}

/// Search PATH for the fixed Docker executable name; never invoke a shell.
fn find_docker() -> Option<PathBuf> {
    let executable = if cfg!(windows) {
        "docker.exe"
    } else {
        "docker"
    };
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|directory| directory.join(executable))
        .find(|candidate| candidate.is_file())
}

/// Drain a pipe until EOF/cancellation, retaining only its first MAX_PIPE_BYTES.
fn drain_pipe(
    mut pipe: impl Read + platform::Pipe,
    cancelled: &AtomicBool,
) -> io::Result<(Vec<u8>, bool)> {
    let mut retained = Vec::new();
    let mut truncated = false;
    let mut buffer = [0; 8192];
    while !cancelled.load(Ordering::Acquire) {
        let Some(available) = pipe.ready()? else {
            std::thread::sleep(PIPE_POLL_INTERVAL);
            continue;
        };
        if available == 0 {
            break;
        }
        let length = available.min(buffer.len());
        let count = match pipe.read(&mut buffer[..length]) {
            Ok(0) => break,
            Ok(count) => count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        let keep = count.min(MAX_PIPE_BYTES - retained.len());
        retained.extend_from_slice(&buffer[..keep]);
        truncated |= keep != count;
    }
    Ok((retained, truncated))
}

/// Merge bounded streams into UTF-8 without splitting a character or growing
/// beyond MAX_OUTPUT_BYTES when invalid bytes expand into replacement characters.
fn merge_output(stdout: (Vec<u8>, bool), stderr: (Vec<u8>, bool)) -> String {
    let mut bytes = stdout.0;
    if !bytes.is_empty() && !stderr.0.is_empty() {
        bytes.push(b'\n');
    }
    bytes.extend_from_slice(&stderr.0);
    let mut text = String::from_utf8_lossy(&bytes).into_owned();
    let truncated = stdout.1 || stderr.1 || text.len() > MAX_OUTPUT_BYTES;
    if truncated {
        let mut boundary = (MAX_OUTPUT_BYTES - TRUNCATED.len()).min(text.len());
        while !text.is_char_boundary(boundary) {
            boundary -= 1;
        }
        text.truncate(boundary);
        text.push_str(TRUNCATED);
    }
    text
}

struct ManagedChild {
    child: Child,
    scope: platform::ProcessScope,
}

impl ManagedChild {
    /// Stop the owned process tree and reap its direct child, reporting failures.
    fn stop(&mut self) -> Result<(), &'static str> {
        let tree_result = self.scope.terminate();
        if self
            .child
            .try_wait()
            .map_err(|_| "validator wait failed")?
            .is_none()
        {
            // A process can exit between try_wait and kill; reap that race too.
            if self.child.kill().is_err()
                && self
                    .child
                    .try_wait()
                    .map_err(|_| "validator wait failed")?
                    .is_none()
            {
                return Err("validator cleanup failed");
            }
        }
        reap_child(&mut self.child)?;
        tree_result.map_err(|_| "validator process-tree cleanup failed")
    }
}

/// Reap an already stopped child without an unbounded wait on OS termination.
fn reap_child(child: &mut Child) -> Result<(), &'static str> {
    let started = Instant::now();
    loop {
        if child
            .try_wait()
            .map_err(|_| "validator reap failed")?
            .is_some()
        {
            return Ok(());
        }
        if started.elapsed() >= CLEANUP_TIMEOUT {
            return Err("validator cleanup deadline exceeded");
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

impl Drop for ManagedChild {
    /// Ensure early returns also stop the owned child; normal cleanup is checked.
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

/// Run a fixed command with null stdin and concurrent, bounded pipe drains.
/// Only completed exits return output; all lifecycle failures are unavailable.
fn run_process(
    mut command: Command,
    timeout: Duration,
) -> Result<(ExitStatus, String), &'static str> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    platform::prepare(&mut command);
    let started = Instant::now();
    let mut child = command.spawn().map_err(|_| "validator could not start")?;
    let scope = match platform::ProcessScope::attach(&child) {
        Ok(scope) => scope,
        Err(_) => {
            let _ = child.kill();
            let _ = reap_child(&mut child);
            return Err("validator process containment unavailable");
        }
    };
    let mut process = ManagedChild { child, scope };
    let stdout = process
        .child
        .stdout
        .take()
        .ok_or("validator stdout unavailable")?;
    let stderr = process
        .child
        .stderr
        .take()
        .ok_or("validator stderr unavailable")?;
    let cancelled = Arc::new(AtomicBool::new(false));
    let stop_stdout = Arc::clone(&cancelled);
    let stdout = std::thread::Builder::new()
        .spawn(move || drain_pipe(stdout, &stop_stdout))
        .map_err(|_| "validator output reader unavailable")?;
    let stop_stderr = Arc::clone(&cancelled);
    let stderr = match std::thread::Builder::new().spawn(move || drain_pipe(stderr, &stop_stderr)) {
        Ok(reader) => reader,
        Err(_) => {
            cancelled.store(true, Ordering::Release);
            let _ = stdout.join();
            return Err("validator output reader unavailable");
        }
    };
    let result = wait_for_process(&mut process.child, &stdout, &stderr, started, timeout);
    let cleanup = process.stop();
    cancelled.store(true, Ordering::Release);
    let stdout = stdout.join();
    let stderr = stderr.join();
    cleanup?;
    let status = result?;
    let stdout = stdout
        .map_err(|_| "validator output reader failed")?
        .map_err(|_| "validator output read failed")?;
    let stderr = stderr
        .map_err(|_| "validator output reader failed")?
        .map_err(|_| "validator output read failed")?;
    Ok((status, merge_output(stdout, stderr)))
}

/// Allow sibling mock tests to run their own test binary with bounded cleanup.
#[cfg(test)]
pub(crate) fn run_test_command(
    command: Command,
    timeout: Duration,
) -> Result<(ExitStatus, String), &'static str> {
    run_process(command, timeout)
}

/// Poll process and drain completion against one deadline, including inherited
/// pipes held by descendants. Returns the direct child's completed exit status.
fn wait_for_process<T>(
    child: &mut Child,
    stdout: &std::thread::JoinHandle<T>,
    stderr: &std::thread::JoinHandle<T>,
    started: Instant,
    timeout: Duration,
) -> Result<ExitStatus, &'static str> {
    loop {
        if started.elapsed() >= timeout {
            return Err("config validation timed out; no verdict");
        }
        if let Some(status) = child.try_wait().map_err(|_| "validator wait failed")? {
            if stdout.is_finished() && stderr.is_finished() {
                return Ok(status);
            }
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Interpret only normal kernel exits as config verdicts; a signal is unavailable.
///
/// Docker reports its own failures as 125..=127 and a container killed by a
/// signal (OOM/SIGKILL = 137, SIGSEGV = 139) as 128+n: none of those is a
/// config verdict, and reading one as `Failed` blocked rounds with a wrong
/// "kernel rejects the generated config" diagnosis on memory-pressured hosts.
fn interpret(result: Result<(ExitStatus, String), &'static str>, docker: bool) -> ConfigCheck {
    match result {
        Ok((status, output)) if status.success() => ConfigCheck::Passed(output),
        Ok((status, _))
            if status.code().is_none_or(|code| code < 0)
                || (docker && status.code().is_some_and(|code| code >= 125)) =>
        {
            ConfigCheck::Degraded("validator did not complete a config check".into())
        }
        Ok((_, output)) => ConfigCheck::Failed(output),
        Err(message) => ConfigCheck::Degraded(message.into()),
    }
}

/// Validate one explicit config file with an explicit binary, using exactly
/// `-t -f <canonical-config>`. No environment lookup, Docker fallback or shell.
/// Timeout is clamped to 1ms..=120s; missing files and incomplete runs degrade.
pub fn validate_with_binary(binary: &Path, config_path: &Path, timeout: Duration) -> ConfigCheck {
    let Some(binary) = canonical_file(binary) else {
        return ConfigCheck::Degraded("validator binary unavailable".into());
    };
    if cfg!(windows)
        && !binary
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("exe"))
    {
        return ConfigCheck::Degraded("validator must be a native executable".into());
    }
    let Some(config_path) = canonical_file(config_path) else {
        return ConfigCheck::Degraded("validation config file unavailable".into());
    };
    let mut command = Command::new(binary);
    command.arg("-t").arg("-f").arg(config_path);
    interpret(
        run_process(
            command,
            timeout.clamp(Duration::from_millis(1), KERNEL_TIMEOUT),
        ),
        false,
    )
}

/// Resolve an existing regular file, returning None without exposing path errors.
fn canonical_file(path: &Path) -> Option<PathBuf> {
    let resolved = path.canonicalize().ok()?;
    resolved.is_file().then_some(resolved)
}

/// Validate `<host_dir>/config.yaml` with MIHOMO_BIN, then the pinned Docker
/// validator only when MIHOMO_BIN is unset. Invalid explicit binaries degrade.
/// A symlink escaping the caller's config directory is never passed to a kernel.
pub fn validate(host_dir: &Path) -> ConfigCheck {
    let Ok(directory) = host_dir.canonicalize() else {
        return ConfigCheck::Degraded("validation directory unavailable".into());
    };
    let Some(config) = canonical_file(&directory.join("config.yaml")) else {
        return ConfigCheck::Degraded("validation config file unavailable".into());
    };
    if config.parent() != Some(directory.as_path()) {
        return ConfigCheck::Degraded("validation config escapes its directory".into());
    }
    if let Some(binary) = std::env::var_os("MIHOMO_BIN") {
        return validate_with_binary(Path::new(&binary), &config, KERNEL_TIMEOUT);
    }
    match docker_cli() {
        Some(cli) => validate_with_docker(&cli, &config),
        None => ConfigCheck::Degraded("no available native or Docker validator; no verdict".into()),
    }
}

/// Run only an owned, uniquely named validation container with one read-only
/// config mount. Cleanup targets that generated name, never a deployment name.
fn validate_with_docker(cli: &Path, config: &Path) -> ConfigCheck {
    static NEXT_CONTAINER: AtomicU64 = AtomicU64::new(0);
    let Some(config) = config.to_str().filter(|path| !path.contains(',')) else {
        return ConfigCheck::Degraded("validation path cannot be mounted".into());
    };
    let Ok(timestamp) = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) else {
        return ConfigCheck::Degraded("validation container identity unavailable".into());
    };
    let name = format!(
        "probe-mihomo-validate-{}-{:x}-{}",
        std::process::id(),
        timestamp.as_nanos(),
        NEXT_CONTAINER.fetch_add(1, Ordering::Relaxed)
    );
    let mut command = Command::new(cli);
    command.args([
        "run",
        "--rm",
        "--pull=never",
        "--network=none",
        "--read-only",
        "--cap-drop=ALL",
        "--security-opt=no-new-privileges",
        "--tmpfs",
        "/root/.config/mihomo",
        "--tmpfs",
        "/tmp",
        "--name",
        &name,
        "--mount",
        &format!("type=bind,source={config},target=/config.yaml,readonly"),
        VALIDATOR_IMAGE,
        "-t",
        "-f",
        "/config.yaml",
    ]);
    let result = run_process(command, KERNEL_TIMEOUT);
    // A killed Docker CLI does not kill its daemon-owned container.
    if result.is_err()
        || matches!(&result, Ok((status, _)) if status.code().is_none_or(|code| code < 0))
    {
        let mut cleanup = Command::new(cli);
        cleanup.args(["rm", "--force", &name]);
        if !matches!(run_process(cleanup, DOCKER_PROBE_TIMEOUT), Ok((status, _)) if status.success())
        {
            return ConfigCheck::Degraded(
                "Docker validation incomplete; cleanup not confirmed".into(),
            );
        }
    }
    interpret(result, true)
}

/// One candidate the kernel's error text may point at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyRef {
    pub name: String,
    pub server: String,
}

/// Locate one unambiguous proxy index, preferring quoted names, then considering
/// bounded names and server tokens together. Ambiguity in a stronger tier stops
/// lookup; truncated diagnostics never authorize pruning.
pub fn culprit_from(output: &str, proxies: &[ProxyRef]) -> Option<usize> {
    if output.contains(TRUNCATED) {
        return None;
    }
    for tier in 0..2 {
        let mut candidate = None;
        for (index, proxy) in proxies.iter().enumerate() {
            let matched = match tier {
                0 => {
                    !proxy.name.is_empty()
                        && serde_json::to_string(&proxy.name)
                            .is_ok_and(|quoted| output.contains(&quoted))
                }
                _ => {
                    (proxy.name.chars().count() >= 4 && find_word(output, &proxy.name, false))
                        || (proxy.server.chars().count() >= 4
                            && find_word(output, &proxy.server, true))
                }
            };
            if matched {
                if candidate.is_some() {
                    return None;
                }
                candidate = Some(index);
            }
        }
        if candidate.is_some() {
            return candidate;
        }
    }
    None
}

/// Match a case-insensitive token on Unicode character boundaries. Only known
/// separators count as boundaries, so combining marks cannot split a name.
///
/// The boundary exceptions are `_` and `-` and nothing else for a NAME,
/// matching Python's `(?<![\w-])` / `(?![\w-])`: a `.` IS a boundary there, so
/// kernel output like `proxy 'v1.tokyo' is not a valid proxy` must match a
/// node named `tokyo`. Treating `.` as part of the name found no culprit and
/// blocked the whole round where Python pruned the node and proceeded.
///
/// The SERVER tier keeps the stricter rule (a `.` glues `sub.example.test`
/// together and `198.51.100.9` must never match `198.51.100.90`): Python's
/// server tier is a raw substring match that would prune an innocent node
/// there, and blocking the round is the fail-safe direction.
fn find_word(haystack: &str, needle: &str, server: bool) -> bool {
    if needle.is_empty() {
        return false;
    }
    let haystack = haystack.to_lowercase();
    let needle = needle.to_lowercase();
    let is_ipv6 = server && needle.contains(':');
    haystack.match_indices(&needle).any(|(start, matched)| {
        let end = start + matched.len();
        let is_boundary = |character: char| {
            character.is_whitespace()
                || (character.is_ascii_punctuation()
                    && !matches!(character, '_' | '-')
                    && (!server || character != '.')
                    && !(is_ipv6 && character == ':'))
        };
        haystack[..start]
            .chars()
            .next_back()
            .is_none_or(is_boundary)
            && haystack[end..].chars().next().is_none_or(is_boundary)
    })
}

#[cfg(unix)]
mod platform {
    use super::*;
    use std::os::fd::AsRawFd;
    use std::os::unix::process::CommandExt;

    #[repr(C)]
    struct PollFd {
        fd: i32,
        events: i16,
        revents: i16,
    }

    unsafe extern "C" {
        fn poll(fds: *mut PollFd, count: usize, timeout: i32) -> i32;
        fn kill(pid: i32, signal: i32) -> i32;
    }

    pub trait Pipe {
        /// Return readable capacity or None without blocking; EOF is readable.
        fn ready(&self) -> io::Result<Option<usize>>;
    }

    impl<T: AsRawFd> Pipe for T {
        fn ready(&self) -> io::Result<Option<usize>> {
            let mut descriptor = PollFd {
                fd: self.as_raw_fd(),
                events: 1,
                revents: 0,
            };
            // The descriptor remains owned by the reader throughout this call.
            let result = unsafe { poll(&mut descriptor, 1, 0) };
            if result < 0 {
                let error = io::Error::last_os_error();
                return if error.kind() == io::ErrorKind::Interrupted {
                    Ok(None)
                } else {
                    Err(error)
                };
            }
            Ok((result > 0).then_some(8192))
        }
    }

    /// Isolate the validator and its descendants in a new process group.
    pub fn prepare(command: &mut Command) {
        command.process_group(0);
    }

    pub struct ProcessScope(i32);

    impl ProcessScope {
        /// Record the process group created by prepare for this owned child.
        pub fn attach(child: &Child) -> io::Result<Self> {
            Ok(Self(child.id() as i32))
        }

        /// Kill only the owned process group; an already absent group is clean.
        pub fn terminate(&mut self) -> io::Result<()> {
            if self.0 == 0 {
                return Ok(());
            }
            // A negative PID addresses the process group, never the parent group.
            let result = unsafe { kill(-self.0, 9) };
            let error = io::Error::last_os_error();
            if result == 0 || error.raw_os_error() == Some(3) {
                self.0 = 0;
                Ok(())
            } else {
                Err(error)
            }
        }
    }
}

#[cfg(windows)]
mod platform {
    use super::*;
    use std::ffi::c_void;
    use std::os::windows::io::{AsRawHandle, RawHandle};
    use std::os::windows::process::CommandExt;

    #[repr(C)]
    #[derive(Default)]
    struct BasicLimits {
        process_time: i64,
        job_time: i64,
        flags: u32,
        minimum_working_set: usize,
        maximum_working_set: usize,
        active_processes: u32,
        affinity: usize,
        priority: u32,
        scheduling: u32,
    }

    #[repr(C)]
    #[derive(Default)]
    struct ExtendedLimits {
        basic: BasicLimits,
        io_counters: [u64; 6],
        process_memory: usize,
        job_memory: usize,
        peak_process_memory: usize,
        peak_job_memory: usize,
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn PeekNamedPipe(
            pipe: RawHandle,
            buffer: *mut c_void,
            size: u32,
            read: *mut u32,
            available: *mut u32,
            left: *mut u32,
        ) -> i32;
        fn CreateJobObjectW(attributes: *const c_void, name: *const u16) -> RawHandle;
        fn SetInformationJobObject(
            job: RawHandle,
            class: i32,
            info: *const c_void,
            length: u32,
        ) -> i32;
        fn AssignProcessToJobObject(job: RawHandle, process: RawHandle) -> i32;
        fn TerminateJobObject(job: RawHandle, code: u32) -> i32;
        fn CloseHandle(handle: RawHandle) -> i32;
    }

    #[link(name = "ntdll")]
    unsafe extern "system" {
        fn NtResumeProcess(process: RawHandle) -> i32;
    }

    pub trait Pipe {
        /// Return available bytes, EOF (zero), or None without blocking a reader.
        fn ready(&self) -> io::Result<Option<usize>>;
    }

    impl<T: AsRawHandle> Pipe for T {
        fn ready(&self) -> io::Result<Option<usize>> {
            let mut available = 0;
            // Peek never consumes bytes; this thread is the only pipe reader.
            let result = unsafe {
                PeekNamedPipe(
                    self.as_raw_handle(),
                    std::ptr::null_mut(),
                    0,
                    std::ptr::null_mut(),
                    &mut available,
                    std::ptr::null_mut(),
                )
            };
            if result != 0 {
                return Ok((available > 0).then_some(available as usize));
            }
            let error = io::Error::last_os_error();
            if matches!(error.raw_os_error(), Some(109 | 232)) {
                Ok(Some(0))
            } else {
                Err(error)
            }
        }
    }

    /// Hide windows and suspend startup until the kill-on-close job is attached,
    /// preventing a fast helper from spawning uncontained descendants.
    pub fn prepare(command: &mut Command) {
        command.creation_flags(0x0800_0004); // CREATE_NO_WINDOW | CREATE_SUSPENDED
    }

    pub struct ProcessScope(RawHandle);

    impl ProcessScope {
        /// Contain and resume the owned child; failure leaves it suspended for
        /// run_process to kill and reap without having run any untracked code.
        pub fn attach(child: &Child) -> io::Result<Self> {
            // Unnamed job, non-inheritable handle, correctly sized Win32 structure.
            let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
            if handle.is_null() {
                return Err(io::Error::last_os_error());
            }
            let scope = Self(handle);
            let limits = ExtendedLimits {
                basic: BasicLimits {
                    flags: 0x2000,
                    ..BasicLimits::default()
                },
                ..ExtendedLimits::default()
            };
            let configured = unsafe {
                SetInformationJobObject(
                    handle,
                    9,
                    &limits as *const _ as *const c_void,
                    std::mem::size_of_val(&limits) as u32,
                )
            };
            if configured == 0
                || unsafe { AssignProcessToJobObject(handle, child.as_raw_handle()) } == 0
            {
                return Err(io::Error::last_os_error());
            }
            if unsafe { NtResumeProcess(child.as_raw_handle()) } < 0 {
                return Err(io::Error::other("validator resume failed"));
            }
            Ok(scope)
        }

        /// Terminate every process in the owned job before pipe readers join.
        pub fn terminate(&mut self) -> io::Result<()> {
            if unsafe { TerminateJobObject(self.0, 1) } == 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        }
    }

    impl Drop for ProcessScope {
        /// Close the owned handle; KILL_ON_JOB_CLOSE is a final lifecycle guard.
        fn drop(&mut self) {
            unsafe { CloseHandle(self.0) };
        }
    }
}

#[cfg(test)]
mod tests;
