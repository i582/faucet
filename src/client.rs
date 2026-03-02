use crate::config::Config;
use anyhow::{Context, anyhow};
use serde::Deserialize;
use serde_json::{Value, json};
use std::time::Duration;
use tokio::time::sleep;
use tracing::warn;

pub struct ToncenterClient {
    client: reqwest::Client,
    base_url: String,
    api_key: Option<String>,
    max_retries: u32,
    retry_base_delay: Duration,
}

#[derive(Deserialize, Debug)]
pub struct GetMethodResult {
    pub stack: Vec<Value>,
}

impl ToncenterClient {
    pub fn new(config: &Config) -> anyhow::Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(config.toncenter_timeout_seconds))
            .connect_timeout(Duration::from_secs(
                config.toncenter_connect_timeout_seconds,
            ))
            .build()
            .context("Failed to build Toncenter HTTP client")?;

        Ok(Self {
            client,
            base_url: config.toncenter_url.clone(),
            api_key: config.toncenter_api_key.clone(),
            max_retries: config.toncenter_max_retries,
            retry_base_delay: Duration::from_millis(config.toncenter_retry_base_delay_ms),
        })
    }

    fn apply_auth(&self, mut request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        if let Some(ref key) = self.api_key {
            request = request.header("X-API-Key", key);
        }
        request
    }

    pub async fn get_wallet_seqno(&self, address: &str) -> anyhow::Result<u32> {
        let result = self.run_get_method(address, "seqno").await?;

        for first in result.stack {
            if let Some(val) = first.as_array() {
                if val.len() == 2 {
                    if let (Some(type_str), Some(value_str)) = (val[0].as_str(), val[1].as_str()) {
                        if type_str == "num" {
                            let seqno = u32::from_str_radix(value_str.trim_start_matches("0x"), 16)
                                .unwrap_or(0);
                            return Ok(seqno);
                        }
                    }
                }
            } else if let Some(type_str) = first.get("type").and_then(|t| t.as_str()) {
                if type_str == "num" {
                    if let Some(value_str) = first.get("value").and_then(|v| v.as_str()) {
                        let seqno = u32::from_str_radix(value_str.trim_start_matches("0x"), 16)
                            .unwrap_or(0);
                        return Ok(seqno);
                    }
                }
            }
        }

        Ok(0)
    }

    pub async fn run_get_method(
        &self,
        address: &str,
        method: &str,
    ) -> anyhow::Result<GetMethodResult> {
        let json = json!({
            "id": "1",
            "jsonrpc": "2.0",
            "method": "runGetMethod",
            "params": {
                "address": address,
                "method": method,
                "stack": []
            }
        });

        let response = self.post_jsonrpc_with_retry(&json, "runGetMethod").await?;
        let result = response.get("result").cloned().unwrap_or(response);

        serde_json::from_value(result).context("Failed to parse runGetMethod response payload")
    }

    pub async fn send_boc(&self, boc: &str) -> anyhow::Result<Value> {
        let json = json!({
            "id": "1",
            "jsonrpc": "2.0",
            "method": "sendBoc",
            "params": {
                "boc": boc
            }
        });

        self.post_jsonrpc_with_retry(&json, "sendBoc").await
    }

    fn jsonrpc_url(&self) -> String {
        format!("{}/api/v2/jsonRPC", self.base_url)
    }

    fn retry_delay(&self, attempt: u32) -> Duration {
        let multiplier = 1u64 << attempt.min(8);
        self.retry_base_delay.saturating_mul(multiplier as u32)
    }

    fn is_retryable_status(status: reqwest::StatusCode) -> bool {
        status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
    }

    fn is_retryable_error(error: &reqwest::Error) -> bool {
        error.is_timeout() || error.is_connect() || error.is_request()
    }

    fn is_retryable_rpc_error(error: &Value) -> bool {
        if let Some(code) = error.get("code").and_then(|v| v.as_i64())
            && (code == 429 || code >= 500)
        {
            return true;
        }

        if let Some(message) = error.get("message").and_then(|v| v.as_str()) {
            let msg = message.to_ascii_lowercase();
            return msg.contains("rate limit")
                || msg.contains("too many requests")
                || msg.contains("timeout")
                || msg.contains("temporar");
        }

        false
    }

    async fn post_jsonrpc_with_retry(
        &self,
        payload: &Value,
        operation: &str,
    ) -> anyhow::Result<Value> {
        let url = self.jsonrpc_url();

        for attempt in 0..=self.max_retries {
            let request = self.apply_auth(self.client.post(&url).json(payload));

            let response = match request.send().await {
                Ok(response) => response,
                Err(err) => {
                    if attempt < self.max_retries && Self::is_retryable_error(&err) {
                        warn!(
                            operation,
                            attempt = attempt + 1,
                            max_attempts = self.max_retries + 1,
                            error = %err,
                            "Toncenter request failed, retrying"
                        );
                        sleep(self.retry_delay(attempt)).await;
                        continue;
                    }

                    return Err(err).context(format!("Failed to send {} request", operation));
                }
            };

            let status = response.status();
            let body = response
                .text()
                .await
                .context(format!("Failed to read {} response body", operation))?;

            if !status.is_success() {
                if attempt < self.max_retries && Self::is_retryable_status(status) {
                    warn!(
                        operation,
                        attempt = attempt + 1,
                        max_attempts = self.max_retries + 1,
                        status = %status,
                        "Toncenter returned retryable HTTP status"
                    );
                    sleep(self.retry_delay(attempt)).await;
                    continue;
                }

                return Err(anyhow!(
                    "TonCenter API returned status: {} for {}. Error: {}",
                    status,
                    operation,
                    body
                ));
            }

            let response_json: Value = serde_json::from_str(&body)
                .context(format!("Failed to parse {} response as JSON", operation))?;

            if let Some(error) = response_json.get("error").filter(|e| !e.is_null()) {
                if attempt < self.max_retries && Self::is_retryable_rpc_error(error) {
                    warn!(
                        operation,
                        attempt = attempt + 1,
                        max_attempts = self.max_retries + 1,
                        error = %error,
                        "Toncenter returned retryable JSON-RPC error"
                    );
                    sleep(self.retry_delay(attempt)).await;
                    continue;
                }

                return Err(anyhow!(
                    "TonCenter JSON-RPC error for {}: {}",
                    operation,
                    error
                ));
            }

            return Ok(response_json);
        }

        Err(anyhow!(
            "Exceeded retry budget for Toncenter operation: {}",
            operation
        ))
    }
}
