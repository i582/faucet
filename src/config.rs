use serde::Deserialize;

#[derive(Deserialize, Clone, Debug)]
pub struct Config {
    pub database_url: String,
    pub mnemonic: String,
    pub toncenter_api_key: Option<String>,
    pub toncenter_url: String,
    pub port: u16,
    pub faucet_amount: u64,
    pub pow_difficulty: u32,
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        dotenvy::dotenv().ok();

        Ok(Config {
            database_url: std::env::var("DATABASE_URL")
                .unwrap_or_else(|_| "sqlite:./db.sqlite".to_string()),
            mnemonic: std::env::var("MNEMONIC").expect("MNEMONIC must be set"),
            toncenter_api_key: std::env::var("TONCENTER_API_KEY").ok(),
            toncenter_url: std::env::var("TONCENTER_URL")
                .unwrap_or_else(|_| "https://testnet.toncenter.com".to_string()),
            port: std::env::var("PORT")
                .ok()
                .and_then(|p| p.parse().ok())
                .unwrap_or(3001),
            faucet_amount: std::env::var("FAUCET_AMOUNT")
                .ok()
                .and_then(|a| a.parse().ok())
                .unwrap_or(1_000_000), // 0.001 TON default
            pow_difficulty: std::env::var("POW_DIFFICULTY")
                .ok()
                .and_then(|d| d.parse().ok())
                .unwrap_or(21),
        })
    }
}
