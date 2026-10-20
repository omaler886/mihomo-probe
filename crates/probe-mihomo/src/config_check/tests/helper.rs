//! Standalone std-only executable compiled by native.rs, never a live kernel.

use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

/// Dispatch a synthetic fixture mode; invalid argv exits distinctly from the
/// intentional config-rejection mode so fixed-argument regressions fail tests.
fn main() {
    let arguments: Vec<_> = std::env::args_os().skip(1).collect();
    if arguments.len() == 2 && arguments[0] == "--exit" {
        std::process::exit(arguments[1].to_str().unwrap().parse().unwrap());
    }
    if arguments.len() == 2 && arguments[0] == "--descendant" {
        hold_listener(Path::new(&arguments[1]));
    }
    assert_eq!(arguments.len(), 3);
    assert_eq!(arguments[0], "-t");
    assert_eq!(arguments[1], "-f");
    let config = Path::new(&arguments[2]);
    assert_eq!(config, config.canonicalize().unwrap().as_path());
    assert_eq!(std::io::stdin().read(&mut [0; 1]).unwrap(), 0);
    match std::fs::read_to_string(config).unwrap().as_str() {
        "pass" => println!("accepted"),
        "fail" => {
            eprintln!("proxy \"synthetic-node\" rejected");
            std::process::exit(1);
        }
        "large" => emit_large_output(false),
        "invalid-utf8" => emit_large_output(true),
        "hang" => hold_listener(config),
        "descendant" => spawn_descendant(config, false),
        "descendant-exit" => spawn_descendant(config, true),
        "signal" => raise_signal(),
        _ => panic!("unknown synthetic mode"),
    }
}

/// Write beyond both pipe capacities simultaneously, using invalid UTF-8 when
/// requested; completion proves the validator drains while the process runs.
fn emit_large_output(invalid_utf8: bool) {
    let stderr = std::thread::spawn(move || {
        let mut output = std::io::stderr().lock();
        output.write_all(b"stderr-marker\n").unwrap();
        for _ in 0..256 {
            output.write_all(&[if invalid_utf8 { 0xff } else { b'E' }; 8192]).unwrap();
        }
    });
    let mut output = std::io::stdout().lock();
    output.write_all(b"stdout-marker\n").unwrap();
    for _ in 0..256 {
        output.write_all(&[if invalid_utf8 { 0xff } else { b'O' }; 8192]).unwrap();
    }
    stderr.join().unwrap();
}

/// Hold an exclusive loopback listener and inherited pipes until killed;
/// publish only the synthetic local address used to verify process cleanup.
fn hold_listener(config: &Path) -> ! {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    std::fs::write(config.with_extension("address"), listener.local_addr().unwrap().to_string()).unwrap();
    loop {
        std::thread::sleep(Duration::from_secs(1));
        std::hint::black_box(&listener);
    }
}

/// Start one owned helper inheriting this validator's pipes and containment,
/// then either wait or exit while the descendant keeps those pipes open.
fn spawn_descendant(config: &Path, exit_parent: bool) {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command.arg("--descendant").arg(config).stdin(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    let mut child = command.spawn().unwrap();
    if exit_parent {
        std::process::exit(0);
    }
    child.wait().unwrap();
}

/// Produce an abnormal Unix termination without a core dump, not a rejection.
fn raise_signal() {
    #[cfg(unix)]
    {
        unsafe extern "C" {
            fn raise(signal: i32) -> i32;
        }
        unsafe { raise(9) };
    }
    #[cfg(not(unix))]
    std::process::exit(1);
}
