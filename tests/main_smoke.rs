//! Smoke test through the real `seestar-proxy` binary in `--replay` mode:
//! startup wiring, port binding, client delivery and SIGINT shutdown.
#![cfg(unix)]

use std::io::{BufRead, BufReader};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

const DEADLINE: Duration = Duration::from_secs(8);

/// Ask the OS for a currently free loopback port.
fn free_port() -> u16 {
    TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn connect_with_retry(port: u16, child: &mut Child) -> TcpStream {
    let start = Instant::now();
    loop {
        if let Ok(s) = TcpStream::connect((Ipv4Addr::LOCALHOST, port)) {
            return s;
        }
        if let Some(status) = child.try_wait().unwrap() {
            panic!("proxy exited before accepting connections: {status}");
        }
        assert!(start.elapsed() < DEADLINE, "control port never opened");
        std::thread::yield_now();
    }
}

fn wait_exit(child: &mut Child) -> ExitStatus {
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        if start.elapsed() > DEADLINE {
            let _ = child.kill();
            panic!("proxy did not exit after SIGINT");
        }
        std::thread::yield_now();
    }
}

#[test]
fn main_binary_replay_delivers_telescope_json_and_exits_cleanly_on_sigint() {
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/data/session_s50_fw670");
    let control = free_port();
    let mut child = Command::new(env!("CARGO_BIN_EXE_seestar-proxy"))
        .arg("--replay")
        .arg(&fixture)
        .args(["--bind", "127.0.0.1"])
        .args(["--control-port", &control.to_string()])
        .args(["--imaging-port", &free_port().to_string()])
        .args(["--dashboard-port", &free_port().to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    let stream = connect_with_retry(control, &mut child);
    stream.set_read_timeout(Some(DEADLINE)).unwrap();
    let mut line = String::new();
    let read = BufReader::new(stream).read_line(&mut line);

    // Always stop the child before asserting on what was read.
    let rc = unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGINT) };
    assert_eq!(rc, 0, "kill(SIGINT) failed");
    let status = wait_exit(&mut child);

    assert!(read.unwrap() > 0, "no data replayed to client");
    let v: serde_json::Value = serde_json::from_str(line.trim()).expect("replayed line is JSON");
    assert!(v.is_object());
    assert!(status.success(), "unclean exit: {status}");
}
