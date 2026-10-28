//! The real terminal: the view takes the terminal over, and gives it
//! back.
//!
//! The view runs inside a pseudo-terminal, so the alternate screen and
//! the raw mode are real: this is the only test that can say whether
//! the operator's shell survives the view.

#[path = "common/mod.rs"]
mod common;
use common::Fleet;

use std::{
    io::Write,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

/// Run the view in a pty, leave it, and return what the terminal saw.
fn run_view(fleet: &Fleet) -> (bool, String) {
    let command = format!(
        // The pty starts with the size of whatever started it, and a
        // test harness has no terminal: so the size is set inside the
        // pty, before the view takes it over.
        "stty rows 40 cols 120 && {} tui --server {} --key-file {}",
        env!("CARGO_BIN_EXE_dg"),
        fleet.cli.server,
        fleet.cli.key_file.display()
    );
    let mut child = Command::new("script")
        .arg("-qec")
        .arg(&command)
        .arg("/dev/null")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("script starts a pseudo-terminal");
    let mut stdin = child.stdin.take().expect("the pipe is there");
    // The view has to be up before the key means anything, and the key
    // is buffered by the pty, so this is a pause for the drawing and
    // not a race.
    std::thread::sleep(Duration::from_millis(800));
    let _ = stdin.write_all(b"q");
    let _ = stdin.flush();
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut status = None;
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(done)) => {
                status = Some(done);
                break;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(100)),
            Err(_) => break,
        }
    }
    let status = match status {
        Some(status) => status,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            panic!("the view did not leave when the operator asked it to");
        }
    };
    let mut output = String::new();
    if let Some(mut stdout) = child.stdout.take() {
        use std::io::Read;
        let _ = stdout.read_to_string(&mut output);
    }
    (status.success(), output)
}

#[test]
fn the_view_draws_the_gateway_and_gives_the_terminal_back() {
    let fleet = Fleet::boot();
    fleet.cli.run(&["create", "demo"]);

    let (success, output) = run_view(&fleet);

    assert!(success, "the view did not exit cleanly");
    // The alternate screen was entered and left again: the operator's
    // shell is the way they found it.
    assert!(
        output.contains("\u{1b}[?1049h"),
        "the view did not take the terminal over:\n{output:?}"
    );
    assert!(
        output.contains("\u{1b}[?1049l"),
        "the view did not give the terminal back:\n{output:?}"
    );
    // And the view really drew the gateway's facts.
    assert!(
        output.contains("demo"),
        "the view did not draw the database:\n{output:?}"
    );
    assert!(
        output.contains("connected"),
        "the view did not say the gateway answered:\n{output:?}"
    );
}

#[test]
fn the_view_leaves_a_shell_that_works() {
    // The point of restoring the terminal: the command after the view
    // still behaves like a command.
    let fleet = Fleet::boot();
    let (success, _) = run_view(&fleet);
    assert!(success, "the view did not exit cleanly");
    let (ok, stdout, stderr) = fleet.cli.run(&["status", "--json"]);
    assert!(ok, "the gateway is not answering after the view: {stderr}");
    assert!(
        stdout.contains("uptime_secs"),
        "the gateway does not answer the same way after the view: {stdout}"
    );
}

/// Run a command in a pty and send it `keys`, returning what the
/// terminal saw.
fn pty_command(command: &str, keys: &[u8], settle_ms: u64) -> (bool, String) {
    let mut child = Command::new("script")
        .arg("-qec")
        .arg(command)
        .arg("/dev/null")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("script starts a pseudo-terminal");
    let mut stdin = child.stdin.take().expect("the pipe is there");
    std::thread::sleep(Duration::from_millis(settle_ms));
    let _ = stdin.write_all(keys);
    let _ = stdin.flush();
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut status = None;
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(done)) => {
                status = Some(done);
                break;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(100)),
            Err(_) => break,
        }
    }
    let status = match status {
        Some(status) => status,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            panic!("the command did not exit in a pty: {command}");
        }
    };
    let mut output = String::new();
    if let Some(mut stdout) = child.stdout.take() {
        use std::io::Read;
        let _ = stdout.read_to_string(&mut output);
    }
    (status.success(), output)
}

#[test]
fn explicit_tui_refuses_redirected_input_before_taking_over_the_terminal() {
    let fleet = Fleet::boot();
    let command = format!(
        "stty rows 40 cols 120 && {} tui --server {} --key-file {} </dev/null",
        env!("CARGO_BIN_EXE_dg"),
        fleet.cli.server,
        fleet.cli.key_file.display()
    );
    let (success, output) = pty_command(&command, b"", 900);
    assert!(
        !success,
        "explicit tui should refuse redirected stdin:\n{output:?}"
    );
    assert!(
        output.contains("not_a_terminal"),
        "explicit tui did not report the terminal-input prerequisite:\n{output:?}"
    );
    assert!(
        !output.contains("\u{1b}[?1049h"),
        "explicit tui took over stdout without an interactive stdin:\n{output:?}"
    );
}

// Bare `dg` used to print the subcommand list and exit 2, so a person who
// typed the program's name got a menu rather than the screen it describes.
// Now it opens the view — but only where there is a terminal to take over.
// All three cases are pinned below, because the risk runs both ways: a pipe
// that starts a full-screen view cannot be read by the script that ran it,
// and a terminal that prints usage is the bug this change exists to fix.
#[test]
fn a_bare_invocation_opens_the_view_on_a_terminal() {
    let fleet = Fleet::boot();
    fleet.cli.run(&["create", "bare"]);

    // No subcommand at all: just the credential and the server.
    let command = format!(
        "stty rows 40 cols 120 && {} --server {} --key-file {}",
        env!("CARGO_BIN_EXE_dg"),
        fleet.cli.server,
        fleet.cli.key_file.display()
    );
    let (success, output) = pty_command(&command, b"q", 900);

    assert!(success, "the bare invocation did not exit cleanly");
    assert!(
        output.contains("\u{1b}[?1049h"),
        "a bare invocation did not open the view:\n{output:?}"
    );
    assert!(
        output.contains("\u{1b}[?1049l"),
        "a bare invocation did not give the terminal back:\n{output:?}"
    );
    assert!(
        output.contains("bare"),
        "the view did not draw the database:\n{output:?}"
    );
    assert!(
        !output.contains("Usage: dg"),
        "a bare invocation printed usage instead of the view:\n{output:?}"
    );
}

// The other side of the rule. A script that runs `dg` to read its command
// list must keep getting the list: it cannot drive a full-screen view, and
// a `--json` caller has explicitly asked for machine output.
//
// The STREAM is the contract, not just the text. The required-subcommand
// build wrote everything to stderr and left stdout empty, so `dg --json |
// <json consumer>` received nothing at all. An earlier version of this
// change reproduced the text with `print_help()`, which writes to stdout:
// the exit code and the wording still looked right, but 2 KB of help prose
// arrived where a pipeline had always seen silence. Asserting only that
// "Usage: dg" appears somewhere let that through, so both streams are
// pinned here.
#[test]
fn a_bare_invocation_off_a_terminal_prints_usage_and_changes_nothing() {
    // A colour-forcing environment is included deliberately. clap honours
    // CLICOLOR_FORCE/FORCE_COLOR even when the stream is a pipe, so a
    // fallback that renders styled text and writes it verbatim keeps the
    // escape sequences, splits words like "Usage:" into styled fragments so
    // a plain-text search no longer matches, and changes the byte count. That
    // is exactly the shape a reviewer saw once, so it is a case the pin has
    // to cover rather than an exotic extra.
    let environments: [&[(&str, &str)]; 3] = [
        &[],
        &[("CLICOLOR_FORCE", "1")],
        &[("FORCE_COLOR", "1"), ("TERM", "xterm-256color")],
    ];
    for env in environments {
        for args in [vec![], vec!["--json"]] {
            let mut command = Command::new(env!("CARGO_BIN_EXE_dg"));
            command.args(&args).stdin(Stdio::null());
            for (name, value) in env {
                command.env(name, value);
            }
            let output = command.output().expect("dg runs");
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert_eq!(
                output.status.code(),
                Some(2),
                "bare `dg` {:?} should keep the old exit code",
                args
            );
            // The usage goes to stderr, where it has always gone.
            assert!(
                stderr.contains("Usage: dg"),
                "bare `dg` {:?} off a terminal must print the usage on stderr:\n{stderr}",
                args
            );
            // And stdout stays empty, so nothing new appears in a pipeline or
            // in front of a `--json` consumer.
            assert!(
                stdout.is_empty(),
                "bare `dg` {:?} must write nothing to stdout, got:\n{stdout}",
                args
            );
            // No alternate screen: nothing took the terminal over.
            assert!(
                !stderr.contains("\u{1b}[?1049h") && !stdout.contains("\u{1b}[?1049h"),
                "bare `dg` {:?} off a terminal must not start the view",
                args
            );
            // And no colour escapes either. This is written to stderr without
            // clap's own `print()` (so the stream cannot depend on which error
            // kind a release maps where), and the obvious way to get that wrong
            // is `render().ansi()`, which emits escapes unconditionally -- in a
            // captured pipe, where clap itself would have emitted none.
            assert!(
                !stderr.contains('\u{1b}'),
                "bare `dg` {:?} must not send escape sequences to a pipe:\n{stderr:?}",
                args
            );
        }
    }
}
