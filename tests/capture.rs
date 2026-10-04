use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::PathBuf,
    process::Command,
    sync::atomic::{AtomicUsize, Ordering},
    thread,
    time::{Duration, Instant},
};
use trailsplice::read_report;

const HASH: &str = "0x1111111111111111111111111111111111111111111111111111111111111111";
static NEXT: AtomicUsize = AtomicUsize::new(0);

struct Output(PathBuf);

impl Output {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "trailsplice-capture-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path.join("evidence.json"))
    }
}

impl Drop for Output {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
        let _ = std::fs::remove_dir(self.0.parent().unwrap());
    }
}

fn identities() -> Vec<String> {
    [
        json!("anvil/test"),
        json!("0x7a69"),
        json!({"number":"0x0", "hash":HASH}),
    ]
    .into_iter()
    .map(rpc)
    .collect()
}

fn rpc(value: Value) -> String {
    json!({"jsonrpc":"2.0", "id":1, "result":value}).to_string()
}

fn read_request(stream: &mut TcpStream) -> Value {
    stream.set_nonblocking(false).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let mut bytes = Vec::new();
    let end = loop {
        let mut byte = [0];
        stream.read_exact(&mut byte).unwrap();
        bytes.push(byte[0]);
        assert!(bytes.len() <= 8192);
        if bytes.ends_with(b"\r\n\r\n") {
            break bytes.len();
        }
    };
    let headers = String::from_utf8_lossy(&bytes);
    let length: usize = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse().unwrap())
        })
        .unwrap();
    assert!(length < 4096);
    bytes.resize(end + length, 0);
    stream.read_exact(&mut bytes[end..]).unwrap();
    let request: Value = serde_json::from_slice(&bytes[end..]).unwrap();
    assert_eq!(request["jsonrpc"], "2.0");
    assert_eq!(request["id"], 1);
    request
}

fn server(bodies: Vec<String>, chunked: bool) -> (String, thread::JoinHandle<Vec<Value>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    listener.set_nonblocking(true).unwrap();
    let handle = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut requests = Vec::new();
        for body in bodies {
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "test server deadline");
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("{error}"),
                }
            };
            requests.push(read_request(&mut stream));
            let response = if chunked {
                format!(
                    "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:x}\r\n{body}\r\n0\r\n\r\n",
                    body.len()
                )
            } else {
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
            };
            let _ = stream.write_all(response.as_bytes());
        }
        requests
    });
    (url, handle)
}

fn observe(url: &str, output: &Output, seconds: &str) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_trailsplice"))
        .args([
            "observe",
            url,
            HASH,
            output.0.to_str().unwrap(),
            "--wait-seconds",
            seconds,
        ])
        .output()
        .unwrap()
}

#[test]
fn capture_saves_original_responses_and_replays_without_the_server() {
    let receipt = json!({"transactionHash":HASH, "blockHash":HASH, "blockNumber":"0x2a", "status":"0x1", "extra":"retained"});
    let raw_receipt = format!(" {{ \"jsonrpc\": \"2.0\", \"id\": 1, \"result\": {receipt} }} ");
    let mut responses = identities();
    responses.extend([rpc(Value::Null), raw_receipt.clone()]);
    let (url, server) = server(responses, false);
    let output = Output::new();
    let process = observe(&url, &output, "3");
    assert!(
        process.status.success(),
        "{}",
        String::from_utf8_lossy(&process.stderr)
    );
    let requests = server.join().unwrap();
    assert_eq!(requests[2]["params"], json!(["0x0", false]));
    assert_eq!(requests[3]["params"], json!([HASH]));
    assert_eq!(requests[4]["method"], "eth_getTransactionReceipt");
    let saved = std::fs::read(&output.0).unwrap();
    let value: Value = serde_json::from_slice(&saved).unwrap();
    assert_eq!(value["schema_version"], 2);
    assert_eq!(value["capture"]["requests"][4]["response"], raw_receipt);
    assert_eq!(value["observations"][0]["kind"], "receipt_absent");
    assert_eq!(value["observations"][1]["kind"], "receipt");
    let replay = read_report(&output.0).unwrap();
    assert_eq!(replay.as_bytes(), process.stdout);
    assert!(replay.contains("local capture (not authenticated)"));
    assert!(replay.contains("execution successful"));
    assert!(replay.contains("Chain ID: 0x7a69"));
    let mut reordered: Value = serde_json::from_slice(&saved).unwrap();
    reordered["capture"]["requests"][4]["completed_elapsed_ms"] = json!(0);
    assert!(trailsplice::report_from_bytes(&serde_json::to_vec(&reordered).unwrap()).is_err());
    let retry = observe(&url, &output, "1");
    assert!(!retry.status.success());
    assert_eq!(std::fs::read(&output.0).unwrap(), saved);
}

#[test]
fn null_receipt_and_rpc_errors_have_distinct_deadline_evidence() {
    for (response, expected) in [
        (rpc(Value::Null), "receipt_absent"),
        (
            json!({"jsonrpc":"2.0", "id":1, "error":{"code":-32000, "message":"unavailable"}})
                .to_string(),
            "request_failed",
        ),
    ] {
        let mut responses = identities();
        responses.push(response);
        let (url, server) = server(responses, false);
        let output = Output::new();
        let process = observe(&url, &output, "1");
        assert!(
            process.status.success(),
            "{}",
            String::from_utf8_lossy(&process.stderr)
        );
        server.join().unwrap();
        let value: Value = serde_json::from_slice(&std::fs::read(&output.0).unwrap()).unwrap();
        assert_eq!(value["observations"][0]["kind"], expected);
        assert_eq!(value["observations"][1]["kind"], "wait_timed_out");
        assert!(
            read_report(&output.0)
                .unwrap()
                .contains("No saved receipt establishes inclusion.")
        );
    }
}

#[test]
fn invalid_and_oversized_responses_are_saved_as_failures() {
    for (body, chunked) in [
        ("{".into(), false),
        (r#"{"jsonrpc":"2.0","id":1}"#.into(), false),
        (
            r#"{"jsonrpc":"2.0","id":1,"result":null,"error":null}"#.into(),
            false,
        ),
        (
            json!({"jsonrpc":"2.0", "id":2, "result":null}).to_string(),
            false,
        ),
        (
            rpc(
                json!({"transactionHash":"0x2222222222222222222222222222222222222222222222222222222222222222", "blockHash":HASH, "blockNumber":"0x2a", "status":"0x1"}),
            ),
            false,
        ),
        (" ".repeat(16385), false),
        (" ".repeat(16385), true),
    ] {
        let mut responses = identities();
        responses.push(body);
        let (url, server) = server(responses, chunked);
        let output = Output::new();
        let process = observe(&url, &output, "3");
        assert!(
            process.status.success(),
            "{}",
            String::from_utf8_lossy(&process.stderr)
        );
        server.join().unwrap();
        let value: Value = serde_json::from_slice(&std::fs::read(&output.0).unwrap()).unwrap();
        assert_eq!(
            value["observations"][0],
            json!({"kind":"request_failed", "request":4})
        );
        assert_eq!(
            value["observations"].as_array().unwrap().last().unwrap()["kind"],
            "wait_timed_out"
        );
    }
}

#[test]
fn unreachable_endpoint_records_failure_and_output_errors_do_not_report_success() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    let output = Output::new();
    let process = observe(&url, &output, "1");
    assert!(
        process.status.success(),
        "{}",
        String::from_utf8_lossy(&process.stderr)
    );
    let report = read_report(&output.0).unwrap();
    assert!(report.contains("request failed:"));
    assert!(!report.contains("source returned no receipt"));
    assert!(report.contains("Chain ID: unknown"));
    let missing = Output::new();
    std::fs::remove_dir(missing.0.parent().unwrap()).unwrap();
    let failed = observe(&url, &missing, "1");
    assert!(!failed.status.success());
    assert!(failed.stdout.is_empty());
    assert!(!missing.0.exists());
}

#[test]
fn invalid_configuration_is_rejected_before_writing() {
    for url in [
        "https://127.0.0.1:8545",
        "http://example.com",
        "http://192.0.2.1",
        "http://user:password@127.0.0.1",
        "http://127.0.0.1/path",
        "http://127.0.0.1?token=secret",
    ] {
        let output = Output::new();
        assert!(!observe(url, &output, "1").status.success());
        assert!(!output.0.exists());
    }
    for seconds in ["0", "31", "invalid"] {
        let output = Output::new();
        assert!(
            !observe("http://127.0.0.1:8545", &output, seconds)
                .status
                .success()
        );
        assert!(!output.0.exists());
    }
}

#[test]
fn slow_response_is_cancelled_at_the_run_deadline() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    listener.set_nonblocking(true).unwrap();
    let server = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline);
                    thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("{error}"),
            }
        };
        read_request(&mut stream);
        thread::sleep(Duration::from_secs(2));
    });
    let output = Output::new();
    let process = observe(&url, &output, "1");
    assert!(
        process.status.success(),
        "{}",
        String::from_utf8_lossy(&process.stderr)
    );
    server.join().unwrap();
    let report = read_report(&output.0).unwrap();
    assert!(report.contains("request deadline reached"));
    assert!(!report.contains("source returned no receipt"));
    assert!(report.contains("observer's wait timed out"));
    let evidence: Value = serde_json::from_slice(&std::fs::read(&output.0).unwrap()).unwrap();
    assert_eq!(evidence["capture"]["requests"].as_array().unwrap().len(), 1);
    assert_eq!(
        evidence["capture"]["requests"][0]["method"],
        "web3_clientVersion"
    );
}

#[test]
fn a_failed_poll_can_be_followed_by_a_receipt_for_failed_execution() {
    let mut responses = identities();
    responses.push(json!({"jsonrpc":"2.0", "id":1, "error":{"code":-32000,"message":"temporarily unavailable"}}).to_string());
    responses.push(rpc(
        json!({"transactionHash":HASH,"blockHash":HASH,"blockNumber":"0x2a","status":"0x0"}),
    ));
    let (url, server) = server(responses, false);
    let output = Output::new();
    let process = observe(&url, &output, "3");
    assert!(
        process.status.success(),
        "{}",
        String::from_utf8_lossy(&process.stderr)
    );
    server.join().unwrap();
    let evidence: Value = serde_json::from_slice(&std::fs::read(&output.0).unwrap()).unwrap();
    assert_eq!(
        evidence["observations"][0],
        json!({"kind":"request_failed","request":4})
    );
    assert_eq!(evidence["observations"][1]["kind"], "receipt");
    assert_eq!(evidence["observations"].as_array().unwrap().len(), 2);
    let report = read_report(&output.0).unwrap();
    assert!(report.contains("Saved receipts report inclusion according to this source."));
    assert!(report.contains("execution failed"));
    assert!(!report.contains("observer's wait timed out"));
}

#[test]
fn total_evidence_size_is_checked_before_output_creation() {
    let mut responses = identities();
    responses.push(rpc(
        json!({"transactionHash":HASH, "blockHash":HASH, "blockNumber":"0x2a", "status":"0x1"}),
    ));
    for body in &mut responses {
        body.push_str(&" ".repeat(16384 - body.len()));
    }
    let (url, server) = server(responses, false);
    let output = Output::new();
    let process = observe(&url, &output, "3");
    server.join().unwrap();
    assert!(!process.status.success());
    assert!(
        String::from_utf8_lossy(&process.stderr).contains("capture exceeds the evidence limits")
    );
    assert!(process.stdout.is_empty());
    assert!(!output.0.exists());
}
