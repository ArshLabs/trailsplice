use serde::{Deserialize, Serialize};
use std::{error::Error, fs::File, io::Read, path::Path};

pub mod capture;

const MAX_BYTES: usize = 64 * 1024;
const MAX_OBSERVATIONS: usize = 128;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Evidence {
    schema_version: u32,
    source: String,
    provenance: String,
    transaction_hash: String,
    observations: Vec<Observation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    capture: Option<capture::Capture>,
}

#[derive(Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Observation {
    Receipt {
        receipt: Receipt,
    },
    ReceiptAbsent,
    RequestFailed {
        request: usize,
    },
    WaitTimedOut,
    #[serde(rename = "observation_gap")]
    Gap {
        reason: String,
    },
}

#[derive(Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct Receipt {
    transaction_hash: String,
    block_hash: String,
    block_number: String,
    status: String,
}

pub fn read_report(path: &Path) -> Result<String, Box<dyn Error>> {
    let mut bytes = Vec::new();
    File::open(path)?
        .take((MAX_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    report_from_bytes(&bytes)
}

pub fn report_from_bytes(bytes: &[u8]) -> Result<String, Box<dyn Error>> {
    if bytes.len() > MAX_BYTES {
        return Err("evidence exceeds 64 KiB".into());
    }
    let evidence: Evidence = serde_json::from_slice(bytes)?;
    validate(&evidence)?;
    Ok(report(&evidence))
}

fn validate(evidence: &Evidence) -> Result<(), String> {
    if !matches!(evidence.schema_version, 1 | 2) {
        return Err("unsupported schema version; expected 1 or 2".into());
    }
    if (evidence.schema_version == 1 && evidence.provenance != "constructed")
        || (evidence.schema_version == 2 && evidence.provenance != "captured_local")
    {
        return Err("provenance does not match the schema version".into());
    }
    capture::validate(evidence)?;
    if evidence.source.is_empty()
        || evidence.source.len() > 64
        || !evidence
            .source
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
    {
        return Err("source must be a label of 1 to 64 letters, digits, '-', '_' or '.'".into());
    }
    validate_hash(&evidence.transaction_hash)?;
    if evidence.observations.len() > MAX_OBSERVATIONS {
        return Err("evidence exceeds 128 observations".into());
    }
    for (index, observation) in evidence.observations.iter().enumerate() {
        let result = match observation {
            Observation::Receipt { receipt } => {
                validate_receipt(receipt, &evidence.transaction_hash)
            }
            Observation::Gap { reason } => {
                if reason.is_empty()
                    || reason.len() > 256
                    || !reason.bytes().all(|b| b.is_ascii_graphic() || b == b' ')
                {
                    Err("gap reason must contain 1 to 256 printable ASCII bytes".into())
                } else {
                    Ok(())
                }
            }
            Observation::ReceiptAbsent
            | Observation::RequestFailed { .. }
            | Observation::WaitTimedOut => Ok(()),
        };
        result.map_err(|error| format!("observation #{}: {error}", index + 1))?;
    }
    Ok(())
}

fn validate_hash(hash: &str) -> Result<(), String> {
    let digits = hash.strip_prefix("0x").ok_or("hash must start with 0x")?;
    if digits.len() != 64
        || !digits
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err("hash must contain exactly 64 lowercase hexadecimal digits".into());
    }
    Ok(())
}

fn block_number(quantity: &str) -> Result<u64, String> {
    let digits = quantity
        .strip_prefix("0x")
        .ok_or("block number must start with 0x")?;
    if digits.is_empty()
        || (digits.len() > 1 && digits.starts_with('0'))
        || !digits
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err("block number must be a canonical hexadecimal quantity".into());
    }
    u64::from_str_radix(digits, 16).map_err(|_| "block number exceeds u64".into())
}

fn validate_receipt(receipt: &Receipt, transaction_hash: &str) -> Result<(), String> {
    validate_hash(&receipt.transaction_hash)?;
    validate_hash(&receipt.block_hash)?;
    block_number(&receipt.block_number)?;
    if receipt.transaction_hash != transaction_hash {
        return Err("receipt belongs to a different transaction".into());
    }
    if !matches!(receipt.status.as_str(), "0x0" | "0x1") {
        return Err("receipt status must be 0x0 or 0x1".into());
    }
    Ok(())
}

fn report(evidence: &Evidence) -> String {
    let mut output = format!(
        "Transaction: {}\nSource: {}\nEvidence: {}\n",
        evidence.transaction_hash,
        evidence.source,
        if evidence.schema_version == 1 {
            "constructed example"
        } else {
            "local capture (not authenticated)"
        }
    );
    let receipts: Vec<_> = evidence
        .observations
        .iter()
        .enumerate()
        .filter_map(|(index, observation)| match observation {
            Observation::Receipt { receipt } => Some((index + 1, receipt)),
            _ => None,
        })
        .collect();
    if let Some((_, first)) = receipts.first() {
        if receipts.iter().any(|(_, receipt)| receipt != first) {
            output.push_str("Saved receipts disagree about the block or execution result.\n");
        } else {
            output.push_str("Saved receipts report inclusion according to this source.\n");
        }
    } else {
        output.push_str("No saved receipt establishes inclusion.\n");
    }
    if let Some(capture) = &evidence.capture {
        output.push_str(&capture.summary());
    }
    for (index, observation) in evidence.observations.iter().enumerate() {
        let id = index + 1;
        match observation {
            Observation::Receipt { receipt } => {
                let execution = if receipt.status == "0x1" {
                    "successful"
                } else {
                    "failed"
                };
                let number = block_number(&receipt.block_number).expect("validated block number");
                output.push_str(&format!(
                    "#{id}: receipt reports block {number} ({}); execution {execution}.\n",
                    receipt.block_hash
                ));
            }
            Observation::ReceiptAbsent => {
                output.push_str(&format!("#{id}: source returned no receipt.\n"));
            }
            Observation::RequestFailed { request } => {
                let (method, error) = evidence
                    .capture
                    .as_ref()
                    .expect("validated capture")
                    .failure(*request);
                output.push_str(&format!("#{id}: {method} request failed: {error}\n"));
            }
            Observation::WaitTimedOut => {
                output.push_str(&format!("#{id}: observer's wait timed out.\n"));
            }
            Observation::Gap { reason } => {
                output.push_str(&format!("#{id}: observation gap recorded: {reason}\n"));
                if let Some((receipt_id, _)) =
                    receipts.iter().find(|(receipt_id, _)| *receipt_id > id)
                {
                    output.push_str(&format!(
                        "The next saved receipt is #{receipt_id}; inclusion timing relative to the gap is unknown.\n"
                    ));
                }
            }
        }
    }
    output.push_str(
        "This replay does not check the current chain or whether the block is finalized.\n\
         Absence or timeout does not establish dropped, failed or reorged status.\n",
    );
    output
}
