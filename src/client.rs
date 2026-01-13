use crate::config::Config;
use anyhow::{Context, anyhow};
use serde::Deserialize;
use serde_json::{Value, json};

pub struct ToncenterClient {
    client: reqwest::Client,
    base_url: String,
    api_key: Option<String>,
}

#[derive(Deserialize, Debug)]
pub struct GetMethodResult {
    pub stack: Vec<Value>,
}

impl ToncenterClient {
    pub fn new(config: &Config) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: config.toncenter_url.clone(),
            api_key: config.toncenter_api_key.clone(),
        }
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
        let url = format!("{}/api/v2/jsonRPC", self.base_url);

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

        let request = self.client.post(&url).json(&json);
        let request = self.apply_auth(request);

        let response = request
            .send()
            .await
            .context("Failed to send runGetMethod request")?;

        if !response.status().is_success() {
            return Err(anyhow!(
                "TonCenter API returned status: {} for runGetMethod",
                response.status()
            ));
        }

        #[derive(Deserialize)]
        struct JsonRpcResponse {
            result: GetMethodResult,
        }

        let res: JsonRpcResponse = response
            .json()
            .await
            .context("Failed to parse runGetMethod response")?;

        Ok(res.result)
    }

    pub async fn send_boc(&self, boc: &str) -> anyhow::Result<Value> {
        let url = format!("{}/api/v2/jsonRPC", self.base_url);

        let json = json!({
            "id": "1",
            "jsonrpc": "2.0",
            "method": "sendBoc",
            "params": {
                "boc": boc
            }
        });

        let request = self.client.post(&url).json(&json);
        let request = self.apply_auth(request);

        let response = request
            .send()
            .await
            .context("Failed to send sendBoc request")?;

        if !response.status().is_success() {
            let status = response.status();
            let error_text = response.text().await.unwrap_or_default();
            return Err(anyhow!(
                "TonCenter API returned status: {} for sendBoc. Error: {}",
                status,
                error_text
            ));
        }

        let res: Value = response
            .json()
            .await
            .context("Failed to parse sendBoc response")?;

        Ok(res)
    }
}
