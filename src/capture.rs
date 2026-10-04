use crate::{
    Evidence, MAX_BYTES, MAX_OBSERVATIONS, Observation, Receipt, block_number, validate_hash,
    validate_receipt,
};
use reqwest::{Client, Url};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Value, json, value::RawValue};
use std::{
    error::Error,
    fs::OpenOptions,
    io::Write,
    net::IpAddr,
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::time::{Instant, sleep_until, timeout_at};

const MAX_RESPONSE_BYTES: usize = 16 * 1024;
const IDENTITY_METHODS: [&str; 3] = ["web3_clientVersion", "eth_chainId", "eth_getBlockByNumber"];

#[derive(Deserialize)]
struct RpcResponse {
    jsonrpc: String,
    id: u64,
    #[serde(default)]
    result: RpcResult,
    #[serde(default)]
    error: RpcResult,
}

#[derive(Default)]
enum RpcResult {
    #[default]
    Missing,
    Present(Box<RawValue>),
}

impl<'de> Deserialize<'de> for RpcResult {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Box::<RawValue>::deserialize(deserializer).map(Self::Present)
    }
}

#[derive(Deserialize)]
struct RpcError {
    code: i64,
    message: String,
}

#[derive(Deserialize)]
struct Genesis {
    number: String,
    hash: String,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Capture {
    endpoint: String,
    run_id: String,
    started_unix_ms: u64,
    run_budget_ms: u64,
    finished_elapsed_ms: u64,
    requests: Vec<RequestRecord>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RequestRecord {
    method: String,
    started_elapsed_ms: u64,
    completed_elapsed_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    response: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

fn endpoint(value: &str) -> Result<Url, Box<dyn Error>> {
    let url = Url::parse(value)?;
    let ip: IpAddr = url
        .host_str()
        .ok_or("endpoint needs an IP address")?
        .trim_matches(['[', ']'])
        .parse()?;
    if url.scheme() != "http"
        || !ip.is_loopback()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
        || url.port_or_known_default() == Some(0)
    {
        return Err(
            "use a credential-free HTTP loopback endpoint, such as http://127.0.0.1:8545".into(),
        );
    }
    Ok(url)
}

fn result(body: &str, method: &str, transaction: &str) -> Result<Value, String> {
    let response: RpcResponse =
        serde_json::from_str(body).map_err(|_| "invalid RPC response envelope")?;
    if response.jsonrpc != "2.0" {
        return Err("response must use JSON-RPC 2.0".into());
    }
    if response.id != 1 {
        return Err("response ID does not match the request".into());
    }
    let raw = match (response.result, response.error) {
        (RpcResult::Present(value), RpcResult::Missing) => value,
        (RpcResult::Missing, RpcResult::Present(error)) => {
            let error: RpcError =
                serde_json::from_str(error.get()).map_err(|_| "invalid RPC error object")?;
            let message: String = error
                .message
                .chars()
                .take(128)
                .map(|c| {
                    if c.is_ascii_graphic() || c == ' ' {
                        c
                    } else {
                        '?'
                    }
                })
                .collect();
            return Err(format!("RPC error {}: {message}", error.code));
        }
        _ => return Err("response needs exactly one result or error".into()),
    };
    let value: Value = serde_json::from_str(raw.get()).map_err(|_| "invalid RPC result")?;
    match method {
        "web3_clientVersion" => {
            let text = value.as_str().ok_or("client version must be text")?;
            if text.is_empty()
                || text.len() > 128
                || !text.bytes().all(|b| b.is_ascii_graphic() || b == b' ')
            {
                return Err("invalid client version".into());
            }
        }
        "eth_chainId" => {
            block_number(value.as_str().ok_or("chain ID must be a quantity")?)?;
        }
        "eth_getBlockByNumber" => {
            let genesis: Genesis =
                serde_json::from_str(raw.get()).map_err(|_| "invalid genesis fields")?;
            if genesis.number != "0x0" {
                return Err("genesis response must identify block zero".into());
            }
            validate_hash(&genesis.hash)?;
        }
        "eth_getTransactionReceipt" if !value.is_null() => {
            let receipt: Receipt = serde_json::from_str(raw.get())
                .map_err(|_| "receipt fields are missing or invalid")?;
            validate_receipt(&receipt, transaction)?;
        }
        "eth_getTransactionReceipt" => {}
        _ => return Err("unsupported captured method".into()),
    }
    Ok(value)
}

fn observation(
    record: &RequestRecord,
    index: usize,
    transaction: &str,
) -> Result<Option<Observation>, String> {
    if let Some(error) = &record.error {
        if error.is_empty()
            || error.len() > 256
            || !error.bytes().all(|b| b.is_ascii_graphic() || b == b' ')
        {
            return Err("invalid captured error".into());
        }
        if let Some(body) = &record.response
            && result(body, &record.method, transaction).err().as_ref() != Some(error)
        {
            return Err("captured error does not match the RPC response".into());
        }
        return Ok(Some(Observation::RequestFailed { request: index + 1 }));
    }
    let body = record
        .response
        .as_deref()
        .ok_or("request needs a response or error")?;
    let value = result(body, &record.method, transaction)?;
    if record.method != "eth_getTransactionReceipt" {
        return Ok(None);
    }
    if value.is_null() {
        return Ok(Some(Observation::ReceiptAbsent));
    }
    let receipt = serde_json::from_value(value).map_err(|_| "invalid receipt")?;
    Ok(Some(Observation::Receipt { receipt }))
}

pub(super) fn validate(evidence: &Evidence) -> Result<(), String> {
    if evidence.schema_version == 1 {
        if evidence.capture.is_some()
            || evidence
                .observations
                .iter()
                .any(|obs| matches!(obs, Observation::RequestFailed { .. }))
        {
            return Err("schema 1 cannot contain capture metadata or request failures".into());
        }
        return Ok(());
    }
    let capture = evidence
        .capture
        .as_ref()
        .ok_or("schema 2 requires capture metadata")?;
    if endpoint(&capture.endpoint)
        .map_err(|_| "invalid captured endpoint")?
        .as_str()
        != capture.endpoint
        || capture.run_id.is_empty()
        || capture.run_id.len() > 64
        || !capture
            .run_id
            .bytes()
            .all(|b| b.is_ascii_digit() || b == b'-')
        || !(1000..=30000).contains(&capture.run_budget_ms)
        || capture.requests.len() > MAX_OBSERVATIONS - 1
    {
        return Err("invalid capture metadata or request count".into());
    }
    let mut expected = Vec::new();
    let mut previous = 0;
    let mut included = false;
    for (index, record) in capture.requests.iter().enumerate() {
        let method = IDENTITY_METHODS
            .get(index)
            .copied()
            .unwrap_or("eth_getTransactionReceipt");
        if record.method != method
            || record.started_elapsed_ms >= capture.run_budget_ms
            || record.started_elapsed_ms < previous
            || record.completed_elapsed_ms < record.started_elapsed_ms
            || record.completed_elapsed_ms > capture.finished_elapsed_ms
            || included
            || record
                .response
                .as_ref()
                .is_some_and(|body| body.len() > MAX_RESPONSE_BYTES)
        {
            return Err("invalid request ordering, method or response size".into());
        }
        previous = record.completed_elapsed_ms;
        if let Some(obs) = observation(record, index, &evidence.transaction_hash)? {
            included = matches!(obs, Observation::Receipt { .. });
            expected.push(obs);
        }
    }
    if !included {
        if capture.finished_elapsed_ms < capture.run_budget_ms {
            return Err("deadline observation precedes the run deadline".into());
        }
        expected.push(Observation::WaitTimedOut);
    }
    if expected != evidence.observations {
        return Err("observations do not match captured request outcomes".into());
    }
    Ok(())
}

impl Capture {
    pub(super) fn failure(&self, request: usize) -> (&str, &str) {
        let record = &self.requests[request - 1];
        (
            &record.method,
            record.error.as_deref().expect("validated failure"),
        )
    }

    pub(super) fn summary(&self) -> String {
        let mut text = format!(
            "Endpoint: {}\nRun: {} (started at Unix {} ms)\n",
            self.endpoint, self.run_id, self.started_unix_ms
        );
        for (index, label) in ["Client", "Chain ID", "Genesis"].into_iter().enumerate() {
            let value = self.requests.get(index).and_then(|record| {
                record
                    .response
                    .as_deref()
                    .filter(|_| record.error.is_none())
                    .and_then(|body| result(body, &record.method, "").ok())
            });
            let field = value.as_ref().map(|value| {
                if label == "Genesis" {
                    &value["hash"]
                } else {
                    value
                }
            });
            text.push_str(&format!(
                "{label}: {}\n",
                field.and_then(Value::as_str).unwrap_or("unknown")
            ));
        }
        text
    }
}

async fn request(
    client: &Client,
    url: &Url,
    method: &str,
    transaction: &str,
) -> Result<String, String> {
    let params = match method {
        "eth_getBlockByNumber" => json!(["0x0", false]),
        "eth_getTransactionReceipt" => json!([transaction]),
        _ => json!([]),
    };
    let payload = json!({"jsonrpc":"2.0", "id":1, "method":method, "params":params});
    let mut response = client
        .post(url.clone())
        .json(&payload)
        .send()
        .await
        .map_err(|_| "HTTP request failed")?;
    if !response.status().is_success() {
        return Err(format!("HTTP status {}", response.status().as_u16()));
    }
    if response
        .content_length()
        .is_some_and(|size| size > MAX_RESPONSE_BYTES as u64)
    {
        return Err("RPC response exceeds 16 KiB".into());
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| "HTTP response read failed")?
    {
        if chunk.len() > MAX_RESPONSE_BYTES - bytes.len() {
            return Err("RPC response exceeds 16 KiB".into());
        }
        bytes.extend_from_slice(&chunk);
    }
    String::from_utf8(bytes).map_err(|_| "RPC response is not UTF-8".into())
}

pub async fn observe(
    endpoint_text: &str,
    transaction: &str,
    destination: &Path,
    wait_seconds: u64,
) -> Result<String, Box<dyn Error>> {
    validate_hash(transaction)?;
    let url = endpoint(endpoint_text)?;
    if !(1..=30).contains(&wait_seconds) {
        return Err("wait must be 1 to 30 seconds".into());
    }
    if destination.try_exists()? {
        return Err("output file already exists".into());
    }
    let client = Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .build()?;
    let started_unix_ms = u64::try_from(SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis())?;
    let start = Instant::now();
    let deadline = start + Duration::from_secs(wait_seconds);
    let mut evidence = Evidence {
        schema_version: 2,
        source: "local-http".into(),
        provenance: "captured_local".into(),
        transaction_hash: transaction.into(),
        observations: Vec::new(),
        capture: Some(Capture {
            endpoint: url.to_string(),
            run_id: format!("{}-{started_unix_ms}", std::process::id()),
            started_unix_ms,
            run_budget_ms: wait_seconds * 1000,
            finished_elapsed_ms: 0,
            requests: Vec::new(),
        }),
    };
    let mut index = 0;
    loop {
        let request_start = Instant::now();
        if request_start >= deadline {
            evidence.observations.push(Observation::WaitTimedOut);
            break;
        }
        let method = IDENTITY_METHODS
            .get(index)
            .copied()
            .unwrap_or("eth_getTransactionReceipt");
        let started_elapsed_ms = request_start.duration_since(start).as_millis() as u64;
        let outcome = timeout_at(
            deadline.min(request_start + Duration::from_secs(2)),
            request(&client, &url, method, transaction),
        )
        .await;
        let (response, error) = match outcome {
            Ok(Ok(body)) => {
                let error = result(&body, method, transaction).err();
                (Some(body), error)
            }
            Ok(Err(error)) => (None, Some(error)),
            Err(_) => (None, Some("request deadline reached".into())),
        };
        let record = RequestRecord {
            method: method.into(),
            started_elapsed_ms,
            completed_elapsed_ms: start.elapsed().as_millis() as u64,
            response,
            error,
        };
        let obs = observation(&record, index, transaction)?;
        let included = matches!(obs, Some(Observation::Receipt { .. }));
        if matches!(obs, Some(Observation::ReceiptAbsent)) {
            eprintln!("Observed no receipt.");
        }
        if let Some(obs) = obs {
            evidence.observations.push(obs);
        }
        evidence
            .capture
            .as_mut()
            .expect("capture exists")
            .requests
            .push(record);
        if evidence.observations.len() >= MAX_OBSERVATIONS - 1
            || serde_json::to_vec(&evidence)?.len() > MAX_BYTES
        {
            return Err("capture exceeds the evidence limits; no file was written".into());
        }
        index += 1;
        if included {
            break;
        }
        if index > 3 {
            sleep_until(deadline.min(Instant::now() + Duration::from_secs(1))).await;
        }
    }
    evidence
        .capture
        .as_mut()
        .expect("capture exists")
        .finished_elapsed_ms = start.elapsed().as_millis() as u64;
    let bytes = serde_json::to_vec(&evidence)?;
    let report = crate::report_from_bytes(&bytes)?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)?;
    file.write_all(&bytes)?;
    file.flush()?;
    Ok(report)
}
