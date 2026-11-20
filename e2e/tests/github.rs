//! End-to-end tests of `kumikit.github` (exit criterion E3):
//! - Sets `github-token` via `secret.set`, verifies `secret.list` never leaks it.
//! - Routes `api.github.com` to a loopback fake GitHub HTTP server via
//!   `KUMIKIT_TEST_HTTP_ROUTES` (debug builds only).
//! - An agent calls read tools (no approval), `create_pull_request` (write -> approved),
//!   `create_issue_comment` (write -> denied -> fake server never receives the POST), and
//!   a rate-limited endpoint (`429` with `Retry-After`).
//! - Scans every event, every RPC message, and the core's entire stderr log to prove the
//!   token never leaks anywhere.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tempfile::TempDir;

const DEADLINE: Duration = Duration::from_secs(60);
const TEST_TOKEN: &str = "ghp_TEST_SECRET_TOKEN_0123456789abcdef";

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf()
}

fn exe(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_owned()
    }
}

#[derive(Debug, Clone)]
struct RecordedReq {
    method: String,
    path: String,
    auth: Option<String>,
    body: String,
}

/// Minimal loopback fake GitHub REST API server.
fn start_fake_github() -> (u16, Arc<Mutex<Vec<RecordedReq>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let log: Arc<Mutex<Vec<RecordedReq>>> = Arc::new(Mutex::new(Vec::new()));
    let log_bg = Arc::clone(&log);

    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut stream = stream;
            let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut req_line = String::new();
            if reader.read_line(&mut req_line).unwrap_or(0) == 0 {
                continue;
            }
            let mut parts = req_line.split_whitespace();
            let method = parts.next().unwrap_or_default().to_owned();
            let path = parts.next().unwrap_or_default().to_owned();

            let mut content_len: usize = 0;
            let mut chunked = false;
            let mut auth = None;
            loop {
                let mut h = String::new();
                if reader.read_line(&mut h).unwrap_or(0) == 0 || h == "\r\n" || h == "\n" {
                    break;
                }
                if let Some((k, v)) = h.split_once(':') {
                    let kl = k.trim().to_ascii_lowercase();
                    let vv = v.trim().to_owned();
                    if kl == "content-length" {
                        content_len = vv.parse().unwrap_or(0);
                    } else if kl == "transfer-encoding"
                        && vv.to_ascii_lowercase().contains("chunked")
                    {
                        chunked = true;
                    } else if kl == "authorization" {
                        auth = Some(vv);
                    }
                }
            }
            let body_bytes = if chunked {
                let mut out = Vec::new();
                loop {
                    let mut size_line = String::new();
                    if reader.read_line(&mut size_line).unwrap_or(0) == 0 {
                        break;
                    }
                    let size = usize::from_str_radix(size_line.trim(), 16).unwrap_or(0);
                    if size == 0 {
                        let mut trailer = String::new();
                        let _ = reader.read_line(&mut trailer);
                        break;
                    }
                    let mut chunk = vec![0u8; size];
                    let _ = reader.read_exact(&mut chunk);
                    out.extend_from_slice(&chunk);
                    let mut crlf = [0u8; 2];
                    let _ = reader.read_exact(&mut crlf);
                }
                out
            } else if content_len > 0 {
                let mut b = vec![0u8; content_len];
                let _ = reader.read_exact(&mut b);
                b
            } else {
                Vec::new()
            };
            let body = String::from_utf8_lossy(&body_bytes).into_owned();
            log_bg.lock().unwrap().push(RecordedReq {
                method: method.clone(),
                path: path.clone(),
                auth,
                body,
            });

            let (status, extra_headers, resp_body) = match (method.as_str(), path.as_str()) {
                ("GET", p) if p.starts_with("/repos/acme/demo/pulls?") => (
                    "200 OK",
                    "",
                    json!([
                        {"number": 7, "state": "open", "title": "Existing PR", "html_url": "https://github.com/acme/demo/pull/7"}
                    ])
                    .to_string(),
                ),
                ("GET", "/repos/acme/demo/commits/main/check-runs") => (
                    "200 OK",
                    "",
                    json!({
                        "total_count": 1,
                        "check_runs": [
                            {"name": "ci", "status": "completed", "conclusion": "success"}
                        ]
                    })
                    .to_string(),
                ),
                ("POST", "/repos/acme/demo/pulls") => (
                    "201 Created",
                    "",
                    json!({
                        "number": 42,
                        "state": "open",
                        "title": "Add M5 plugins",
                        "html_url": "https://github.com/acme/demo/pull/42"
                    })
                    .to_string(),
                ),
                ("GET", p) if p.starts_with("/repos/acme/limited/issues") => (
                    "429 Too Many Requests",
                    "Retry-After: 42\r\n",
                    json!({"message": "secondary rate limit"}).to_string(),
                ),
                _ => (
                    "404 Not Found",
                    "",
                    json!({"message": "Not Found"}).to_string(),
                ),
            };
            let reply = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\n{extra_headers}Content-Length: {}\r\nConnection: close\r\n\r\n{resp_body}",
                resp_body.len()
            );
            let _ = stream.write_all(reply.as_bytes());
            let _ = stream.flush();
        }
    });

    (port, log)
}

struct Bins {
    kumikit: PathBuf,
    fake_agent: PathBuf,
    acp_dir: PathBuf,
    acp_wasm: PathBuf,
    github_wasm: PathBuf,
}

impl Bins {
    fn locate() -> Self {
        let k = std::env::var_os("KUMIKIT_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| repo_root().parent().unwrap().join("kumikit"));
        let b = Bins {
            kumikit: k.join("target/debug").join(exe("kumikit")),
            fake_agent: k
                .join("target/kumikit-test/release")
                .join(exe("fake-acp-agent")),
            acp_dir: k.join("plugins/provider-acp"),
            acp_wasm: k.join("target/kumikit-test/wasm32-wasip2/release/kumikit_provider_acp.wasm"),
            github_wasm: repo_root().join("target/wasm32-wasip2/release/kumikit_github.wasm"),
        };
        for f in [&b.kumikit, &b.fake_agent, &b.acp_wasm, &b.github_wasm] {
            assert!(f.is_file(), "{} is missing", f.display());
        }
        b
    }

    fn pack(&self, dir: &Path, out: &Path) {
        let o = Command::new(&self.kumikit)
            .args(["plugin", "pack"])
            .arg(dir)
            .arg("-o")
            .arg(out)
            .output()
            .unwrap();
        assert!(
            o.status.success(),
            "kumikit plugin pack {}: {}",
            dir.display(),
            String::from_utf8_lossy(&o.stderr)
        );
    }
}

struct Core {
    tmp: TempDir,
    workspace: PathBuf,
    core: Child,
    bridge: Child,
    stdin: ChildStdin,
    rx: mpsc::Receiver<Value>,
    next_id: u64,
    events: Vec<Value>,
    all_messages: Vec<String>,
    stderr_log: Arc<Mutex<String>>,
}

impl Core {
    fn start(bins: &Bins, http_route: &str) -> Self {
        let tmp = tempfile::Builder::new().prefix("kkgh").tempdir().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let socket = if cfg!(windows) {
            format!(
                r"\\.\pipe\kkgh-{}",
                tmp.path().file_name().unwrap().to_string_lossy()
            )
        } else {
            tmp.path().join("s.sock").to_string_lossy().into_owned()
        };
        let workspace = tmp.path().join("ws");
        std::fs::create_dir_all(&workspace).unwrap();

        let mut paths = vec![bins.fake_agent.parent().unwrap().to_path_buf()];
        paths.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        let mut core = Command::new(&bins.kumikit)
            .args(["serve", "--socket", &socket, "--data-dir"])
            .arg(tmp.path().join("data"))
            .env("PATH", std::env::join_paths(paths).unwrap())
            .env("KUMIKIT_TEST_HTTP_ROUTES", http_route)
            .env("KUMIKIT_LOG", "debug")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stdout = core.stdout.take().unwrap();
        let stderr = core.stderr.take().unwrap();
        let stderr_log = Arc::new(Mutex::new(String::new()));
        let stderr_bg = Arc::clone(&stderr_log);
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                let mut buf = stderr_bg.lock().unwrap();
                buf.push_str(&line);
                buf.push('\n');
            }
        });
        let (ready_tx, ready_rx) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if line.starts_with("ready") {
                    let _ = ready_tx.send(());
                }
            }
        });
        ready_rx.recv_timeout(DEADLINE).expect("no ready line");

        let mut bridge = Command::new(&bins.kumikit)
            .args(["bridge", "--socket", &socket])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let stdin = bridge.stdin.take().unwrap();
        let out = bridge.stdout.take().unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(out).lines().map_while(Result::ok) {
                if let Ok(v) = serde_json::from_str::<Value>(&line)
                    && tx.send(v).is_err()
                {
                    break;
                }
            }
        });
        let mut c = Core {
            tmp,
            workspace,
            core,
            bridge,
            stdin,
            rx,
            next_id: 0,
            events: Vec::new(),
            all_messages: Vec::new(),
            stderr_log,
        };
        c.call(
            "core.hello",
            json!({ "protocol": "0.6", "client": { "name": "kumikit-github-e2e", "version": "0" } }),
        );
        c
    }

    fn recv(&mut self, deadline: Instant) -> Value {
        let left = deadline
            .checked_duration_since(Instant::now())
            .expect("deadline passed");
        let v = self.rx.recv_timeout(left).expect("bridge closed");
        self.all_messages.push(v.to_string());
        v
    }

    fn call(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        let req = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        writeln!(self.stdin, "{req}").unwrap();
        self.stdin.flush().unwrap();
        let deadline = Instant::now() + DEADLINE;
        loop {
            let m = self.recv(deadline);
            if m["method"] == "events.event" {
                self.events.push(m["params"].clone());
            } else if m["id"] == json!(id) {
                assert!(m.get("error").is_none(), "{method}: {}", m["error"]);
                return m["result"].clone();
            }
        }
    }

    fn event(&mut self, topic: &str, deadline: Instant) -> Value {
        if let Some(i) = self.events.iter().position(|e| e["topic"] == topic) {
            return self.events.remove(i);
        }
        loop {
            let m = self.recv(deadline);
            if m["method"] == "events.event" {
                if m["params"]["topic"] == topic {
                    return m["params"].clone();
                }
                self.events.push(m["params"].clone());
            }
        }
    }

    fn install_dir(&mut self, bins: &Bins, name: &str, dir: &Path) -> Value {
        let kkp = self.tmp.path().join(format!("{name}.kkp"));
        bins.pack(dir, &kkp);
        let p = self.call("plugin.install", json!({ "path": kkp }))["plugin"].clone();
        self.call(
            "plugin.enable",
            json!({ "plugin": p["id"], "grant": p["requested"] }),
        )["plugin"]
            .clone()
    }
}

impl Drop for Core {
    fn drop(&mut self) {
        let _ = self.bridge.kill();
        let _ = self.bridge.wait();
        let _ = self.core.kill();
        let _ = self.core.wait();
    }
}

#[test]
fn github_tools_approve_writes_handle_rate_limits_and_never_leak_token() {
    let bins = Bins::locate();
    let (port, recorded) = start_fake_github();
    let mut core = Core::start(&bins, &format!("api.github.com=http://127.0.0.1:{port}"));

    // Install provider-acp.
    let acp_stage = core.tmp.path().join("stage-acp");
    std::fs::create_dir_all(&acp_stage).unwrap();
    std::fs::copy(
        bins.acp_dir.join("plugin.toml"),
        acp_stage.join("plugin.toml"),
    )
    .unwrap();
    std::fs::copy(
        bins.acp_dir.join("acp.schema.json"),
        acp_stage.join("acp.schema.json"),
    )
    .unwrap();
    std::fs::copy(&bins.acp_wasm, acp_stage.join("plugin.wasm")).unwrap();
    core.install_dir(&bins, "provider-acp", &acp_stage);

    // Stage and install kumikit.github.
    let gh_src = repo_root().join("github");
    let gh_stage = core.tmp.path().join("stage-github");
    std::fs::create_dir_all(gh_stage.join("tools")).unwrap();
    for f in ["plugin.toml", "settings.schema.json", "README.md"] {
        std::fs::copy(gh_src.join(f), gh_stage.join(f)).unwrap();
    }
    std::fs::copy(&bins.github_wasm, gh_stage.join("plugin.wasm")).unwrap();
    for entry in std::fs::read_dir(gh_src.join("tools")).unwrap() {
        let p = entry.unwrap().path();
        std::fs::copy(&p, gh_stage.join("tools").join(p.file_name().unwrap())).unwrap();
    }
    let gh_plugin = core.install_dir(&bins, "kumikit-github", &gh_stage);
    assert_eq!(gh_plugin["id"], "kumikit.github");
    assert_eq!(gh_plugin["contributes"]["settings"], true);

    // Store the token through the protocol and confirm secret.list never reveals it.
    core.call(
        "secret.set",
        json!({ "name": "github-token", "value": TEST_TOKEN }),
    );
    let listed = core.call("secret.list", json!({}));
    assert!(
        listed["secrets"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["name"] == "github-token" && s["set"] == true),
        "{listed}"
    );
    assert!(!listed.to_string().contains(TEST_TOKEN));

    let ws = core.call(
        "workspace.open",
        json!({ "path": core.workspace.to_string_lossy() }),
    )["workspace"]["id"]
        .as_str()
        .unwrap()
        .to_owned();

    // Script an agent calling read tools, an approved PR creation, a denied comment, and a
    // rate-limited issue list.
    let transcript_path = core.tmp.path().join("gh-mcp.jsonl");
    let transcript = [
        json!({"from":"client","message":{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}}),
        json!({"from":"agent","message":{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1}}}),
        json!({"from":"client","message":{"jsonrpc":"2.0","id":2,"method":"session/new","params":{}}}),
        json!({"from":"agent","message":{"jsonrpc":"2.0","id":2,"result":{"sessionId":"s1"}}}),
        json!({"from":"client","message":{"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{}}}),
        json!({"from":"mcp","session":"s1","call":{"name":"kumikit_github__pull_requests_list","arguments":{"owner":"acme","repo":"demo"}}}),
        json!({"from":"mcp","session":"s1","call":{"name":"kumikit_github__checks_for_ref","arguments":{"owner":"acme","repo":"demo","ref":"main"}}}),
        json!({"from":"mcp","session":"s1","call":{"name":"kumikit_github__create_pull_request","arguments":{"owner":"acme","repo":"demo","title":"Add M5 plugins","head":"feat","base":"main"}}}),
        json!({"from":"mcp","session":"s1","call":{"name":"kumikit_github__create_issue_comment","arguments":{"owner":"acme","repo":"demo","number":7,"body":"Should not be sent"}}}),
        json!({"from":"mcp","session":"s1","call":{"name":"kumikit_github__issues_list","arguments":{"owner":"acme","repo":"limited"}}}),
        json!({"from":"agent","message":{"jsonrpc":"2.0","id":3,"result":{"stopReason":"end_turn"}}}),
    ]
    .iter()
    .map(Value::to_string)
    .collect::<Vec<_>>()
    .join("\n");
    std::fs::write(&transcript_path, format!("{transcript}\n")).unwrap();

    let agent_stage = core.tmp.path().join("stage-agent");
    std::fs::create_dir_all(&agent_stage).unwrap();
    let tpath = std::fs::canonicalize(&transcript_path)
        .unwrap()
        .to_string_lossy()
        .replace('\\', "\\\\");
    std::fs::write(
        agent_stage.join("plugin.toml"),
        format!(
            "id = \"test.gh-agent\"\nname = \"GH agent\"\nversion = \"0.1.0\"\nkind = [\"provider\"]\napi = \"^0.2\"\n\
             [permissions]\nspawn = [\"fake-acp-agent\"]\ntools = true\n\
             [contributes]\nproviders = [{{ id = \"agent\", name = \"Agent\", acp = {{ command = \"fake-acp-agent\", args = [\"replay\", \"{tpath}\"] }} }}]\n"
        ),
    )
    .unwrap();
    core.install_dir(&bins, "gh-agent", &agent_stage);

    let thread = core.call(
        "thread.create",
        json!({ "workspace": ws, "provider": "test.gh-agent/agent" }),
    )["thread"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let topic = format!("thread:{thread}");
    core.call("events.subscribe", json!({ "topic": topic, "after": 0 }));
    core.call(
        "thread.send",
        json!({ "thread": thread, "input": { "text": "work with github" } }),
    );

    let deadline = Instant::now() + DEADLINE;
    let mut approvals = Vec::new();
    let mut final_text = String::new();
    loop {
        let e = core.event(&topic, deadline);
        match e["kind"].as_str() {
            Some("approval.requested") => {
                let summary = e["data"]["approval"]["summary"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned();
                let aid = e["data"]["approval"]["id"].as_str().unwrap().to_owned();
                let decision = if summary.contains("create_pull_request") {
                    "allow"
                } else {
                    "deny"
                };
                approvals.push((summary, decision));
                core.call(
                    "thread.respondApproval",
                    json!({ "approval": aid, "decision": decision }),
                );
            }
            Some("text.done") => {
                final_text = e["data"]["text"].as_str().unwrap_or_default().to_owned();
            }
            Some("turn.ended") => {
                assert_eq!(e["data"]["outcome"], "done", "{e}");
                break;
            }
            _ => {}
        }
    }

    assert_eq!(approvals.len(), 2, "{approvals:?}");
    assert!(approvals[0].0.contains("create_pull_request"));
    assert!(approvals[1].0.contains("create_issue_comment"));
    assert!(final_text.contains("#7 [open] Existing PR"), "{final_text}");
    assert!(
        final_text.contains("ci: completed (success)"),
        "{final_text}"
    );
    assert!(
        final_text.contains("PR #42 [open] Add M5 plugins"),
        "{final_text}"
    );
    assert!(
        final_text.contains("retry after 42 seconds"),
        "{final_text}"
    );

    // Verify requests received by fake-github: auth header present, POST pulls received,
    // denied comment POST never sent.
    let reqs = recorded.lock().unwrap().clone();
    assert!(
        reqs.iter()
            .all(|r| r.auth.as_deref() == Some(&format!("Bearer {TEST_TOKEN}"))),
        "missing auth header: {reqs:?}"
    );
    assert!(
        reqs.iter().any(|r| r.method == "POST"
            && r.path == "/repos/acme/demo/pulls"
            && r.body.contains("Add M5 plugins")),
        "missing PR POST: {reqs:?}"
    );
    assert!(
        !reqs.iter().any(|r| r.path.contains("/comments")),
        "denied comment reached fake-github: {reqs:?}"
    );

    // Token must never appear in any client RPC message, event, or core log line.
    for m in &core.all_messages {
        assert!(
            !m.contains(TEST_TOKEN),
            "token leaked into RPC message: {m}"
        );
    }
    let logs = core.stderr_log.lock().unwrap().clone();
    assert!(!logs.contains(TEST_TOKEN), "token leaked into core log");
}
