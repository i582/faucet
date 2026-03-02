use apalis::layers::WorkerBuilderExt;
use apalis::prelude::json::JsonCodec;
use apalis::prelude::{Data, TaskSink, WorkerBuilder};
use apalis_sqlite::fetcher::SqliteFetcher;
use apalis_sqlite::{CompactType, SqliteStorage};
use axum::extract::State;
use axum::{
    Json, Router,
    http::StatusCode,
    routing::{get, post},
};
use axum_governor::GovernorLayer;
use client::ToncenterClient;
use config::Config;
use lazy_limit::{Duration, RuleConfig, init_rate_limiter};
use moka::sync::Cache;
use num_bigint::BigUint;
use rand::RngCore;
use real::RealIpLayer;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration as StdDuration;
use tonlib_core::TonAddress;
use tonlib_core::cell::{Cell, CellBuilder};
use tonlib_core::tlb_types::block::coins::{CurrencyCollection, Grams};
use tonlib_core::tlb_types::block::message::{CommonMsgInfo, IntMsgInfo, Message};
use tonlib_core::tlb_types::primitives::either::EitherRef;
use tonlib_core::tlb_types::tlb::TLB;
use tower::ServiceBuilder;
use tracing::{error, info, warn};
use wallet::Wallet;

mod client;
mod config;
mod wallet;

#[tokio::main]
async fn main() {
    let config = Config::from_env().expect("Failed to load config");
    tracing_subscriber::fmt::init();

    let port = config.port;

    let opts = SqliteConnectOptions::from_str(&config.database_url)
        .expect("Invalid database URL")
        .create_if_missing(true);

    let pool = SqlitePoolOptions::new()
        .max_connections(5)
        .connect_with(opts)
        .await
        .expect("Failed to connect to database");

    init_rate_limiter!(
        default: RuleConfig::new(Duration::seconds(1), 5),
        max_memory: Some(64 * 1024 * 1024),
        routes: [
            ("/claim", RuleConfig::new(Duration::hours(24), 2)), // 2 req/24h
        ]
    )
    .await;

    SqliteStorage::setup(&pool)
        .await
        .expect("Failed to setup storage");
    let storage = SqliteStorage::new(&pool);

    let wallet = Wallet::new(&config.mnemonic).expect("Failed to create faucet wallet");
    let client =
        Arc::new(ToncenterClient::new(&config).expect("Failed to create Toncenter client"));

    let shared_state = AppState {
        storage: storage.clone(),
        wallet: Arc::new(wallet),
        client: client.clone(),
        config: Arc::new(config),
        pow_cache: Cache::builder()
            .time_to_live(StdDuration::from_secs(300)) // 5 minutes
            .max_capacity(10000)
            .build(),
    };

    let worker_state = shared_state.clone();

    let worker = WorkerBuilder::new("claim-worker")
        .backend(storage)
        .concurrency(1)
        .data(worker_state)
        .build(send_claim);

    tokio::spawn(async move {
        info!("Starting claim worker");
        worker.run().await.expect("Worker failed");
    });

    let app = Router::new()
        .route("/", get(root))
        .route("/challenge", get(get_challenge))
        .route("/claim", post(create_claim))
        .layer(
            ServiceBuilder::new()
                .layer(RealIpLayer::default())
                .layer(GovernorLayer::default()),
        )
        .with_state(shared_state);

    info!("Listening on 0.0.0.0:{}", port);
    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{}", port))
        .await
        .unwrap();
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await
    .unwrap();
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install signal handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }

    info!("Shutting down gracefully...");
}

#[derive(Clone)]
struct AppState {
    storage: SqliteStorage<CreateClaim, JsonCodec<CompactType>, SqliteFetcher>,
    wallet: Arc<Wallet>,
    client: Arc<ToncenterClient>,
    config: Arc<Config>,
    pow_cache: Cache<String, ()>,
}

async fn send_claim(task: CreateClaim, state: Data<AppState>) -> anyhow::Result<()> {
    let wallet = state.wallet.as_ref();
    let client = state.client.as_ref();

    info!("Processing claim for address: {}", task.address);

    let max_retries = state.config.worker_max_retries;

    for attempt in 0..=max_retries {
        match process_send_tokens(wallet, client, &task.address, state.config.faucet_amount).await {
            Ok(_) => {
                info!("Successfully sent claim to {}", task.address);
                return Ok(());
            }
            Err(err) => {
                if attempt < max_retries {
                    let delay =
                        exponential_backoff(state.config.worker_retry_base_delay_ms, attempt);
                    warn!(
                        address = %task.address,
                        attempt = attempt + 1,
                        max_attempts = max_retries + 1,
                        retry_in_ms = delay.as_millis(),
                        error = %err,
                        "Claim send attempt failed, retrying"
                    );
                    tokio::time::sleep(delay).await;
                    continue;
                }

                error!(
                    address = %task.address,
                    attempts = max_retries + 1,
                    error = %err,
                    "Failed to send claim after retries"
                );
                anyhow::bail!("Failed to send claim: {}", err);
            }
        }
    }

    unreachable!("send_claim loop should always return");
}

fn exponential_backoff(base_delay_ms: u64, attempt: u32) -> StdDuration {
    let multiplier = 1u64 << attempt.min(8);
    StdDuration::from_millis(base_delay_ms.saturating_mul(multiplier))
}

async fn process_send_tokens(
    wallet: &Wallet,
    client: &ToncenterClient,
    dest: &str,
    amount: u64,
) -> anyhow::Result<()> {
    let dest = TonAddress::from_str(dest)?;

    let message_cell = build_message(wallet, amount, dest)?;

    let seqno = client
        .get_wallet_seqno(&wallet.wallet.address.to_base64_url())
        .await?;

    let expire_at = (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs()
        + 600) as u32;

    let external =
        wallet
            .wallet
            .create_external_msg(expire_at, seqno, false, vec![message_cell.to_arc()])?;

    let response = client.send_boc(&external.to_boc_b64(false)?).await?;

    if let Some(ok) = response.get("ok").and_then(|v| v.as_bool())
        && !ok
    {
        anyhow::bail!("Toncenter returned ok: false. Response: {:?}", response);
    }

    Ok(())
}

fn build_message(wallet: &Wallet, amount: u64, dest: TonAddress) -> anyhow::Result<Cell> {
    let message_info = IntMsgInfo {
        ihr_disabled: true,
        bounce: false,
        bounced: false,
        src: wallet.wallet.address.to_msg_address(),
        dest: dest.to_msg_address(),
        value: CurrencyCollection::new(BigUint::from(amount)),
        ihr_fee: Grams::new(BigUint::from(0u64)),
        fwd_fee: Grams::new(BigUint::from(0u64)),
        created_at: 0,
        created_lt: 0,
    };

    let mut message_body_builder = CellBuilder::new();
    message_body_builder.store_u32(32, 0)?;
    message_body_builder.store_string("Testnet faucet")?;
    let message_body = message_body_builder.build()?;

    let message = Message {
        info: CommonMsgInfo::Int(message_info),
        init: None, // Some(EitherRef::new(state_init)),
        body: EitherRef::new(message_body.to_arc()),
    };

    let message_cell = message.to_cell()?;
    Ok(message_cell)
}

async fn root() -> &'static str {
    "TON Faucet is running!"
}

async fn get_challenge(State(state): State<AppState>) -> Json<Value> {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    let challenge = hex::encode(bytes);

    state.pow_cache.insert(challenge.clone(), ());

    Json(json!({
        "challenge": challenge,
        "difficulty": state.config.pow_difficulty
    }))
}

fn verify_pow(challenge: &str, nonce: u64, difficulty: u32) -> bool {
    let mut hasher = Sha256::new();
    hasher.update(challenge.as_bytes());
    hasher.update(nonce.to_be_bytes());
    let result = hasher.finalize();

    let mut zero_bits = 0;
    for &byte in result.iter() {
        let leading_zeros = byte.leading_zeros();
        zero_bits += leading_zeros;
        if leading_zeros < 8 {
            break;
        }
    }
    zero_bits >= difficulty
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct CreateClaim {
    address: String,
    challenge: String,
    nonce: u64,
}

//noinspection RsLiveness
#[axum::debug_handler]
async fn create_claim(
    State(mut state): State<AppState>,
    Json(payload): Json<CreateClaim>,
) -> Result<(StatusCode, Json<Value>), (StatusCode, Json<Value>)> {
    if TonAddress::from_str(&payload.address).is_err() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "Invalid TON address" })),
        ));
    }

    // Verify PoW
    if state.pow_cache.get(&payload.challenge).is_none() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "Invalid or expired challenge" })),
        ));
    }

    if !verify_pow(
        &payload.challenge,
        payload.nonce,
        state.config.pow_difficulty,
    ) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "Invalid PoW solution" })),
        ));
    }

    // Remove challenge to prevent reuse
    state.pow_cache.invalidate(&payload.challenge);

    state.storage.push(payload).await.map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": "Failed to queue claim" })),
        )
    })?;

    Ok((
        StatusCode::OK,
        Json(json!({ "message": "Your claim has been queued. It will be processed soon." })),
    ))
}
