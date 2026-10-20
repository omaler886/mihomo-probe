use super::super::*;

pub(super) struct Fixture {
    pub(super) directory: PathBuf,
    binary: PathBuf,
}

impl Fixture {
    /// Create an isolated, uniquely owned directory; no environment is modified.
    pub(super) fn empty() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let directory = std::env::temp_dir().join(format!(
            "probe-mihomo-native-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&directory).unwrap();
        let binary = directory.join(if cfg!(windows) {
            "helper.exe"
        } else {
            "helper"
        });
        Self { directory, binary }
    }

    /// Compile the std-only native fixture with the test toolchain. Missing
    /// rustc is a failing test, not a skipped or supposedly successful check.
    fn compile() -> Self {
        let fixture = Self::empty();
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/config_check/tests/helper.rs");
        let mut compiler =
            Command::new(std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into()));
        compiler
            .arg("--edition=2021")
            .arg("--crate-name=validation_helper")
            .arg(source)
            .arg("-o")
            .arg(&fixture.binary);
        let (status, output) = run_process(compiler, KERNEL_TIMEOUT)
            .expect("native helper compiler must be available and complete");
        assert!(
            status.success(),
            "synthetic helper compilation failed: {output}"
        );
        fixture
    }

    /// Write a synthetic mode file whose path also exercises spaces and Unicode.
    fn config(&self, mode: &str) -> PathBuf {
        let config = self.directory.join(format!("{mode} space \u{4e2d}.yaml"));
        std::fs::write(&config, mode).unwrap();
        config
    }

    /// Run the public helper against a synthetic mode with a short test deadline.
    fn validate(&self, mode: &str) -> ConfigCheck {
        validate_with_binary(&self.binary, &self.config(mode), Duration::from_secs(5))
    }
}

impl Drop for Fixture {
    /// Remove only the resolved, uniquely owned test directory under temp.
    fn drop(&mut self) {
        let root = self.directory.canonicalize().unwrap();
        let temporary = std::env::temp_dir().canonicalize().unwrap();
        assert_eq!(root.parent(), Some(temporary.as_path()));
        assert!(root
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("probe-mihomo-native-"));
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[test]
fn native_fixed_argv_null_stdin_success_and_rejection() {
    let fixture = Fixture::compile();
    let passed = fixture.validate("pass");
    assert!(matches!(passed, ConfigCheck::Passed(ref text) if text.contains("accepted")));
    let failed = fixture.validate("fail");
    assert!(matches!(failed, ConfigCheck::Failed(ref text) if text.contains("\"synthetic-node\"")));
    assert!(!format!("{failed:?}").contains("synthetic-node"));
    let missing = fixture.directory.join("missing.yaml");
    assert!(matches!(
        validate_with_binary(&fixture.binary, &missing, KERNEL_TIMEOUT),
        ConfigCheck::Degraded(_)
    ));
}

#[test]
fn native_drains_both_oversized_pipes_and_bounds_lossy_utf8() {
    let fixture = Fixture::compile();
    for mode in ["large", "invalid-utf8"] {
        let check = fixture.validate(mode);
        let ConfigCheck::Passed(output) = check else {
            panic!("synthetic oversized output must not deadlock: {check:?}");
        };
        assert!(output.len() <= MAX_OUTPUT_BYTES);
        assert!(output.ends_with(TRUNCATED));
        if mode == "large" {
            assert!(output.contains("stdout-marker"));
            assert!(output.contains("stderr-marker"));
        }
    }
}

/// Verify startup and listener release within a bounded OS termination window.
fn assert_listener_closed(config: &Path) {
    let address = std::fs::read_to_string(config.with_extension("address"))
        .expect("synthetic helper must reach its hang point before timeout");
    let started = Instant::now();
    loop {
        if std::net::TcpListener::bind(address.trim()).is_ok() {
            return;
        }
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "owned helper listener survived validation cleanup"
        );
        std::thread::sleep(POLL_INTERVAL);
    }
}

#[test]
fn native_timeout_reaps_child_and_closes_inherited_descendant_pipes() {
    let fixture = Fixture::compile();
    for mode in ["hang", "descendant", "descendant-exit"] {
        let config = fixture.config(mode);
        let started = Instant::now();
        let result = validate_with_binary(&fixture.binary, &config, Duration::from_secs(2));
        assert!(
            matches!(result, ConfigCheck::Degraded(ref text) if text.contains("timed out")),
            "{result:?}"
        );
        assert!(started.elapsed() < Duration::from_secs(6));
        assert_listener_closed(&config);
    }
}

#[test]
fn failed_spawn_and_docker_reserved_exits_are_unavailable() {
    let fixture = Fixture::compile();
    let command = Command::new(fixture.directory.join("not-an-executable"));
    assert!(matches!(
        interpret(run_process(command, Duration::from_millis(50)), false),
        ConfigCheck::Degraded(_)
    ));
    let invalid_binary = fixture.directory.join("invalid.exe");
    std::fs::write(&invalid_binary, b"not a native executable").unwrap();
    assert!(matches!(
        validate_with_binary(&invalid_binary, &fixture.config("pass"), KERNEL_TIMEOUT),
        ConfigCheck::Degraded(_)
    ));
    for code in 125..=127 {
        let mut command = Command::new(&fixture.binary);
        command.args(["--exit", &code.to_string()]);
        let result = run_process(command, Duration::from_secs(5));
        assert!(matches!(interpret(result, true), ConfigCheck::Degraded(_)));
    }
    // Exercise the same bounded runner used by Docker probing, without Docker.
    let mut command = Command::new(&fixture.binary);
    let config = fixture.config("hang");
    // Production always hands the kernel a canonical path (canonical_file in
    // validate_with_binary), and the helper enforces that contract. On Windows
    // env::temp_dir() is not canonical -- fs::canonicalize returns the verbatim
    // form -- so passing the raw path made the helper panic and exit at once:
    // the runner then returned Ok, and this assertion failed for a reason that
    // had nothing to do with its deadline.
    let canonical = config.canonicalize().unwrap();
    command.arg("-t").arg("-f").arg(&canonical);
    let error = run_process(command, Duration::from_secs(2))
        .expect_err("a validator that never exits must reach the deadline");
    assert!(
        error.contains("timed out"),
        "the deadline, not a lifecycle failure, must end the run: {error}"
    );
    assert_listener_closed(&config);
}

#[cfg(unix)]
#[test]
fn signal_exit_is_not_a_configuration_rejection() {
    let fixture = Fixture::compile();
    assert!(matches!(
        fixture.validate("signal"),
        ConfigCheck::Degraded(_)
    ));
}
