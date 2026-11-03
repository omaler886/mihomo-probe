//! Process-level tests for the `gist` subcommands (workstreams/18 B2).
//!
//! Offline by construction: an unconfigured deployment has no token file, so
//! both subcommands must fail at construction -- before any request could be
//! built, let alone leave the process. The masked-output contract (no token,
//! no message bodies) is what these tests pin.

use std::path::Path;
use std::process::Command;

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_probe-cli")
}

fn run(root: &Path, args: &[&str]) -> (i32, String, String) {
    let output = Command::new(binary())
        .arg("--root")
        .arg(root)
        .args(args)
        .output()
        .expect("spawn probe-cli");
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// `gist test` on an unconfigured root: `configured=false` plus the reason
/// code, a non-zero exit, and nothing else -- no token, no path, no URL.
#[test]
fn gist_test_on_an_unconfigured_root_answers_configured_false() {
    let tmp = tempfile::tempdir().unwrap();
    let (code, out, err) = run(tmp.path(), &["gist", "test"]);
    assert_eq!(code, 1, "an unconfigured destination is not ok: {out}{err}");
    assert!(out.contains("configured=false"), "{out}");
    assert!(out.contains("error_code=config"), "{out}");
    assert!(!out.contains("token"), "{out}");
    assert!(
        !out.contains("api.github.com"),
        "no destination URL in masked output: {out}"
    );
}

/// `gist init` without a token file cannot create anything and must say so
/// without a network round-trip and without printing credential material.
#[test]
fn gist_init_without_a_token_file_refuses_to_create() {
    let tmp = tempfile::tempdir().unwrap();
    let (code, out, _err) = run(tmp.path(), &["gist", "init"]);
    assert_eq!(code, 1, "a create needs a token: {out}");
    assert!(out.contains("configured=false"), "{out}");
    assert!(out.contains("error_code=config"), "{out}");
    assert!(
        !out.contains("gist created"),
        "no Gist can be created without a token: {out}"
    );
}

/// An unknown gist subcommand is a usage error, like `db`'s.
#[test]
fn an_unknown_gist_subcommand_is_a_usage_error() {
    let tmp = tempfile::tempdir().unwrap();
    let (code, _out, err) = run(tmp.path(), &["gist", "deploy"]);
    assert_eq!(code, 2);
    assert!(err.contains("expected test | init"), "{err}");
}
