use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::error::{GatewayError, Result};

#[derive(Clone, Debug)]
pub struct HfSidecarClient {
    command_template: String,
    timeout: Duration,
}

#[derive(Debug)]
pub struct HfSidecarResult {
    pub payload: Value,
    pub changed_items: Option<usize>,
}

#[derive(Debug, Serialize)]
struct HfSidecarRequest<'a> {
    model_id: &'a str,
    payload: &'a Value,
}

#[derive(Debug, Deserialize)]
struct HfSidecarResponse {
    payload: Value,
    #[serde(default)]
    changed_items: Option<usize>,
}

impl HfSidecarClient {
    pub fn new(command_template: String, timeout: Duration) -> Self {
        Self {
            command_template,
            timeout,
        }
    }

    pub async fn preprocess(&self, model_id: &str, payload: &Value) -> Result<HfSidecarResult> {
        let sidecar_cmd = self
            .command_template
            .replace("{model_id}", model_id)
            .replace("{model}", model_id);
        let model_id = model_id.to_string();
        let payload = payload.clone();
        let task = tokio::task::spawn_blocking(move || {
            run_sidecar_once(&sidecar_cmd, &model_id, &payload)
        });
        tokio::time::timeout(self.timeout, task)
            .await
            .map_err(|_| GatewayError::Internal("hf sidecar timed out".to_string()))?
            .map_err(|e| GatewayError::Internal(format!("hf sidecar task join failed: {e}")))?
    }
}

fn run_sidecar_once(sidecar_cmd: &str, model_id: &str, payload: &Value) -> Result<HfSidecarResult> {
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(sidecar_cmd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| GatewayError::Internal(format!("failed to start hf sidecar: {e}")))?;

    let req = HfSidecarRequest { model_id, payload };
    let req_line = serde_json::to_string(&req)
        .map_err(|e| GatewayError::Internal(format!("serialize hf sidecar request failed: {e}")))?;

    if let Some(stdin) = child.stdin.as_mut() {
        stdin
            .write_all(req_line.as_bytes())
            .and_then(|_| stdin.write_all(b"\n"))
            .map_err(|e| GatewayError::Internal(format!("write hf sidecar stdin failed: {e}")))?;
    } else {
        return Err(GatewayError::Internal(
            "hf sidecar stdin not available".to_string(),
        ));
    }

    let mut output_line = String::new();
    if let Some(stdout) = child.stdout.take() {
        let mut reader = BufReader::new(stdout);
        reader
            .read_line(&mut output_line)
            .map_err(|e| GatewayError::Internal(format!("read hf sidecar stdout failed: {e}")))?;
    }

    let out = child
        .wait_with_output()
        .map_err(|e| GatewayError::Internal(format!("wait hf sidecar failed: {e}")))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(GatewayError::Internal(format!(
            "hf sidecar exited with non-zero status: {}",
            stderr.trim()
        )));
    }

    if output_line.trim().is_empty() {
        return Err(GatewayError::Internal(
            "hf sidecar returned empty output".to_string(),
        ));
    }
    let parsed: HfSidecarResponse = serde_json::from_str(output_line.trim()).map_err(|e| {
        GatewayError::Internal(format!(
            "parse hf sidecar output failed: {e}, raw={output_line}"
        ))
    })?;
    Ok(HfSidecarResult {
        payload: parsed.payload,
        changed_items: parsed.changed_items,
    })
}
