use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_bt-emulator"))
}

fn tmp_db() -> String {
    let path = std::env::temp_dir().join(format!("bt-emu-cli-{}.db", uuid::Uuid::new_v4()));
    format!("sqlite://{}", path.display())
}

/// Spawn `serve` on an ephemeral port and return the child plus the bound address.
fn spawn_server() -> (Child, String) {
    let mut child = bin()
        .args([
            "serve",
            "--listen",
            "127.0.0.1:0",
            "--database-url",
            &tmp_db(),
        ])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.as_mut().unwrap())
        .read_line(&mut line)
        .unwrap();
    let addr = line
        .trim()
        .strip_prefix("listening on http://")
        .expect(&line)
        .to_owned();
    (child, addr)
}

fn signal(child: &Child, sig: &str) {
    let status = Command::new("kill")
        .args([sig, &child.id().to_string()])
        .status()
        .unwrap();
    assert!(status.success());
}

#[test]
fn version_flag_prints_version() {
    let out = bin().arg("--version").output().unwrap();
    assert!(out.status.success());
    assert!(
        String::from_utf8(out.stdout)
            .unwrap()
            .contains(env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn serve_fails_cleanly_on_bad_database_url() {
    let out = bin()
        .args(["serve", "--database-url", "mysql://nope"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(
        String::from_utf8(out.stderr)
            .unwrap()
            .contains("unsupported database url")
    );
}

#[test]
fn serve_fails_cleanly_when_port_is_taken() {
    let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = taken.local_addr().unwrap().to_string();
    let out = bin()
        .args(["serve", "--listen", &addr, "--database-url", &tmp_db()])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
}

#[test]
fn serve_answers_requests_and_stops_on_sigint() {
    let (mut child, addr) = spawn_server();
    let mut stream = std::net::TcpStream::connect(&addr).unwrap();
    std::io::Write::write_all(
        &mut stream,
        b"GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    )
    .unwrap();
    let mut resp = String::new();
    std::io::Read::read_to_string(&mut stream, &mut resp).unwrap();
    assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");
    signal(&child, "-INT");
    assert!(child.wait().unwrap().success());
}

#[test]
fn serve_stops_on_sigterm() {
    let (mut child, _) = spawn_server();
    signal(&child, "-TERM");
    assert!(child.wait().unwrap().success());
}
