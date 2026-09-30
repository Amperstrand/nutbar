//! mockgate — a scriptable mock TollGate gateway for the e2e suite.
//!
//!   GET  /               -> kind 10021 advertisement (pricing from env)
//!   GET  /whoami         -> "mac=DE:AD:BE:EF:00:42"
//!   GET  /usage          -> "used/allotment" in the active metric, "-1/-1" when
//!                           expired (or always, under the no-usage scenario)
//!   GET  /session-state?mac=… -> {"status":1,"mac":…,"state":"none|active|expired"}
//!                           (TollGate 0.6; "expired" latches after /usage
//!                           observes expiry, clears on the next payment)
//!   POST /               -> raw cashu token (text/plain) or kind-21000 event;
//!                           each token accepted once, replays get the spent notice,
//!                           allotment accumulates while a session is live
//!
//! Environment knobs (all optional):
//!   MOCKGATE_LISTEN            bind address            (127.0.0.1:2121)
//!   MOCKGATE_METRIC            milliseconds|bytes      (milliseconds)
//!   MOCKGATE_STEP_SIZE_MS      ms of access per sat    (60000)
//!   MOCKGATE_STEP_SIZE_BYTES   bytes of access per sat (100000)
//!   MOCKGATE_PRICE_PER_STEP    sats per step           (1)
//!   MOCKGATE_MINT_URL          accepted mint           (testnut)
//!   MOCKGATE_EXTRA_MINT_URL    advertise a decoy offer for ANOTHER mint
//!                              first, to exercise offer selection
//!   MOCKGATE_BYTES_PER_SEC     bytes-mode usage drain  (50000)
//!   MOCKGATE_SCENARIO          comma-separated failure injection:
//!       drop-first=N                   swallow the first N payments
//!                                       (empty 200 -> client keeps pending)
//!       outcome-unknown-once            one payment-outcome-unknown notice
//!       terminal-mint-not-accepted-once one terminal rejection notice
//!       no-usage                        /usage always reports no session
//!
//! `--phone` binds 0.0.0.0 for the phone-hotspot rehearsal. It logs the
//! sats it "received" but never redeems proofs at a mint.

use std::collections::HashSet;
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use axum::{routing::get, Json, Router};
use axum::response::IntoResponse as _;
use cdk::nuts::Token;
use serde_json::{json, Value};

const DEFAULT_LISTEN: &str = "127.0.0.1:2121";
const DEFAULT_STEP_SIZE_MS: u64 = 60_000;
const DEFAULT_STEP_SIZE_BYTES: u64 = 100_000;
const DEFAULT_BYTES_PER_SEC: u64 = 50_000;
const DEFAULT_MINT_URL: &str = "https://testnut.cashu.space";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Metric {
    Milliseconds,
    Bytes,
}

impl Metric {
    fn from_env() -> Self {
        match std::env::var("MOCKGATE_METRIC").as_deref() {
            Ok("bytes") => Metric::Bytes,
            _ => Metric::Milliseconds,
        }
    }

    fn tag(&self) -> &'static str {
        match self {
            Metric::Milliseconds => "milliseconds",
            Metric::Bytes => "bytes",
        }
    }
}

struct GatewayState {
    ad_pubkey: String,
    metric: Metric,
    step_size: u64,
    price_per_step: u64,
    mint_url: String,
    extra_mint_url: Option<String>,
    bytes_per_sec: u64,
    no_usage: bool,
    expired: std::sync::atomic::AtomicBool,
    drops_left: AtomicU64,
    outcome_unknown_left: AtomicU64,
    terminal_left: AtomicU64,
    spent: std::sync::Mutex<HashSet<String>>,
    session: std::sync::Mutex<Option<MockSession>>,
}

struct MockSession {
    started: Instant,
    allotment: u64,
    start_time: u64,
}

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|v| *v > 0)
        .unwrap_or(default)
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn rand_nonce() -> u128 {
    use rand::RngCore;
    rand::thread_rng().next_u32() as u128
}

fn parse_scenario(raw: Option<String>) -> (bool, u64, u64, u64) {
    let mut no_usage = false;
    let mut drops = 0;
    let mut outcome_unknown = 0;
    let mut terminal = 0;
    let Some(raw) = raw else {
        return (no_usage, drops, outcome_unknown, terminal);
    };
    for mode in raw.split(',').map(str::trim).filter(|m| !m.is_empty()) {
        match mode {
            "no-usage" => no_usage = true,
            "outcome-unknown-once" => outcome_unknown += 1,
            "terminal-mint-not-accepted-once" => terminal += 1,
            other if other.starts_with("drop-first=") => {
                drops = other
                    .split_once('=')
                    .and_then(|(_, n)| n.parse().ok())
                    .unwrap_or(0);
            }
            _ => {}
        }
    }
    (no_usage, drops, outcome_unknown, terminal)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let bind_all = std::env::args().any(|a| a == "--phone");
    let ad_pubkey = {
        use rand::RngCore;
        let mut pk = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut pk);
        pk.iter().map(|b| format!("{b:02x}")).collect::<String>()
    };
    let default_listen = if bind_all { "0.0.0.0:2121" } else { DEFAULT_LISTEN };
    let listen = std::env::var("MOCKGATE_LISTEN").unwrap_or_else(|_| default_listen.to_string());
    let metric = Metric::from_env();
    let (no_usage, drops, outcome_unknown, terminal) = parse_scenario(std::env::var("MOCKGATE_SCENARIO").ok());

    let step_size = match metric {
        Metric::Milliseconds => env_u64("MOCKGATE_STEP_SIZE_MS", DEFAULT_STEP_SIZE_MS),
        Metric::Bytes => env_u64("MOCKGATE_STEP_SIZE_BYTES", DEFAULT_STEP_SIZE_BYTES),
    };

    println!(
        "mockgate listening on {listen} — metric {} step {} price {} mint {} scenario no_usage={no_usage} drops={drops} outcome_unknown={outcome_unknown} terminal={terminal} — pubkey {}",
        metric.tag(),
        step_size,
        env_u64("MOCKGATE_PRICE_PER_STEP", 1),
        std::env::var("MOCKGATE_MINT_URL").unwrap_or_else(|_| DEFAULT_MINT_URL.to_string()),
        ad_pubkey
    );

    let state = std::sync::Arc::new(GatewayState {
        ad_pubkey: ad_pubkey.clone(),
        metric,
        step_size,
        price_per_step: env_u64("MOCKGATE_PRICE_PER_STEP", 1),
        mint_url: std::env::var("MOCKGATE_MINT_URL").unwrap_or_else(|_| DEFAULT_MINT_URL.to_string()),
        extra_mint_url: std::env::var("MOCKGATE_EXTRA_MINT_URL").ok(),
        bytes_per_sec: env_u64("MOCKGATE_BYTES_PER_SEC", DEFAULT_BYTES_PER_SEC),
        no_usage,
        expired: std::sync::atomic::AtomicBool::new(false),
        drops_left: AtomicU64::new(drops),
        outcome_unknown_left: AtomicU64::new(outcome_unknown),
        terminal_left: AtomicU64::new(terminal),
        spent: std::sync::Mutex::new(HashSet::new()),
        session: std::sync::Mutex::new(None),
    });

    let app = Router::new()
        .route("/", get(advertisement).post(handle_payment))
        .route("/usage", get(usage))
        .route("/session-state", get(session_state))
        .route("/whoami", get(|| async { "mac=DE:AD:BE:EF:00:42" }))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&listen).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn advertisement(
    axum::extract::State(state): axum::extract::State<std::sync::Arc<GatewayState>>,
) -> Json<Value> {
    let mut tags = vec![
        json!(["metric", state.metric.tag()]),
        json!(["step_size", state.step_size.to_string()]),
    ];
    if let Some(extra) = &state.extra_mint_url {
        tags.push(json!([
            "price_per_step",
            "cashu",
            state.price_per_step.to_string(),
            "sat",
            extra,
            "1"
        ]));
    }
    tags.push(json!([
        "price_per_step",
        "cashu",
        state.price_per_step.to_string(),
        "sat",
        state.mint_url,
        "1"
    ]));

    Json(json!({
        "id": format!("{:064x}", rand_nonce()),
        "pubkey": state.ad_pubkey,
        "created_at": now(),
        "kind": 10021,
        "tags": tags,
        "content": "",
        "sig": format!("{:0128x}", rand_nonce())
    }))
}

fn notice_event(customer: &str, code: &str, message: &str) -> Value {
    json!({
        "id": format!("{:064x}", rand_nonce()),
        "pubkey": "mockgate",
        "created_at": now(),
        "kind": 21023,
        "tags": [
            ["p", customer],
            ["code", code],
            ["message", message]
        ],
        "content": format!("{code}: {message}"),
        "sig": format!("{:0128x}", rand_nonce())
    })
}

fn session_event(customer: &str, metric: &str, allotment: u64, start_time: u64) -> Value {
    json!({
        "id": format!("{:064x}", rand_nonce()),
        "pubkey": "mockgate",
        "created_at": now(),
        "kind": 1022,
        "tags": [
            ["p", customer],
            ["device-identifier", "mac", "DE:AD:BE:EF:00:42"],
            ["allotment", allotment.to_string()],
            ["start-time", start_time.to_string()],
            ["metric", metric]
        ],
        "content": "",
        "sig": format!("{:0128x}", rand_nonce())
    })
}

fn session_used(state: &GatewayState, session: &MockSession) -> u64 {
    match state.metric {
        Metric::Milliseconds => session.started.elapsed().as_millis() as u64,
        Metric::Bytes => (session.started.elapsed().as_secs() as u64).saturating_mul(state.bytes_per_sec),
    }
}

async fn usage(
    axum::extract::State(state): axum::extract::State<std::sync::Arc<GatewayState>>,
) -> String {
    if state.no_usage {
        return "-1/-1".to_string();
    }
    let mut guard = state.session.lock().unwrap();
    let Some(session) = guard.as_ref() else {
        return "-1/-1".to_string();
    };
    let used = session_used(&state, session);
    if used >= session.allotment {
        *guard = None;
        state
            .expired
            .store(true, std::sync::atomic::Ordering::SeqCst);
        return "-1/-1".to_string();
    }
    format!("{used}/{}", session.allotment)
}

async fn session_state(
    axum::extract::State(state): axum::extract::State<std::sync::Arc<GatewayState>>,
) -> Json<Value> {
    let active = {
        let guard = state.session.lock().unwrap();
        guard
            .as_ref()
            .map(|s| session_used(&state, s) < s.allotment)
            .unwrap_or(false)
    };
    let state_str = if active {
        "active"
    } else if state.expired.load(std::sync::atomic::Ordering::SeqCst) {
        "expired"
    } else {
        "none"
    };
    Json(json!({
        "status": 1,
        "mac": "de:ad:be:ef:00:42",
        "state": state_str,
    }))
}

/// Accepts either a raw token body ("cashuA…") or a kind 21000 event JSON
/// whose payment tag carries the token — mirroring the real gateway, which
/// reads the bearer instrument from both shapes.
async fn handle_payment(
    axum::extract::State(state): axum::extract::State<std::sync::Arc<GatewayState>>,
    body: String,
) -> axum::response::Response {
    let trimmed = body.trim();
    let (token_str, customer) = if trimmed.starts_with("cashu") {
        (trimmed.to_string(), "raw-token-client".to_string())
    } else {
        let event: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(e) => return json_error_response(format!("unrecognized payment body: {e}")),
        };
        if event["kind"].as_u64() != Some(21000) {
            return json_error_response("expected kind 21000".to_string());
        }
        let customer = event["pubkey"].as_str().unwrap_or("unknown").to_string();
        let mut token_str: Option<String> = None;
        if let Some(tags) = event["tags"].as_array() {
            for tag in tags {
                if let Some(parts) = tag.as_array() {
                    if parts.first().and_then(|v| v.as_str()) == Some("payment") {
                        token_str = parts.get(1).and_then(|v| v.as_str()).map(String::from);
                    }
                }
            }
        }
        match token_str {
            Some(t) => (t, customer),
            None => return json_error_response("payment event missing payment tag".to_string()),
        }
    };

    // Scenario: swallow the payment without registering or answering —
    // the client must keep the token as pending and retry the SAME one.
    if state
        .drops_left
        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |v| v.checked_sub(1))
        .is_ok()
    {
        println!("mockgate: DROP (scenario) token from {customer}");
        return axum::response::Response::builder()
            .status(200)
            .body(axum::body::Body::empty())
            .unwrap();
    }

    if state
        .outcome_unknown_left
        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |v| v.checked_sub(1))
        .is_ok()
    {
        println!("mockgate: OUTCOME-UNKNOWN (scenario) token from {customer}");
        return axum::Json(notice_event(
            &customer,
            "payment-outcome-unknown",
            "gateway could not determine the outcome; do not retry this note",
        ))
        .into_response();
    }

    if state
        .terminal_left
        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |v| v.checked_sub(1))
        .is_ok()
    {
        println!("mockgate: TERMINAL mint-not-accepted (scenario) token from {customer}");
        return axum::Json(notice_event(
            &customer,
            "payment-error-mint-not-accepted",
            "the gateway does not accept this mint",
        ))
        .into_response();
    }

    {
        let spent = state.spent.lock().unwrap();
        if spent.contains(&token_str) {
            println!("mockgate: REPLAY of an already-spent token from {customer}");
            return axum::Json(notice_event(
                &customer,
                "payment-error-token-spent",
                "Cashu token already spent",
            ))
            .into_response();
        }
    }

    let amount_sats: u64 = match Token::from_str(&token_str) {
        Ok(t) => t.value().map(u64::from).unwrap_or(0),
        Err(e) => {
            println!("mockgate: bad token: {e}");
            return json_error_response(format!("invalid cashu token: {e}"));
        }
    };
    if amount_sats == 0 {
        return json_error_response("token carries zero sats".to_string());
    }

    let allotment = amount_sats
        .checked_mul(state.step_size)
        .filter(|a| *a <= 365 * 24 * 60 * 60 * 1000)
        .unwrap_or(0);
    if allotment == 0 {
        return json_error_response("payment too large".to_string());
    }

    state.spent.lock().unwrap().insert(token_str);
    state
        .expired
        .store(false, std::sync::atomic::Ordering::SeqCst);
    let (total_allotment, start_time) = {
        let mut guard = state.session.lock().unwrap();
        let now_ms = now().saturating_mul(1000);
        match guard.as_mut() {
            Some(session) if session_used(&state, session) < session.allotment => {
                session.allotment = session.allotment.saturating_add(allotment);
                (session.allotment, session.start_time)
            }
            _ => {
                *guard = Some(MockSession {
                    started: Instant::now(),
                    allotment,
                    start_time: now_ms,
                });
                (allotment, now_ms)
            }
        }
    };
    println!(
        "mockgate: accepted {amount_sats} sats from {customer} -> allotment {allotment} {}",
        state.metric.tag()
    );

    axum::Json(session_event(
        &customer,
        state.metric.tag(),
        total_allotment,
        start_time,
    ))
    .into_response()
}

fn json_error_response(message: String) -> axum::response::Response {
    axum::Json(json!({ "error": message })).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scenario_parsing() {
        let (no_usage, drops, unknown, terminal) =
            parse_scenario(Some("drop-first=2,outcome-unknown-once".into()));
        assert!((no_usage, drops, unknown, terminal) == (false, 2, 1, 0));
        let (no_usage, drops, unknown, terminal) = parse_scenario(Some("no-usage".into()));
        assert!((no_usage, drops, unknown, terminal) == (true, 0, 0, 0));
        let (no_usage, drops, unknown, terminal) = parse_scenario(None);
        assert!((no_usage, drops, unknown, terminal) == (false, 0, 0, 0));
    }

    #[test]
    fn env_u64_defaults_on_garbage() {
        // cannot set env in parallel tests reliably; just check default path
        // with a name nothing sets
        assert_eq!(env_u64("MOCKGATE_TEST_UNSET_VAR_XYZ", 42), 42);
    }
}
