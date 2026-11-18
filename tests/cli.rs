use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_bt-emulator"))
}

fn tmp_db() -> String {
    let path = std::env::temp_dir().join(format!("bt-emu-cli-{}.db", uuid::Uuid::new_v4()));
    format!("sqlite://{}", path.display())
}

/// A running `serve`, killed on drop so a failing test leaves no server behind.
struct Server {
    child: Child,
    addr: String,
}

impl Server {
    /// Spawn `serve` on an ephemeral port with extra `args`, once it is listening.
    fn spawn(args: &[&str]) -> Self {
        let db = tmp_db();
        let mut child = bin()
            .args(["serve", "--listen", "127.0.0.1:0", "--database-url", &db])
            .args(args)
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
        Self { child, addr }
    }

    /// Send `sig` and check the server exits cleanly.
    fn stop(mut self, sig: &str) {
        let status = Command::new("kill")
            .args([sig, &self.child.id().to_string()])
            .status()
            .unwrap();
        assert!(status.success());
        assert!(self.child.wait().unwrap().success());
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// One HTTP/1.1 request over a fresh connection; returns the status line and body.
fn http(addr: &str, method: &str, path: &str, body: &str) -> (String, String) {
    let mut stream = std::net::TcpStream::connect(addr).unwrap();
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer t\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    std::io::Write::write_all(&mut stream, req.as_bytes()).unwrap();
    let mut resp = String::new();
    std::io::Read::read_to_string(&mut stream, &mut resp).unwrap();
    let (head, body) = resp.split_once("\r\n\r\n").unwrap();
    (head.lines().next().unwrap().to_owned(), body.to_owned())
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
    let server = Server::spawn(&[]);
    let (status, _) = http(&server.addr, "GET", "/", "");
    assert!(status.starts_with("HTTP/1.1 200"), "{status}");
    server.stop("-INT");
}

#[test]
fn serve_enforces_a_configured_api_key() {
    let server = Server::spawn(&["--api-key", "k"]);
    let (status, _) = http(&server.addr, "POST", "/api/apikey/login", "");
    assert!(status.starts_with("HTTP/1.1 401"), "{status}");
    server.stop("-INT");
}

#[test]
fn serve_stops_on_sigterm() {
    Server::spawn(&[]).stop("-TERM");
}

#[test]
fn serve_passes_model_and_code_flags_to_the_runtime() {
    // A one-shot model API that records the request it gets.
    let model = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let model_url = format!("http://{}/v1", model.local_addr().unwrap());
    let seen = std::thread::spawn(move || {
        let (mut conn, _) = model.accept().unwrap();
        let mut reader = BufReader::new(conn.try_clone().unwrap());
        let mut head = String::new();
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            if line == "\r\n" {
                break;
            }
            head.push_str(&line);
        }
        let reply = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"ok\":true}";
        std::io::Write::write_all(&mut conn, reply.as_bytes()).unwrap();
        head
    });
    let server = Server::spawn(&[
        "--llm-base-url",
        &model_url,
        "--llm-api-key",
        "sk-test",
        "--allow-code-execution",
    ]);
    let addr = server.addr.as_str();

    // The proxy streams the reply back (chunked).
    let (status, body) = http(addr, "POST", "/v1/proxy/chat/completions", "{}");
    assert_eq!(status, "HTTP/1.1 200 OK");
    assert!(body.contains("{\"ok\":true}"), "{body}");
    let head = seen.join().unwrap();
    assert!(head.starts_with("POST /v1/chat/completions "), "{head}");
    assert!(
        head.to_ascii_lowercase()
            .contains("authorization: bearer sk-test"),
        "{head}"
    );

    // Code execution is allowed: whatever happens to this broken handler, it is not
    // refused for want of the flag.
    let (_, project) = http(addr, "POST", "/v1/project", r#"{"name":"p"}"#);
    let pid = serde_json::from_str::<serde_json::Value>(&project).unwrap()["id"].clone();
    let function = serde_json::json!({"project_id": pid, "name": "f", "slug": "f",
        "function_data": {"type": "code", "data": {"type": "inline",
            "runtime_context": {"runtime": "node", "version": "20"}, "code": "x"}}});
    let (_, created) = http(addr, "POST", "/v1/function", &function.to_string());
    let id = serde_json::from_str::<serde_json::Value>(&created).unwrap()["id"].clone();
    assert!(id.is_string(), "{created}");
    let (status, body) = http(
        addr,
        "POST",
        &format!("/v1/function/{}/invoke", id.as_str().unwrap()),
        r#"{"input":1}"#,
    );
    assert!(!status.starts_with("HTTP/1.1 200"), "{status}");
    assert!(!body.contains("--allow-code-execution"), "{body}");
    server.stop("-INT");
}
