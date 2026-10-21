//! The real terminal: the view takes the terminal over, and gives it
//! back.
//!
//! The view runs inside a pseudo-terminal, so the alternate screen and
//! the raw mode are real: this is the only test that can say whether
//! the operator's shell survives the view.

#[path = "common/mod.rs"]
use common::Fleet;

use std::{
    io::Write,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

/// Run the view in a pty, leave it, and return what the terminal saw.
fn run_view(fleet: &Fleet) -> (bool, String) {
    let command = format!(
        "{} tui --server {} --key-file {}",
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
