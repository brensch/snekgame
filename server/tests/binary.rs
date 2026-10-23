//! The real binary, configured the way deploy/snekgame.service configures
//! it: from the environment. systemd stops it with SIGTERM, and it should
//! go quietly and cleanly.

// Test code throughout, helpers included (see e2e.rs).
#![cfg(test)]

use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("snek-bin-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn server(db: &std::path::Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_snek-server"));
    cmd.env("SNEK_PORT", "0")
        .env("SNEK_WT_PORT", "0")
        .env("SNEK_DB", db)
        .env_remove("SNEK_CERT")
        .env_remove("SNEK_KEY")
        .env("RUST_LOG", "info")
        .env("NO_COLOR", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd
}

/// Start the server and wait for it to say where HTTP is listening.
/// Returns the port and the rest of its log, line by line.
fn start(db: &std::path::Path) -> (Child, u16, std::sync::mpsc::Receiver<String>) {
    let mut child = server(db).spawn().unwrap();
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            if tx.send(line.unwrap()).is_err() {
                break;
            }
        }
    });
    let deadline = Instant::now() + Duration::from_secs(20);
    let port = loop {
        let Ok(line) = rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) else {
            let _ = child.kill();
            let _ = child.wait();
            panic!("the server never said it was listening");
        };
        if let Some(addr) = line.split("snek listening on http://").nth(1) {
            break addr.rsplit(':').next().unwrap().trim().parse().unwrap();
        }
    };
    (child, port, rx)
}

fn get(port: u16, path: &str) -> String {
    let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut out = String::new();
    stream.read_to_string(&mut out).unwrap();
    out
}

/// Send `signal` and wait for the process to exit, which must be soon and
/// successfully, having said so.
fn stop(mut child: Child, logs: &std::sync::mpsc::Receiver<String>, signal: &str) {
    let status = Command::new("kill")
        .args([&format!("-{signal}"), &child.id().to_string()])
        .status()
        .unwrap();
    assert!(status.success());
    let deadline = Instant::now() + Duration::from_secs(15);
    let exit = loop {
        if let Some(exit) = child.try_wait().unwrap() {
            break exit;
        }
        if Instant::now() > deadline {
            child.kill().unwrap();
            panic!("still running {signal} later");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(exit.success(), "exited with {exit} after {signal}");
    let rest: Vec<String> = logs.try_iter().collect();
    assert!(
        rest.iter().any(|l| l.contains("snek stopped")),
        "never said it stopped: {rest:?}"
    );
}

#[test]
fn it_serves_until_sigterm_then_exits_cleanly() {
    let (child, port, logs) = start(&scratch("term").join("snek.db"));
    let reply = get(port, "/api/sessions");
    assert!(reply.starts_with("HTTP/1.1 200"), "{reply}");
    assert!(reply.ends_with("[]"), "{reply}");
    // No certificate configured: the dev route is there.
    assert!(get(port, "/api/dev/wt").contains("certHash"));
    stop(child, &logs, "TERM");
}

#[test]
fn ctrl_c_stops_it_too() {
    let (child, _, logs) = start(&scratch("int").join("snek.db"));
    stop(child, &logs, "INT");
}

/// Misconfigured, it fails at once and says why, so systemd's log shows
/// the reason rather than a restart loop of nothing.
#[test]
fn a_bad_config_fails_at_once_and_says_why() {
    let db = scratch("bad").join("no-such-dir").join("snek.db");
    let out = server(&db).output().unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("failed to open the database"),
        "stderr: {stderr}"
    );
}
