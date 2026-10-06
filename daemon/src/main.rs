//! cashud — Cashu wallet daemon for the Omarchy plugin.
//!
//! cdk 0.18 + cdk-sqlite + axum on 127.0.0.1:3939. The Quickshell plugin
//! (nutbar) polls and posts here. All Cashu protocol logic — proofs,
//! keysets, quotes, sagas, restore, multi-mint wallet management — lives in
//! cdk (`Wallet`/`WalletRepository`); this daemon is the Omarchy UX adapter:
//! the HTTP API, the TollGate client flow, the offline payment stash, and
//! NetworkManager integration. See docs/cashu-architecture-research.md.

mod compact;
mod invoice;
mod stash;
mod state;
mod tollgate;
mod wifi;

use std::{
    net::SocketAddr,
    path::PathBuf,
    str::FromStr,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    sync::Arc,
    time::Duration,
};

use anyhow::{bail, Context, Result};
use axum::{
    extract::State,
    routing::{get, post},
    Json, Router,
};
use bip39::Mnemonic;
use cdk::amount::SplitTarget;
use cdk::cdk_database::WalletDatabase;
use cdk::mint_url::MintUrl;
use cdk::nuts::{CurrencyUnit, MintQuoteState, PaymentMethod, Token};
use cdk::wallet::{ReceiveOptions, Wallet, WalletRepository, WalletRepositoryBuilder};
use cdk_sqlite::wallet::WalletSqliteDatabase;
use rand::RngCore;
use serde_json::{json, Value};
use tokio::sync::Mutex;

use std::collections::HashMap;

use crate::invoice::invoice_metadata;
use crate::stash::Stash;

/// Early-return an API error body from a handler inner function.
macro_rules! bail_json {
    ($($arg:tt)*) => {
        return Err(serde_json::json!({ "ok": false, "error": format!($($arg)*) }))
    };
}
use crate::tollgate::{
    fetch_advertisement, fetch_client_mac, fetch_session_state, fetch_usage, payment_cost,
    post_token, ActiveSession, SessionState,
};
use crate::wifi::WifiState;

const DEFAULT_MINT: &str = "https://testnut.cashu.space"; // public TEST mint — tokens here have no money value
const DEFAULT_LISTEN: &str = "127.0.0.1:3939";

#[derive(Clone)]
struct AppState {
    repo: WalletRepository,
    localstore: Arc<WalletSqliteDatabase>,
    default_mint: String,
    session: Arc<Mutex<Option<ActiveSession>>>,
    /// Gateway of our most recently observed *expired* TollGate session
    /// (0.6 `/session-state` precision) — drives the panel's renew hint.
    expired_gateway: Arc<Mutex<Option<String>>>,
    payment_lock: Arc<Mutex<()>>,
    stash: Stash,
    wifi: WifiState,
    autopay: bool,
    autopay_halted: Arc<AtomicBool>,
    renewal_offset_ms: u64,
    renewal_offset_bytes: u64,
    stash_target: usize,
    max_payment_sats: u64,
    renewals: Arc<AtomicU64>,
    blind_payments: Arc<AtomicU64>,
    max_blind_payments: u64,
    refill_active: Arc<AtomicBool>,
    /// Lightning quote id → mint URL for invoices this daemon created.
    /// /invoice/status and /invoice/complete MUST address the wallet of the
    /// mint the quote was created against — routing them to the default
    /// mint breaks every non-default-mint top-up (the multi-mint SPOF fix).
    quote_mints: Arc<Mutex<HashMap<String, String>>>,
    /// Open cdk saga rows, cached by the compactor task — a live count
    /// per /status poll would re-deserialize every saga row every 5s,
    /// the exact boot-drag the compactor exists to kill (S7 evidence).
    open_sagas: Arc<AtomicU64>,
    /// Last autopay attempt from the expired-resume middle path — paces
    /// the 5s resume probe to at most one payment attempt per 30s; the
    /// panel marker stays either way.
    expired_pay_attempt: Arc<std::sync::Mutex<Option<std::time::Instant>>>,
    /// Same pacing for first-connect autopay: at most one attempt per 30s
    /// while sitting on an unpaid TollGate AP.
    first_pay_attempt: Arc<std::sync::Mutex<Option<std::time::Instant>>>,
    /// Cumulative sats paid per gateway this daemon run — spend
    /// attribution so a balance delta is readable against steps bought
    /// (the "16 sats for cost_sats=1" field question was 16 one-sat
    /// steps, not fees; a fee-0 mint charges nothing per swap).
    gateway_spent: Arc<Mutex<HashMap<String, u64>>>,
}

fn data_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("CASHUD_DATA_DIR") {
        return PathBuf::from(dir);
    }
    if let Ok(xdg) = std::env::var("XDG_DATA_HOME") {
        if !xdg.is_empty() {
            return PathBuf::from(xdg).join("omarchy-cashu");
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/home/user".to_string());
    PathBuf::from(home).join(".local/share/omarchy-cashu")
}

fn write_secret(path: &std::path::Path, contents: &str) -> Result<()> {
    std::fs::write(path, contents)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

fn parse_seed_hex(hex: &str) -> Result<[u8; 64]> {
    let hex = hex.trim();
    if hex.len() != 128 {
        bail!("seed.hex is malformed — refusing to regenerate and orphan wallet.db");
    }
    let mut seed = [0u8; 64];
    for (i, chunk) in hex.as_bytes().chunks(2).enumerate() {
        let s = std::str::from_utf8(chunk)?;
        seed[i] = u8::from_str_radix(s, 16).context("bad seed hex")?;
    }
    Ok(seed)
}

/// Load the 64-byte wallet seed. New wallets get a BIP39 12-word mnemonic
/// (`mnemonic.txt`, also derivable back into the seed); the operative secret
/// on disk stays `seed.hex` either way, which is what cdk consumes. Fails if
/// a wallet database already exists but the seed is missing/invalid —
/// regenerating a seed would orphan every persisted proof.
fn load_or_create_seed(dir: &PathBuf, wallet_exists: bool) -> Result<([u8; 64], Option<String>)> {
    let seed_path = dir.join("seed.hex");
    let mnemonic_path = dir.join("mnemonic.txt");
    if let Ok(hex) = std::fs::read_to_string(&seed_path) {
        return Ok((parse_seed_hex(&hex)?, None));
    }
    if seed_path.exists() {
        bail!("seed.hex is unreadable — refusing to regenerate a seed");
    }
    if let Ok(words) = std::fs::read_to_string(&mnemonic_path) {
        let words = words.trim();
        let mnemonic = Mnemonic::parse(words)
            .context("mnemonic.txt is not a valid BIP39 mnemonic")?;
        let seed = mnemonic.to_seed_normalized("");
        let hex: String = seed.iter().map(|b| format!("{b:02x}")).collect();
        write_secret(&seed_path, &format!("{hex}\n"))?;
        return Ok((seed, Some(words.to_string())));
    }
    if wallet_exists {
        bail!("wallet.db exists but seed.hex is missing — refusing to regenerate a seed");
    }
    let mut entropy = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut entropy);
    let mnemonic = Mnemonic::from_entropy(&entropy).context("could not generate mnemonic")?;
    let words = mnemonic.words().collect::<Vec<_>>().join(" ");
    let seed = mnemonic.to_seed_normalized("");
    let hex: String = seed.iter().map(|b| format!("{b:02x}")).collect();
    std::fs::create_dir_all(dir)?;
    write_secret(&mnemonic_path, &format!("{words}\n"))?;
    write_secret(&seed_path, &format!("{hex}\n"))?;
    Ok((seed, Some(words)))
}

/// Build the upstream multi-mint wallet repository over one SQLite store and
/// bind immediately; saga recovery for every persisted wallet runs as a
/// background task so the panel/API is available while recovery drains
/// pending sagas. cdk's recover_incomplete_sagas is sequential with network
/// round-trips per saga — minutes with payment history. The panel shows
/// DB-backed balance (reserved proofs excluded) immediately; spending gates
/// on per-proof availability, not on recovery completion.
async fn build_repo(
    dir: &PathBuf,
    mint_url: &str,
) -> Result<(WalletRepository, Arc<WalletSqliteDatabase>, [u8; 64])> {
    let db_path = dir.join("wallet.db");
    // Capture existence BEFORE the store open — cdk-sqlite creates the file,
    // and load_or_create_seed must not mistake a fresh install for an
    // existing wallet.
    let db_existed = db_path.exists();
    let localstore = Arc::new(
        WalletSqliteDatabase::new(&db_path)
            .await
            .context("failed to open wallet database")?,
    );
    let (seed, _mnemonic) = load_or_create_seed(dir, db_existed)?;
    let repo = WalletRepositoryBuilder::new()
        .localstore(localstore.clone())
        .seed(seed)
        .build()
        .await
        .context("WalletRepository build failed")?;

    let default_mint: MintUrl = mint_url
        .parse()
        .with_context(|| format!("invalid CASHUD_MINT {mint_url}"))?;
    let default_wallet = repo
        .get_or_create_wallet(default_mint, CurrencyUnit::Sat, None)
        .await
        .context("default wallet creation failed")?;
    // Best-effort metadata fetch so a fresh install lists the default mint
    // in /mints even before the first wallet operation persists it.
    if let Err(e) = default_wallet.fetch_mint_info().await {
        tracing::warn!(error = %e, "default mint metadata fetch failed (offline?) — will retry on use");
    }

    let recovery_repo = repo.clone();
    let recovery_store = localstore.clone();
    tokio::spawn(async move {
        let mut attempt = 0u32;
        loop {
            attempt += 1;
            let mut clean = true;
            match recovery_store.get_mints().await {
                Ok(mints) => {
                    for mint in mints.keys() {
                        let Ok(wallet) = recovery_repo
                            .get_wallet(mint, &CurrencyUnit::Sat)
                            .await
                        else {
                            continue;
                        };
                        match wallet.recover_incomplete_sagas().await {
                            Ok(report) => {
                                if report.recovered > 0 || report.compensated > 0 {
                                    clean = false;
                                    tracing::info!(
                                        %mint,
                                        recovered = report.recovered,
                                        compensated = report.compensated,
                                        "saga recovery progressed"
                                    );
                                }
                            }
                            Err(e) => {
                                clean = false;
                                tracing::warn!(%mint, error = %e, "saga recovery failed (will retry)");
                            }
                        }
                        if let Ok(minted) = wallet.mint_unissued_quotes().await {
                            if u64::from(minted) > 0 {
                                tracing::info!(%mint, minted = u64::from(minted), "minted pending quotes");
                            }
                        }
                    }
                }
                Err(e) => {
                    clean = false;
                    tracing::warn!(attempt, error = %e, "could not enumerate mints for recovery");
                }
            }
            if clean {
                tracing::info!(attempt, "saga recovery complete (background)");
                break;
            }
            if attempt >= 10 {
                tracing::warn!("saga recovery gave up after 10 attempts — manual restore() may be needed");
                break;
            }
            tokio::time::sleep(Duration::from_secs(30)).await;
        }
    });

    warn_legacy_mint_dbs(dir);
    Ok((repo, localstore, seed))
}

/// Pre-repository cashud kept one SQLite file per mint (`wallet-<hash>.db`).
/// Those files are NOT read by the repository layout; surface them loudly so
/// an operator can sweep any balance (send-all → receive) before deleting.
fn warn_legacy_mint_dbs(dir: &PathBuf) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        let legacy: Vec<String> = entries
            .filter_map(|e| e.ok())
            .filter(|e| {
                let n = e.file_name();
                let n = n.to_string_lossy();
                n.starts_with("wallet-") && n.ends_with(".db")
            })
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        if !legacy.is_empty() {
            tracing::warn!(
                ?legacy,
                "legacy per-mint wallet databases found; the wallet repository uses only wallet.db. Sweep balances (POST /send without amount → /receive) from an old checkout, then remove these files"
            );
        }
    }
}

// ---- handlers ----

/// Top-level error for mint-dependent wallet operations. cdk's transport
/// errors name neither the mint nor the user action ("Http transport error
/// Some(502)"), and `.context()` wrapping hides the cause entirely
/// ("mint_quote failed") — during an upstream outage the panel showed a
/// cryptic dead end instead of "your mint is down". Name the mint, the
/// operation, and keep the full causal chain (`.context`, so downstream
/// classification can still see the cdk error; `{:#}` renders the same
/// single line the old baked format did).
fn mint_op_error<E: Into<anyhow::Error>>(op: &str, mint: &str, err: E) -> anyhow::Error {
    err.into()
        .context(format!("mint {mint} is unreachable or failing while {op}"))
}

/// Money-safety taxonomy of a mint failure, derived from cdk's own error
/// discriminators. "Your balance is safe on this computer" is ONLY
/// claimable for [`MintFailureKind::Unreachable`] — the mint never saw the
/// request. An HTTP status means the mint answered: it is up and either
/// rejected the request (4xx, definitive) or is failing server-side
/// (5xx). Everything else is protocol-level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MintFailureKind {
    Unreachable,
    HttpClientError(u16),
    HttpServerError(u16),
    Protocol,
}

impl MintFailureKind {
    fn label(self) -> &'static str {
        match self {
            Self::Unreachable => "unreachable",
            Self::HttpClientError(_) => "http_client_error",
            Self::HttpServerError(_) => "http_server_error",
            Self::Protocol => "protocol",
        }
    }

    fn http_status(self) -> Option<u16> {
        match self {
            Self::HttpClientError(code) | Self::HttpServerError(code) => Some(code),
            _ => None,
        }
    }
}

/// Walk an error chain for cdk's own error and classify it. `None` when no
/// cdk error is in the chain (not a mint failure).
fn classify_mint_failure(err: &anyhow::Error) -> Option<MintFailureKind> {
    let cdk_error = err
        .chain()
        .find_map(|cause| cause.downcast_ref::<cdk::error::Error>())?;
    Some(match cdk_error {
        cdk::error::Error::HttpError(None, _) | cdk::error::Error::Timeout => {
            MintFailureKind::Unreachable
        }
        cdk::error::Error::HttpError(Some(status), _) if *status < 500 => {
            MintFailureKind::HttpClientError(*status)
        }
        cdk::error::Error::HttpError(Some(status), _) => MintFailureKind::HttpServerError(*status),
        _ => MintFailureKind::Protocol,
    })
}

/// Error response for a failed mint operation: the established `.error`
/// string (byte-identical — the hermetic top-up contract locks its shape)
/// plus the additive machine-readable split the panel's money-safety copy
/// branches on.
fn mint_error_response<E: Into<anyhow::Error>>(op: &str, mint: &str, err: E) -> Value {
    let err = mint_op_error(op, mint, err);
    let mut body = json!({ "ok": false, "error": format!("{err:#}") });
    attach_mint_failure(&mut body, &err);
    body
}

/// Add the failure-kind fields to an error body when the error chain
/// carries a classifiable cdk error; leave non-mint errors untouched.
fn attach_mint_failure(body: &mut Value, err: &anyhow::Error) {
    if let Some(kind) = classify_mint_failure(err) {
        body["mint_failure_kind"] = json!(kind.label());
        if let Some(status) = kind.http_status() {
            body["http_status"] = json!(status);
        }
    }
}

/// Count a token's proofs per keyset (cdk's fee inputs). TokenV4 groups
/// carry short keyset ids — v1 ids reconstruct standalone, v2 ids resolve
/// against the cached keyset list; V3 tokens carry none, so their fee is
/// unknown.
fn token_proof_counts(
    token: &Token,
    keysets: &[cdk::nuts::KeySetInfo],
) -> Option<HashMap<cdk::nuts::Id, u64>> {
    let mut counts: HashMap<cdk::nuts::Id, u64> = HashMap::new();
    match token {
        Token::TokenV4(v4) => {
            for group in &v4.token {
                let id = cdk::nuts::Id::from_short_keyset_id(&group.keyset_id, keysets).ok()?;
                *counts.entry(id).or_default() += group.proofs.len() as u64;
            }
            Some(counts)
        }
        Token::TokenV3(_) => None,
    }
}

/// Fee the mint charges to REDEEM a token (NUT-06 input fee per proof,
/// cdk's own computation). This is what a fee-bearing mint shaves off the
/// gateway's side — the reason payment tokens must carry cost + fee.
/// Unknown keysets (foreign token, offline mint) yield None.
async fn token_redemption_fee(wallet: &Wallet, token: &Token) -> Option<u64> {
    // Local keyset rows only — no network; our own tokens' keysets are
    // always in the local store (offline-safe, S10).
    let keysets = wallet
        .localstore
        .get_mint_keysets(wallet.mint_url.clone())
        .await
        .ok()
        .flatten()
        .unwrap_or_default();
    let counts = token_proof_counts(token, &keysets)?;
    let breakdown = wallet.get_proofs_fee_by_count(counts).await.ok()?;
    Some(u64::from(breakdown.total))
}

/// Fee-aware payment coverage: a posted token must be worth at least the
/// gateway cost, and — when the mint's redemption fee is known — no more
/// than cost + that fee. Under-paying buys no step at fee-bearing mints
/// (`payment-error-below-swap-fee` at the real gateway); over-paying
/// beyond the fee means the wrong token was journaled. An unknown fee
/// (V3 token, offline mint) keeps only the under-payment guard so
/// offline stash renewals still work (S10).
fn token_covers_payment(
    value_sats: u64,
    cost_sats: u64,
    redemption_fee_sats: Option<u64>,
) -> std::result::Result<(), String> {
    if value_sats < cost_sats {
        return Err(format!(
            "token is {value_sats} sat but this gateway payment costs {cost_sats} sat — under-covered"
        ));
    }
    if let Some(fee) = redemption_fee_sats {
        if value_sats > cost_sats.saturating_add(fee) {
            return Err(format!(
                "token is {value_sats} sat, beyond cost {cost_sats} + redemption fee {fee} — wrong token for this payment"
            ));
        }
    }
    Ok(())
}

/// A failed selection at HEALTHY balance is a denomination mismatch, not
/// poverty: fixed-denomination proofs cannot make exact change without a
/// mint swap, and cdk reports every selection failure as
/// `InsufficientFunds` — including the 4071-sat-balance field case
/// (estate-lead GO C1, 2026-09-30).
fn selection_failure_is_denomination_mismatch(
    error: &cdk::error::Error,
    balance_sats: u64,
    cost_sats: u64,
) -> bool {
    matches!(error, cdk::error::Error::InsufficientFunds) && balance_sats >= cost_sats
}

/// Prepare an exact online send, healing the selection pool when the
/// wallet's visible balance cannot cover a tiny amount. Incomplete sagas
/// reserve proofs and stale local states shrink the pool that
/// `total_balance()` still counts — the field signature is "8444-sat
/// balance, 1-sat send, InsufficientFunds" (laptop evidence 2026-09-30).
/// One idempotent recovery pass (network-bounded), then a single retry.
async fn prepare_send_with_recovery(
    wallet: &Wallet,
    amount: u64,
) -> Result<cdk::wallet::PreparedSend<'_>, cdk::error::Error> {
    let opts = cdk::wallet::SendOptions {
        include_fee: true,
        ..Default::default()
    };
    match wallet
        .prepare_send(cdk::Amount::from(amount), opts.clone())
        .await
    {
        Err(cdk::error::Error::InsufficientFunds) => {
            let reserved: u64 = wallet
                .total_reserved_balance()
                .await
                .map(u64::from)
                .unwrap_or_default();
            tracing::warn!(
                amount,
                reserved,
                "selection pool could not cover the send; recovering incomplete sagas and retrying once"
            );
            if let Err(e) = wallet.recover_incomplete_sagas().await {
                tracing::warn!(error = %e, "saga recovery pass failed during prepare retry");
            }
            wallet.prepare_send(cdk::Amount::from(amount), opts).await
        }
        other => other,
    }
}

/// Honest copy for a failed payment prepare: name the mechanism (a mint
/// swap is needed for exact change; the mint may be unreachable behind
/// the captive gate) instead of the lying "Insufficient funds". No money
/// has moved — prepare failed before anything was spent.
async fn honest_prepare_failure(
    error: cdk::error::Error,
    cost_sats: u64,
    wallet: &Wallet,
) -> anyhow::Error {
    let balance: u64 = wallet.total_balance().await.unwrap_or_default().into();
    let reserved: u64 = wallet
        .total_reserved_balance()
        .await
        .map(u64::from)
        .unwrap_or_default();
    if selection_failure_is_denomination_mismatch(&error, balance, cost_sats) {
        let reserved_note = if reserved > 0 {
            format!(" ({reserved} sat of that is reserved by incomplete operations — recovery will release it)")
        } else {
            String::new()
        };
        anyhow::anyhow!(
            "cannot make exact {cost_sats}-sat change from the wallet's current denominations — a mint swap is needed and the mint may be unreachable behind the captive gate; no money moved, balance is {balance} sat{reserved_note}; the offline stash covers this when primed"
        )
    } else {
        anyhow::Error::new(error).context(format!(
            "prepare_send failed (insufficient funds?) — balance {balance} sat, {reserved} sat reserved by incomplete operations"
        ))
    }
}

/// Stash tokens are the offline exact-change primitive: one token = one
/// 1-sat payment (plus the mint's redemption fee at fee-bearing mints —
/// `include_fee`), so a cost_sats=1 gateway is payable with NO mint
/// roundtrip (S10's offline-renewal finding). The stash-checkout branch
/// below and `prime_stash` both key on this.
const STASH_TOKEN_SATS: u64 = 1;

/// Serialize and journal the complete money path. A transport failure keeps
/// the pending token for retry; it never falls through to a newly minted note.
async fn pay_auto(st: &AppState, gateway: &str, steps: Option<u64>) -> Result<ActiveSession> {    let _payment_guard = st.payment_lock.lock().await;
    let wallet = st.wallet_for(None).await?;
    if st.blind_payments.load(Ordering::Relaxed) >= st.max_blind_payments {
        st.autopay_halted.store(true, Ordering::Relaxed);
        bail!(
            "{} accepted payment(s) have not appeared in /usage; refusing to spend again",
            st.max_blind_payments
        );
    }

    let ad = fetch_advertisement(gateway).await?;
    let (steps, cost) = payment_cost(&ad, &st.default_mint, steps)?;
    if cost > st.max_payment_sats {
        st.autopay_halted.store(true, Ordering::Relaxed);
        bail!(
            "gateway payment costs {cost} sats, above CASHUD_MAX_PAYMENT_SATS={} safety limit",
            st.max_payment_sats
        );
    }
    let expected_increment = steps.saturating_mul(ad.step_size);
    tracing::info!(
        gateway,
        gateway_pubkey = %ad.tollgate_pubkey,
        metric = %ad.metric,
        expected_increment,
        "validated TollGate offer"
    );
    let (token, origin) = if let Some(token) = st.stash.pending() {
        (token, "pending")
    } else if cost == STASH_TOKEN_SATS && st.stash.count() > 0 {
        let (token, reused) = st.stash.checkout().map_err(|e| anyhow::anyhow!("{e}"))?;
        (token, if reused { "pending" } else { "offline stash" })
    } else {
        // include_fee (inside prepare_send_with_recovery): the gateway
        // redeems this token at the mint, and a fee-bearing mint shaves
        // its input fee off the redeemed value — a cost-exact token
        // under-pays and buys no step. cdk sizes the outputs to cost +
        // the redemption fee (no-op at fee 0).
        let prepared = match prepare_send_with_recovery(&wallet, cost).await {
            Ok(prepared) => prepared,
            Err(e) => return Err(honest_prepare_failure(e, cost, &wallet).await),
        };
        let token = prepared
            .confirm(None)
            .await
            .context("confirm send failed")?
            .to_string();
        st.stash
            .set_pending(&token)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        (token, "wallet")
    };

    let parsed = Token::from_str(&token).context("pending payment token is invalid")?;
    let token_mint = parsed
        .mint_url()
        .map(|url| url.to_string())
        .unwrap_or_default();
    let token_value: u64 = parsed.value().context("pending token has no value")?.into();
    let redemption_fee = token_redemption_fee(&wallet, &parsed).await;
    let mint_mismatch =
        token_mint.trim_end_matches('/') != st.default_mint.trim_end_matches('/');
    if mint_mismatch || token_covers_payment(token_value, cost, redemption_fee).is_err() {
        st.autopay_halted.store(true, Ordering::Relaxed);
        bail!(
                "pending token is {token_value} sat from {token_mint}, but this gateway payment requires {cost} sat (+ redemption fee {}) from {}; recover or quarantine it before continuing",
                redemption_fee.map(|f| f.to_string()).unwrap_or_else(|| "unknown".to_string()),
                st.default_mint
            );
    }

    // A 1022 response is not enough to prove the gateway mapped the credit
    // to this client. Remember the current allotment so `/usage` growth can
    // provide that proof (tollgate-module-basic-go #422/#425).
    let credit_baseline = fetch_usage(gateway)
        .await
        .ok()
        .flatten()
        .map(|usage| usage.allotment);

    st.stash
        .set_pending_meta(&stash::PendingMeta {
            gateway: gateway.to_string(),
            cost_sats: cost,
            created_unix: tollgate::now_ms() / 1000,
        })
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    tracing::info!(
        gateway,
        cost,
        steps,
        origin,
        "posting journaled raw payment token"
    );
    let outcome = post_token(gateway, &token, &ad, cost).await;
    if let Err(e) = &outcome {
        tracing::warn!(gateway, error = %e, "payment POST failed — pending token preserved for retry by the resolver");
    }
    settle_payment_outcome(
        st,
        gateway,
        &ad,
        cost,
        expected_increment,
        &token,
        credit_baseline,
        outcome?,
    )
    .await
}

/// Apply the definitive/ambiguous outcome of a posted payment: complete the
/// journal on success, quarantine on ambiguity, preserve on rejection.
/// Shared by the interactive pay path and the pending resolver so the
/// outcome taxonomy has exactly one implementation.
#[allow(clippy::too_many_arguments)]
async fn settle_payment_outcome(
    st: &AppState,
    gateway: &str,
    ad: &tollgate::TollGateAd,
    cost: u64,
    expected_increment: u64,
    token: &str,
    credit_baseline: Option<u64>,
    outcome: tollgate::PayOutcome,
) -> Result<ActiveSession> {
    match outcome {
        tollgate::PayOutcome::Session(mut session) => {
            st.stash
                .complete(token)
                .map_err(|e| anyhow::anyhow!("payment accepted but journal cleanup failed: {e}"))?;
            st.blind_payments.fetch_add(1, Ordering::Relaxed);
            *st.gateway_spent.lock().await.entry(gateway.to_string()).or_insert(0) += cost;
            session.credit_baseline = credit_baseline;
            let expected_total = credit_baseline
                .unwrap_or_default()
                .saturating_add(expected_increment);
            if session.allotment < expected_total {
                tracing::warn!(
                    credited = session
                        .allotment
                        .saturating_sub(credit_baseline.unwrap_or_default()),
                    expected_increment,
                    "gateway credited less than the advertised purchase (mint swap fees may be the cause)"
                );
            }
            st.autopay_halted.store(false, Ordering::Relaxed);
            spawn_stash_refill(st.clone());
            Ok(*session)
        }
        tollgate::PayOutcome::Spent => {
            st.stash
                .complete(token)
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            *st.gateway_spent.lock().await.entry(gateway.to_string()).or_insert(0) += cost;
            if let Some(usage) = fetch_usage(gateway).await? {
                st.autopay_halted.store(false, Ordering::Relaxed);
                return Ok(ActiveSession::from_usage(gateway, &ad.metric, cost, usage));
            }
            st.autopay_halted.store(true, Ordering::Relaxed);
            bail!("gateway says the pending token is spent but /usage shows no session; refusing to pay a replacement")
        }
        tollgate::PayOutcome::OutcomeUnknown(message) => {
            st.autopay_halted.store(true, Ordering::Relaxed);
            let path = st
                .stash
                .quarantine(token, "outcome-unknown")
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            tracing::warn!(gateway, %message, path = %path.display(), "payment outcome unknown — token quarantined");
            bail!(
                "gateway reported payment-outcome-unknown: {message}; token quarantined at {}",
                path.display()
            );
        }
        tollgate::PayOutcome::Terminal { code, message } => {
            st.autopay_halted.store(true, Ordering::Relaxed);
            let path = st
                .stash
                .quarantine(token, &code)
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            tracing::warn!(gateway, code, %message, path = %path.display(), "terminal gateway rejection — token retained");
            bail!(
                "terminal gateway rejection {code}: {message}; token retained at {}",
                path.display()
            );
        }
        tollgate::PayOutcome::Rejected { code, message } => {
            tracing::warn!(gateway, code, %message, "gateway rejected payment — pending token preserved for retry");
            bail!(
                "gateway rejected payment{}: {message}; pending token preserved for retry",
                code.map(|value| format!(" ({value})")).unwrap_or_default()
            );
        }
    }
}

/// Resolve payments stranded in `payment.pending` (transport loss, daemon
/// restart, ambiguous delivery). With journaled delivery context the SAME
/// token is re-POSTed — the gateway's `payment-error-token-spent` answer
/// on replay is the protocol's own idempotency, so a replay can never pay
/// twice. Legacy pendings without context are surfaced, never retried.
async fn pending_payment_resolver(st: AppState) {
    let mut last_surface = tokio::time::Instant::now()
        .checked_sub(Duration::from_secs(3600))
        .unwrap_or_else(tokio::time::Instant::now);
    loop {
        tokio::time::sleep(Duration::from_secs(10)).await;
        let Some(token) = st.stash.pending() else {
            continue;
        };
        let Some(meta) = st.stash.pending_meta() else {
            if last_surface.elapsed() >= Duration::from_secs(60) {
                tracing::warn!(
                    "pending payment without delivery context — operator reconciliation needed (see payment.pending)"
                );
                last_surface = tokio::time::Instant::now();
            }
            continue;
        };
        if st.blind_payments.load(Ordering::Relaxed) >= st.max_blind_payments {
            continue;
        }
        let _payment_guard = st.payment_lock.lock().await;
        let ad = match tollgate::fetch_advertisement(&meta.gateway).await {
            Ok(ad) => ad,
            Err(e) => {
                tracing::warn!(gateway = %meta.gateway, error = %e, "pending resolver: gateway unreachable, will retry");
                continue;
            }
        };
        tracing::info!(gateway = %meta.gateway, cost = meta.cost_sats, "resolver re-posting pending payment token");
        match tollgate::post_token(&meta.gateway, &token, &ad, meta.cost_sats).await {
            Ok(outcome) => {
                let credit_baseline = fetch_usage(&meta.gateway)
                    .await
                    .ok()
                    .flatten()
                    .map(|usage| usage.allotment);
                let expected_increment = meta.cost_sats.saturating_mul(ad.step_size);
                match settle_payment_outcome(
                    &st,
                    &meta.gateway,
                    &ad,
                    meta.cost_sats,
                    expected_increment,
                    &token,
                    credit_baseline,
                    outcome,
                )
                .await
                {
                    Ok(session) => {
                        *st.session.lock().await = Some(session);
                        tracing::info!(gateway = %meta.gateway, "resolver settled pending payment");
                    }
                    Err(e) => {
                        tracing::warn!(gateway = %meta.gateway, error = %e, "resolver could not settle pending payment yet");
                    }
                }
            }
            Err(e) => {
                tracing::warn!(gateway = %meta.gateway, error = %e, "resolver re-POST failed, will retry");
            }
        }
    }
}

/// Periodic saga compaction: prune `Send/TokenCreated` rows the mint
/// confirms redeemed, so the repository stays compact across a wallet's
/// life and boot recovery never re-pays ~3s per stale row (the
/// 4321-saga-wallet boot drag). Unspent rows are retained — the revoke
/// window is money (see `compact.rs`).
async fn saga_compactor(st: AppState, interval: Duration) {
    loop {
        let wallets: Vec<Wallet> = st
            .list_mints()
            .await
            .into_iter()
            .filter_map(|(_, wallet)| wallet)
            .collect();
        let outcome = compact::compaction_pass(&wallets, compact::COMPACTION_BATCH).await;
        if outcome.compacted > 0 {
            tracing::info!(
                compacted = outcome.compacted,
                retained = outcome.retained,
                open = outcome.open_sagas,
                in_flight = outcome.in_flight_open,
                rollbackable = outcome.rollbackable_open,
                "saga compaction pruned completed send operations"
            );
        }
        st.open_sagas
            .store(outcome.open_sagas as u64, Ordering::Relaxed);
        tokio::time::sleep(interval).await;
    }
}

fn spawn_stash_refill(st: AppState) {
    if st
        .refill_active
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
        .is_err()
    {
        return;
    }
    tokio::spawn(async move {
        let missing = st.stash_target.saturating_sub(st.stash.count());
        if missing > 0 {
            tracing::info!(missing, "refilling offline stash");
            match prime_stash(&st, missing).await {
                Ok(()) => tracing::info!(count = st.stash.count(), "stash refilled"),
                Err(e) => {
                    tracing::warn!(error = %e, "stash refill failed (offline? will retry after next payment)")
                }
            }
        }
        st.refill_active.store(false, Ordering::Release);
    });
}

/// Split `n` renewal units off the balance into single-payment stash
/// tokens. `include_fee` sizes each token to 1 sat + the mint's redemption
/// fee so a fee-bearing mint cannot shave an offline renewal below one
/// step (no-op at fee 0 — the S10 offline re-split behavior is unchanged).
/// Requires a reachable mint (each token is a swap). The payment lock is
/// taken per token, never across the loop: a refill racing a closing toll
/// valve hangs on the dead mint for a full HTTP timeout per swap, and a
/// lock held across all of them stalls every proof-consuming path —
/// including the supervisor's renewal — for minutes (issue #5 journal
/// stall).
async fn prime_stash(st: &AppState, n: usize) -> Result<()> {
    let wallet = st.wallet_for(None).await?;
    for _ in 0..n {
        {
            let _payment_guard = st.payment_lock.lock().await;
            let prepared =
                match prepare_send_with_recovery(&wallet, STASH_TOKEN_SATS).await {
                    Ok(prepared) => prepared,
                    Err(e) => {
                        return Err(honest_prepare_failure(e, STASH_TOKEN_SATS, &wallet).await)
                    }
                };
            let token = prepared.confirm(None).await.context("confirm failed")?;
            st.stash
                .put(&token.to_string())
                .map_err(|e| anyhow::anyhow!("{e}"))?;
        }
        tokio::task::yield_now().await;
    }
    Ok(())
}

/// What the supervisor should do with a session whose `/usage` poll answered
/// `Ok(None)` — the gateway authoritatively reports no session for us.
///
/// Issue #5: with autopay halted (blind-payment cap, cost cap, …) the
/// expiry-driven renewal path can never run, so the 3-failure drop is dead
/// and `Ok(None)` is not a transport error, so the 30-poll drop is dead
/// too. Retaining the session at `remaining == 0` blocks the wifi fallback
/// watchdog and hides the renewal affordance — the client strands on the
/// AP. Expiry is terminal for the halt latch: drop the session and surface
/// `expired_gateway` instead.
fn expiry_decision(autopay: bool, autopay_halted: bool, usage_was_observed: bool) -> ExpiryAction {
    if !usage_was_observed {
        // Freshly paid, /usage never confirmed anything: the blind-payment
        // machinery governs this session, not the expiry path.
        ExpiryAction::Ignore
    } else if autopay && !autopay_halted {
        // Bounded by expiry_renewal_failures (3 strikes → drop).
        ExpiryAction::Renew
    } else {
        ExpiryAction::Drop
    }
}

enum ExpiryAction {
    Drop,
    Renew,
    Ignore,
}

/// The expired-resume middle path (estate-lead GO 2026-09-30): a gateway
/// the daemon has ALREADY paid (the expired marker — first-connect stays
/// manual, unknown gateways never get a marker) is auto-renewed when
/// autopay is on and funds exist. The blind-payment cap still bounds the
/// worst case; a clone of a trusted gateway identity could trigger one
/// renewal-sized payment (documented residual risk).
fn expired_gateway_autopays(autopay: bool, stash_tokens: usize, balance_sats: u64) -> bool {
    autopay && (stash_tokens > 0 || balance_sats > 0)
}

/// First-connect autopay (laptop-lane request 2026-09-30, estate-lead
/// approval): sitting on a TollGate AP with no session and no payment
/// history at that gateway, pay automatically when autopay is opted in
/// (default off) and funds exist. Only fires while the gateway is the
/// live default route of an active TollGate AP — never for a remembered
/// `last_gateway` marker, which would pay for wifi we are not using.
/// Safety envelope: CASHUD_AUTOPAY=1 opt-in, pay_auto's cost cap and
/// blind-payment cap bound a cloned-SSID attacker.
fn first_connect_autopays(
    on_tollgate_ap: bool,
    autopay: bool,
    autopay_halted: bool,
    stash_tokens: usize,
    balance_sats: u64,
) -> bool {
    on_tollgate_ap
        && autopay
        && !autopay_halted
        && (stash_tokens > 0 || balance_sats > 0)
}

/// Single session supervisor. `/usage` is authoritative for both time and byte
/// sessions and is the only observation that resets the blind-payment cap.
async fn session_supervisor(st: AppState) {
    let mut consecutive_failures = 0u32;
    let mut payment_backoff = Duration::from_secs(5);
    let mut next_payment_attempt = tokio::time::Instant::now();
    let mut idle_ticks: u32 = 0;
    let mut expiry_renewal_failures = 0u32;
    loop {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let Some(gateway) = ({
            let guard = st.session.lock().await;
            guard.as_ref().map(|session| session.gateway.clone())
        }) else {
            // Reference-client behavior (tollgate-module-basic-go): a live
            // upstream session survives client restarts — adopt it from
            // /usage instead of paying again. Throttled to every 5s.
            idle_ticks = idle_ticks.saturating_add(1);
            if idle_ticks % 5 == 0 {
                maybe_resume_session(&st).await;
            }
            continue;
        };

        let (should_renew, expiry_driven) = match fetch_usage(&gateway).await {
            Ok(Some(usage)) => {
                consecutive_failures = 0;
                let mut guard = st.session.lock().await;
                if let Some(session) = guard.as_mut().filter(|s| s.gateway == gateway) {
                    let credit_was_observed = session.credit_observed;
                    session.update_usage(usage);
                    if !credit_was_observed && session.credit_observed {
                        st.blind_payments.store(0, Ordering::Relaxed);
                        st.autopay_halted.store(false, Ordering::Relaxed);
                    }
                    let configured_offset = if session.metric == "bytes" {
                        st.renewal_offset_bytes
                    } else {
                        st.renewal_offset_ms
                    };
                    let offset = configured_offset.min(session.allotment / 2);
                    (
                        st.autopay
                            && !st.autopay_halted.load(Ordering::Relaxed)
                            && session.remaining() <= offset,
                        false,
                    )
                } else {
                    (false, false)
                }
            }
            Ok(None) => {
                let was_observed = {
                    let guard = st.session.lock().await;
                    guard
                        .as_ref()
                        .filter(|session| session.gateway == gateway)
                        .is_some_and(|session| session.usage_observed)
                };
                match expiry_decision(
                    st.autopay,
                    st.autopay_halted.load(Ordering::Relaxed),
                    was_observed,
                ) {
                    ExpiryAction::Drop => {
                        *st.expired_gateway.lock().await = Some(gateway.clone());
                        *st.session.lock().await = None;
                        tracing::warn!(
                            %gateway,
                            halted = st.autopay_halted.load(Ordering::Relaxed),
                            "session expired at the gateway with no renewal path (autopay off or halted); dropping so wifi fallback and the renew affordance can proceed"
                        );
                    }
                    ExpiryAction::Ignore => {}
                    ExpiryAction::Renew => {}
                }
                (
                    st.autopay && !st.autopay_halted.load(Ordering::Relaxed) && was_observed,
                    true,
                )
            }
            Err(e) => {
                consecutive_failures += 1;
                if consecutive_failures >= 30 {
                    // 1s poll cadence: 30s of total gateway silence means
                    // the AP is gone or the gateway is dead — past any
                    // reasonable roam convergence. Drop the session so the
                    // fallback watchdog can move the client (and expose the
                    // panel renewal affordance); bytes sessions have no
                    // wall-clock decay, so this is their only escape.
                    *st.expired_gateway.lock().await = Some(gateway.clone());
                    *st.session.lock().await = None;
                    tracing::warn!(
                        %gateway,
                        consecutive_failures,
                        "usage unreachable; dropping session so wifi fallback can resume"
                    );
                    consecutive_failures = 0;
                } else {
                    tracing::warn!(consecutive_failures, error = %e, "session usage poll failed");
                }
                (false, false)
            }
        };

        if !should_renew {
            continue;
        }
        if tokio::time::Instant::now() < next_payment_attempt {
            continue;
        }
        tracing::info!(%gateway, "autopay renewal threshold reached");
        match pay_auto(&st, &gateway, None).await {
            Ok(new_session) => {
                expiry_renewal_failures = 0;
                payment_backoff = Duration::from_secs(5);
                next_payment_attempt = tokio::time::Instant::now() + payment_backoff;
                let count = st.renewals.fetch_add(1, Ordering::Relaxed) + 1;
                *st.session.lock().await = Some(new_session);
                *st.expired_gateway.lock().await = None;
                tracing::info!(count, "session renewed");
            }
            Err(e) => {
                next_payment_attempt = tokio::time::Instant::now() + payment_backoff;
                tracing::warn!(
                    error = %e,
                    retry_in_secs = payment_backoff.as_secs(),
                    halted = st.autopay_halted.load(Ordering::Relaxed),
                    "autopay renewal failed; pending token retained"
                );
                payment_backoff = (payment_backoff * 2).min(Duration::from_secs(60));
                if expiry_driven {
                    expiry_renewal_failures += 1;
                    if expiry_renewal_failures >= 3 {
                        // The session is gone at the gateway and renewal
                        // keeps failing (empty wallet, dead mint, …).
                        // Retaining the session blocks the wifi fallback
                        // watchdog and strands the client on the AP, so
                        // drop it: fallback resumes and the panel keeps a
                        // renewal affordance via expired_gateway
                        // (maybe_resume_session re-probes every 5s).
                        *st.expired_gateway.lock().await = Some(gateway.clone());
                        *st.session.lock().await = None;
                        tracing::warn!(
                            %gateway,
                            attempts = expiry_renewal_failures,
                            "session expired and repeated renewals failed; dropping session so wifi fallback can resume — renew from the panel when funded"
                        );
                        expiry_renewal_failures = 0;
                    }
                }
            }
        }
    }
}

/// Adopt a live upstream session without paying: probe /usage on the
/// current TollGate AP's default-route gateway, or the last gateway a
/// session was held with (survives daemon restarts on the same network).
async fn maybe_resume_session(st: &AppState) {
    let ap_gateway = if st.wifi.enabled
        && wifi::active_ssid()
            .ok()
            .flatten()
            .map(|s| s.starts_with("TollGate"))
            .unwrap_or(false)
    {
        wifi::gateway_ip().ok().flatten()
    } else {
        None
    };
    let gateway = ap_gateway.clone().or_else(|| {
        std::fs::read_to_string(data_dir().join("last_gateway"))
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    });
    let Some(gateway) = gateway else { return };
    // First-connect autopay must only fire for the AP we are actually
    // sitting on — a file-marker gateway is history, not connectivity.
    let on_tollgate_ap = ap_gateway.is_some_and(|g| g == gateway);

    match fetch_usage(&gateway).await {
        Ok(Some(usage)) => {
            let metric = fetch_advertisement(&gateway)
                .await
                .ok()
                .map(|ad| ad.metric)
                .unwrap_or_else(|| "milliseconds".to_string());
            let session = ActiveSession::from_usage(&gateway, &metric, 0, usage);
            tracing::info!(
                %gateway,
                allotment = usage.allotment,
                used = usage.used,
                "resumed live upstream session from /usage (no payment)"
            );
            *st.session.lock().await = Some(session);
            *st.expired_gateway.lock().await = None;
        }
        _ => {
            // /usage cannot distinguish "never paid" from "ran out";
            // TollGate 0.6 /session-state can. An expired session keeps the
            // gateway marker so the panel can offer a renewal (0.6 accepts
            // re-purchase after expiry); anything else forgets the gateway.
            let state = match fetch_client_mac(&gateway).await {
                Ok(mac) => fetch_session_state(&gateway, &mac).await.ok(),
                Err(_) => None,
            };
            if state == Some(SessionState::Expired) {
                *st.expired_gateway.lock().await = Some(gateway.clone());
                tracing::info!(%gateway, "tollgate session expired — renewal available");
                // The middle path (estate-lead GO 2026-09-30): auto-renew
                // a gateway we already paid when autopay is on and funds
                // exist, instead of only surfacing the marker. First
                // connect stays manual; pay_auto's blind cap bounds a
                // cloned-gateway trigger to one renewal-sized payment.
                let balance: u64 = match st.wallet_for(None).await {
                    Ok(wallet) => wallet.total_balance().await.unwrap_or_default().into(),
                    Err(_) => 0,
                };
                let attempt_due = {
                    let mut last = st.expired_pay_attempt.lock().unwrap();
                    let due = last
                        .is_none_or(|at| at.elapsed() >= Duration::from_secs(30));
                    if due {
                        *last = Some(std::time::Instant::now());
                    }
                    due
                };
                if attempt_due && expired_gateway_autopays(st.autopay, st.stash.count(), balance)
                {
                    tracing::info!(%gateway, "expired-gateway autopay: previously paid, funds available");
                    match pay_auto(st, &gateway, None).await {
                        Ok(session) => {
                            *st.session.lock().await = Some(session);
                            *st.expired_gateway.lock().await = None;
                            tracing::info!(%gateway, "expired session auto-renewed");
                        }
                        Err(e) => {
                            // Marker retained: the panel keeps the manual
                            // renewal affordance; the pending journal
                            // makes the next attempt reuse the same token.
                            tracing::warn!(
                                %gateway,
                                error = %e,
                                "expired-gateway autopay attempt failed (marker retained)"
                            );
                        }
                    }
                }
            } else {
                *st.expired_gateway.lock().await = None;
                let marker = data_dir().join("last_gateway");
                if marker.exists() {
                    let _ = std::fs::remove_file(&marker);
                }
                // First-connect autopay: no session and no history at this
                // gateway, but we are sitting on its AP with autopay opted
                // in and funds available — pay instead of falling back.
                // The 30s throttle keeps a broken gateway from being
                // hammered by the 5s resume probe; pay_auto's ad
                // validation, cost cap and blind cap bound the exposure.
                if on_tollgate_ap {
                    let balance: u64 = match st.wallet_for(None).await {
                        Ok(wallet) => wallet.total_balance().await.unwrap_or_default().into(),
                        Err(_) => 0,
                    };
                    let attempt_due = {
                        let mut last = st.first_pay_attempt.lock().unwrap();
                        let due = last
                            .is_none_or(|at| at.elapsed() >= Duration::from_secs(30));
                        if due {
                            *last = Some(std::time::Instant::now());
                        }
                        due
                    };
                    if attempt_due
                        && first_connect_autopays(
                            on_tollgate_ap,
                            st.autopay,
                            st.autopay_halted.load(Ordering::Relaxed),
                            st.stash.count(),
                            balance,
                        )
                    {
                        tracing::info!(%gateway, "first-connect autopay: on TollGate AP, no session, funds available");
                        match pay_auto(st, &gateway, None).await {
                            Ok(session) => {
                                *st.session.lock().await = Some(session);
                                let _ = std::fs::write(data_dir().join("last_gateway"), &gateway);
                                tracing::info!(%gateway, "first-connect session paid");
                            }
                            Err(e) => {
                                tracing::warn!(
                                    %gateway,
                                    error = %e,
                                    "first-connect autopay attempt failed (will retry in 30s or pay from the panel)"
                                );
                            }
                        }
                    }
                }
            }
        }
    }
}

async fn status(State(st): State<AppState>) -> Json<Value> {
    let wallets = st.list_mints().await;
    let mut total_balance: u64 = 0;
    let mut mint_balances = Vec::new();
    for (url, wallet) in &wallets {
        let bal: u64 = match wallet {
            Some(w) => w.total_balance().await.unwrap_or_default().into(),
            None => 0,
        };
        total_balance += bal;
        mint_balances.push(json!({ "url": url, "balance_sats": bal, "is_default": *url == st.default_mint }));
    }
    let balance = total_balance;
    // cdk's own view of in-flight operations (melt/send awaiting mint
    // confirmation) — the in-flight leg of the terminal/in-flight state
    // partition the panel needs to render
    let mut pending_sats: u64 = 0;
    for (_, wallet) in &wallets {
        if let Some(w) = wallet {
            pending_sats += u64::from(w.total_pending_balance().await.unwrap_or_default());
        }
    }
    let session = st.session.lock().await.clone();
    let expired_gateway = st.expired_gateway.lock().await.clone();
    // The journal leg of the terminal/in-flight/rollbackable partition
    // (state.rs) — completes the /status state surface pending_sats
    // started, so the panel renders the money phase without inferring it.
    let payment_phase = if st.stash.pending().is_some() {
        state::journal_phase(if st.stash.pending_meta().is_some() {
            state::JournalState::PendingWithMeta
        } else {
            state::JournalState::PendingLegacy
        })
    } else if st.stash.quarantined_count() > 0 {
        state::journal_phase(state::JournalState::Quarantined)
    } else {
        // Nothing pending, nothing quarantined: the terminal compartment
        // of the partition (empty or settled alike).
        state::journal_phase(state::JournalState::Settled)
    }
    .label();
    let wifi_fallback = st.wifi.fallback_profile.lock().unwrap().clone();
    let active_ssid = if st.wifi.enabled {
        wifi::active_ssid().ok().flatten()
    } else {
        None
    };
    let on_tollgate_ap = active_ssid
        .as_deref()
        .map(|s| s.starts_with("TollGate"))
        .unwrap_or(false);
    let gateway_ip = if on_tollgate_ap {
        wifi::gateway_ip().ok().flatten()
    } else {
        None
    };
    let gateway_spent = {
        let gateway = session
            .as_ref()
            .map(|s| s.gateway.clone())
            .or_else(|| expired_gateway.clone());
        match gateway {
            Some(g) => st.gateway_spent.lock().await.get(&g).copied().unwrap_or(0),
            None => 0,
        }
    };
    Json(json!({
        "ok": true,
        "daemon": "cashud",
        "version": env!("CARGO_PKG_VERSION"),
        "mint": st.default_mint,
        "mints": mint_balances,
        "balance_sats": balance,
        "pending_sats": pending_sats,
        "stash_tokens": st.stash.count(),
        "pending_payment": st.stash.pending().is_some(),
        "quarantined_payments": st.stash.quarantined_count(),
        "autopay": st.autopay,
        "autopay_halted": st.autopay_halted.load(Ordering::Relaxed),
        "payment_phase": payment_phase,
        "renewals": st.renewals.load(Ordering::Relaxed),
        "gateway_spent_sats": gateway_spent,
        "blind_payments": st.blind_payments.load(Ordering::Relaxed),
        "open_sagas": st.open_sagas.load(Ordering::Relaxed),
        "wifi": {
            "enabled": st.wifi.enabled,
            "active_ssid": active_ssid,
            "on_tollgate_ap": on_tollgate_ap,
            "gateway_ip": gateway_ip,
            "fallback": wifi_fallback,
            "watchdog": wifi::watchdog_active(),
            "last_action": *st.wifi.last_action.lock().unwrap(),
        },
        "session": session.map(|s| s.to_json()),
        "expired_gateway": expired_gateway,
    }))
}

async fn mint_sats(State(st): State<AppState>, Json(body): Json<Value>) -> Json<Value> {
    match mint_inner(&st, &body).await {
        Ok(v) => v,
        Err(body) => Json(body),
    }
}

/// NUT-09 restore: recover proofs from the mint by scanning the seed's
/// derivation paths (cdk `Wallet::restore`). This is the balance-recovery
/// path the seed phrase promises — until it has been validated for a given
/// mint, wallet copy must not claim the seed restores balance.
async fn restore_wallet(State(st): State<AppState>, Json(body): Json<Value>) -> Json<Value> {
    let mint = body["mint"].as_str();
    let wallet = match st.wallet_for(mint).await {
        Ok(w) => w,
        Err(e) => return Json(json!({ "ok": false, "error": e.to_string() })),
    };
    match wallet.restore().await {
        Ok(restored) => {
            let balance: u64 = wallet.total_balance().await.unwrap_or_default().into();
            tracing::info!(
                mint = %wallet.mint_url,
                unspent = u64::from(restored.unspent),
                spent = u64::from(restored.spent),
                pending = u64::from(restored.pending),
                "wallet restore completed"
            );
            Json(json!({
                "ok": true,
                "restored_sats": u64::from(restored.unspent),
                "spent_sats": u64::from(restored.spent),
                "pending_sats": u64::from(restored.pending),
                "balance_sats": balance,
            }))
        }
        Err(e) => Json(json!({ "ok": false, "error": format!("restore failed: {e}") })),
    }
}

async fn mint_inner(st: &AppState, body: &Value) -> Result<Json<Value>, Value> {
    let wallet = st
        .wallet_for(None)
        .await
        .map_err(|e| json!({ "ok": false, "error": e.to_string() }))?;
    // 0 must be rejected, not clamped to 1 — a surprise 1-sat top-up is
    // worse than a clear error (the API is the boundary; the panel's own
    // guard is not a contract)
    let Some(amount) = body["amount_sats"].as_u64().filter(|&v| v > 0) else {
        bail_json!("amount_sats must be a positive integer");
    };
    let amount = amount.clamp(1, 100_000);

    let quote = wallet
        .mint_quote(
            PaymentMethod::BOLT11,
            Some(cdk::Amount::from(amount)),
            None,
            None,
        )
        .await
        .map_err(|e| mint_error_response("creating a mint quote", &st.default_mint, e))?;

    // FakeWallet mints (the local omarchy-cashu-fakewallet service, rig
    // fakewallets) auto-pay quotes within seconds. The default zoo signet
    // mint settles for real — fund those wallets via the /invoice
    // lifecycle instead; /mint here will time out after 30s unpaid.
    let mut paid = false;
    for _ in 0..30 {
        let s = wallet
            .check_mint_quote_status(&quote.id)
            .await
            .map_err(|e| mint_error_response("checking the quote status", &st.default_mint, e))?;
        if s.state == MintQuoteState::Paid {
            paid = true;
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    if !paid {
        bail_json!(
            "invoice not paid within 30s (fake wallet should pay instantly). request: {}",
            quote.request
        );
    }

    let proofs = wallet
        .mint(&quote.id, SplitTarget::default(), None)
        .await
        .map_err(|e| mint_error_response("minting the paid quote", &st.default_mint, e))?;
    let minted: u64 = proofs.iter().map(|p| u64::from(p.amount)).sum();

    let balance: u64 = wallet.total_balance().await.unwrap_or_default().into();
    Ok(Json(
        json!({ "ok": true, "minted_sats": minted, "balance_sats": balance }),
    ))
}



impl AppState {
    /// Resolve the sat wallet for an optional mint URL (falls back to default).
    /// The upstream repository creates-on-first-use and restores persisted
    /// wallets after restarts.
    pub async fn wallet_for(&self, mint: Option<&str>) -> Result<Wallet, anyhow::Error> {
        let raw = mint.unwrap_or(&self.default_mint);
        let mint_url: MintUrl = raw
            .parse()
            .with_context(|| format!("invalid mint URL {raw}"))?;
        Ok(self
            .repo
            .get_or_create_wallet(mint_url, CurrencyUnit::Sat, None)
            .await?)
    }

    /// Register a mint with the repository (single wallet.db, sat unit).
    /// `fetch_mint_info` both validates reachability and persists the mint
    /// metadata so it survives restarts and shows up in `/mints`.
    pub async fn add_mint(&self, mint_url: &str) -> Result<(), anyhow::Error> {
        let parsed: MintUrl = mint_url
            .parse()
            .with_context(|| format!("invalid mint URL {mint_url}"))?;
        if self.repo.has_wallet(&parsed, &CurrencyUnit::Sat).await {
            return Ok(());
        }
        let wallet = self
            .repo
            .get_or_create_wallet(parsed, CurrencyUnit::Sat, None)
            .await?;
        wallet
            .fetch_mint_info()
            .await
            .context("mint unreachable or returned no info")?;
        tracing::info!(%mint_url, "mint added");
        Ok(())
    }

    /// All persisted mints with their sat balances (one entry per mint).
    pub async fn list_mints(&self) -> Vec<(String, Option<Wallet>)> {
        let mut out = Vec::new();
        if let Ok(mints) = self.localstore.get_mints().await {
            for mint in mints.keys() {
                let url = mint.to_string();
                let wallet = self
                    .repo
                    .get_wallet(mint, &CurrencyUnit::Sat)
                    .await
                    .ok();
                out.push((url, wallet));
            }
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }
}


// ---- multi-mint management ----

async fn mints_list(State(st): State<AppState>) -> Json<Value> {
    let wallets = st.list_mints().await;
    let mut entries = Vec::new();
    for (url, wallet) in wallets {
        let balance: u64 = match &wallet {
            Some(w) => w.total_balance().await.unwrap_or_default().into(),
            None => 0,
        };
        entries.push(json!({
            "url": url,
            "balance_sats": balance,
            "is_default": url == st.default_mint,
        }));
    }
    Json(json!({ "ok": true, "mints": entries, "default_mint": st.default_mint }))
}

async fn mints_add(State(st): State<AppState>, Json(body): Json<Value>) -> Json<Value> {
    let Some(url) = body["url"].as_str() else {
        return Json(json!({ "ok": false, "error": "url required" }));
    };
    match st.add_mint(url).await {
        Ok(()) => {
            let wallets = st.list_mints().await;
            Json(json!({ "ok": true, "total_mints": wallets.len() }))
        }
        Err(e) => Json(json!({ "ok": false, "error": e.to_string() })),
    }
}


async fn qr_frames(State(st): State<AppState>, Json(body): Json<Value>) -> Json<Value> {
    let _ = &st;
    let Some(token) = body["token"].as_str() else {
        return Json(json!({ "ok": false, "error": "token required" }));
    };

    // Small payloads: single static QR (macadamia threshold: 600 chars)
    if token.len() <= 600 {
        let path = format!("/tmp/cashu-qr-static-{}.png", std::process::id());
        let out = std::process::Command::new("qrencode")
            .args(["-t", "PNG", "-s", "10", "-o", &path, token])
            .output();
        match out {
            Ok(o) if o.status.success() => {
                return Json(json!({
                    "ok": true,
                    "animated": false,
                    "frames": [{"path": path, "ur_part": token}],
                    "interval_ms": 0,
                }));
            }
            _ => return Json(json!({ "ok": false, "error": "qrencode failed" })),
        }
    }

    // Large payloads: BC-UR animated QR (all-ecosystem consensus: 150 chars, 250ms)
    let fragment_size = body["fragment_size"].as_u64().unwrap_or(150) as usize;
    let interval_ms = body["interval_ms"].as_u64().unwrap_or(250);

    let mut encoder = match ur::Encoder::bytes(token.as_bytes(), fragment_size) {
        Ok(e) => e,
        Err(e) => return Json(json!({ "ok": false, "error": format!("UR encoder failed: {e}") })),
    };

    // Generate 2x the minimum fragment count for fountain-code redundancy
    let min_fragments = (token.len() + fragment_size - 1) / fragment_size;
    let num_frames = (min_fragments * 2).max(4).min(20); // cap at 20 frames

    let mut frames = Vec::new();
    for i in 0..num_frames {
        let ur_part = match encoder.next_part() {
            Ok(p) => p,
            Err(e) => return Json(json!({ "ok": false, "error": format!("UR part {i} failed: {e}") })),
        };
        let path = format!("/tmp/cashu-qr-frame-{}-{i}.png", std::process::id());
        let out = std::process::Command::new("qrencode")
            .args(["-t", "PNG", "-s", "10", "-o", &path, &ur_part])
            .output();
        match out {
            Ok(o) if o.status.success() => {
                frames.push(json!({ "path": path, "ur_part": ur_part }));
            }
            _ => return Json(json!({ "ok": false, "error": format!("qrencode frame {i} failed") })),
        }
    }

    Json(json!({
        "ok": true,
        "animated": true,
        "frames": frames,
        "interval_ms": interval_ms,
        "fragment_size": fragment_size,
        "total_fragments": min_fragments,
    }))
}

async fn show_seed(State(st): State<AppState>, Json(body): Json<Value>) -> Json<Value> {
    let _ = &st;
    if body["confirm"].as_str() != Some("show seed") {
        return Json(json!({
            "ok": false,
            "error": "send {\"confirm\": \"show seed\"} to reveal the seed",
        }));
    }
    let dir = data_dir();
    let seed_hex = match std::fs::read_to_string(dir.join("seed.hex")) {
        Ok(hex) => hex.trim().to_string(),
        Err(e) => {
            return Json(json!({ "ok": false, "error": format!("seed file unreadable: {e}") }))
        }
    };
    let mnemonic = std::fs::read_to_string(dir.join("mnemonic.txt"))
        .ok()
        .map(|s| s.trim().to_string());
    Json(json!({
        "ok": true,
        "seed_hex": seed_hex,
        "mnemonic": mnemonic,
        "warning": "ecash is bearer money — anyone with this seed or mnemonic controls the wallet. Store offline.",
    }))
}

async fn token_info(State(st): State<AppState>, Json(body): Json<Value>) -> Json<Value> {
    let _ = &st;
    let Some(token) = body["token"].as_str() else {
        return Json(json!({ "ok": false, "error": "token required" }));
    };
    match Token::from_str(token) {
        Ok(t) => Json(json!({
            "ok": true,
            "amount_sats": t.value().map(u64::from).unwrap_or(0),
            "mint": t.mint_url().map(|u| u.to_string()).unwrap_or_default(),
        })),
        Err(e) => Json(json!({ "ok": false, "error": format!("invalid token: {e}") })),
    }
}

async fn send_sats(State(st): State<AppState>, Json(body): Json<Value>) -> Json<Value> {
    // Proof selection races the background stash refill unless serialized
    // on payment_lock like every other proof-consuming path.
    let _guard = st.payment_lock.lock().await;
    // No amount = send the entire balance (export/backup path).
    let mint = body["mint"].as_str();
    if body["amount_sats"].as_u64() == Some(0) {
        return Json(json!({ "ok": false, "error": "amount_sats must be at least 1" }));
    }
    if body["amount_sats"].as_i64().map(|v| v < 0).unwrap_or(false) {
        return Json(json!({ "ok": false, "error": "amount_sats must be positive" }));
    }
    let wallet = match st.wallet_for(mint).await {
        Ok(w) => w,
        Err(e) => return Json(json!({ "ok": false, "error": e.to_string() })),
    };
    let amount = match body["amount_sats"].as_u64() {
        Some(a) => a.clamp(1, 100_000),
        None => {
            let balance: u64 = wallet.total_balance().await.unwrap_or_default().into();
            if balance == 0 {
                return Json(json!({ "ok": false, "error": "nothing to send — balance is zero" }));
            }
            balance
        }
    };
    let prepared = match wallet
        .prepare_send(
            cdk::Amount::from(amount),
            cdk::wallet::SendOptions::default(),
        )
        .await
    {
        Ok(p) => p,
        Err(e) => {
            return Json(
                json!({ "ok": false, "error": format!("prepare_send failed (insufficient funds?): {e}") }),
            )
        }
    };
    match prepared.confirm(None).await {
        Ok(token) => {
            let token_str = token.to_string();
            let balance: u64 = match st.wallet_for(None).await {
                Ok(w) => w.total_balance().await.unwrap_or_default().into(),
                Err(_) => 0,
            };
            Json(
                json!({ "ok": true, "token": token_str, "sent_sats": amount, "balance_sats": balance }),
            )
        }
        Err(e) => Json(json!({ "ok": false, "error": format!("confirm failed: {e}") })),
    }
}

async fn receive_token(State(st): State<AppState>, Json(body): Json<Value>) -> Json<Value> {
    let Some(token_str) = body["token"].as_str() else {
        return Json(json!({ "ok": false, "error": "missing token" }));
    };
    // Which mint is this token from?
    let tk = match Token::from_str(token_str) {
        Ok(t) => t,
        Err(e) => return Json(json!({ "ok": false, "error": format!("invalid token: {e}") })),
    };
    let token_mint = tk.mint_url().map(|u| u.to_string()).unwrap_or_default();
    // Auto-add unknown mints — the upstream repository makes this free
    if !token_mint.is_empty() {
        if let Err(e) = st.add_mint(&token_mint).await {
            return Json(json!({ "ok": false, "error": format!("failed to add mint {token_mint}: {e}") }));
        }
    }
    let wallet = match st.wallet_for(Some(&token_mint)).await { Ok(w) => w, Err(e) => return Json(json!({"ok": false, "error": format!("no wallet for {token_mint}: {e}")})) };
    match wallet
        .receive(token_str, ReceiveOptions::default())
        .await
    {
        Ok(amount) => {
            let received: u64 = amount.into();
            let balance: u64 = match st.wallet_for(None).await {
                Ok(w) => w.total_balance().await.unwrap_or_default().into(),
                Err(_) => 0,
            };
            Json(json!({ "ok": true, "received_sats": received, "balance_sats": balance }))
        }
        Err(e) => Json(json!({ "ok": false, "error": format!("receive failed: {e}") })),
    }
}

async fn history(State(st): State<AppState>) -> Json<Value> {
    let wallet = match st.wallet_for(None).await {
        Ok(w) => w,
        Err(_) => return Json(json!({ "ok": true, "entries": [] })),
    };
    let txs = wallet.list_transactions(None).await.unwrap_or_default();
    let entries: Vec<Value> = txs
        .iter()
        .rev()
        .take(10)
        .map(|t| {
            json!({
                "amount_sats": u64::from(t.amount),
                "direction": format!("{:?}", t.direction),
                "memo": t.memo.clone().unwrap_or_default(),
                "timestamp": t.timestamp,
            })
        })
        .collect();
    Json(json!({ "ok": true, "entries": entries }))
}

// ---- lightning ----

async fn parse_invoice(State(st): State<AppState>, Json(body): Json<Value>) -> Json<Value> {
    let _ = &st;
    let Some(invoice) = body["invoice"].as_str() else {
        return Json(json!({ "ok": false, "error": "invoice required" }));
    };
    match invoice_metadata(invoice) {
        Ok(meta) => Json(json!({
            "ok": true,
            "amount_sats": meta.amount_sats,
            "description": meta.description,
            "expiry_unix": meta.expiry_unix,
            "expired": meta.expired,
        })),
        Err(e) => Json(json!({ "ok": false, "error": e.to_string() })),
    }
}


async fn pay_invoice(State(st): State<AppState>, Json(body): Json<Value>) -> Json<Value> {
    let Some(invoice) = body["invoice"].as_str().map(str::to_string) else {
        return Json(json!({ "ok": false, "error": "invoice required" }));
    };
    let quote_id = body["quote_id"].as_str().map(str::to_string);
    let result = async {
        let meta = invoice_metadata(&invoice)?;
        if meta.expired {
            anyhow::bail!("invoice has expired");
        }
        if meta.amount_sats > st.max_payment_sats {
            anyhow::bail!(
                "invoice is {} sats, above CASHUD_MAX_PAYMENT_SATS={}",
                meta.amount_sats,
                st.max_payment_sats
            );
        }
        let _guard = st.payment_lock.lock().await;
        let wallet = st.wallet_for(None).await?;
        let melted = match quote_id.as_deref() {
            Some(id) => wallet
                .prepare_melt(id, HashMap::new())
                .await
                .context("prepare_melt (preview quote) failed — it may be expired; parse again")?
                .confirm()
                .await
                .context("melt confirm failed")?,
            None => {
                let quote = wallet
                    .melt_quote(PaymentMethod::BOLT11, invoice.clone(), None, None)
                    .await
                    .map_err(|e| mint_op_error("quoting the payment", &st.default_mint, e))?;
                wallet
                    .prepare_melt(&quote.id, HashMap::new())
                    .await
                    .context("prepare_melt failed")?
                    .confirm()
                    .await
                    .context("melt confirm failed")?
            }
        };
        let paid_sats: u64 = melted.amount().into();
        let fee_sats: u64 = melted.fee_paid().into();
        Ok(Json(json!({
            "ok": true,
            "paid_sats": paid_sats,
            "fee_sats": fee_sats,
            "preimage": melted.payment_proof(),
        })))
    };
    match result.await {
        Ok(v) => v,
        Err(e) => {
            let mut body = json!({ "ok": false, "error": format!("{e:#}") });
            attach_mint_failure(&mut body, &e);
            Json(body)
        }
    }
}

async fn melt_quote_preview(State(st): State<AppState>, Json(body): Json<Value>) -> Json<Value> {
    let Some(invoice) = body["invoice"].as_str().map(str::to_string) else {
        return Json(json!({ "ok": false, "error": "invoice required" }));
    };
    let result = async {
        let meta = invoice_metadata(&invoice)?;
        if meta.expired {
            anyhow::bail!("invoice has expired");
        }
        if meta.amount_sats > st.max_payment_sats {
            anyhow::bail!(
                "invoice is {} sats, above CASHUD_MAX_PAYMENT_SATS={}",
                meta.amount_sats,
                st.max_payment_sats
            );
        }
        let w = st.wallet_for(body["mint"].as_str()).await?;
        let raw_mint = body["mint"].as_str().unwrap_or(&st.default_mint);
        let quote = w
            .melt_quote(PaymentMethod::BOLT11, invoice, None, None)
            .await
            .map_err(|e| mint_op_error("quoting the payment", raw_mint, e))?;
        Ok(Json(json!({
            "ok": true,
            "id": quote.id,
            "amount_sats": u64::from(quote.amount),
            "fee_reserve_sats": u64::from(quote.fee_reserve),
            "expiry": quote.expiry,
        })))
    };
    match result.await {
        Ok(v) => v,
        Err(e) => {
            let mut body = json!({ "ok": false, "error": format!("{e:#}") });
            attach_mint_failure(&mut body, &e);
            Json(body)
        }
    }
}

async fn mint_info(
    State(st): State<AppState>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Json<Value> {
    let mint = params.get("mint").cloned();
    let result = async {
        let raw = mint.as_deref().unwrap_or(&st.default_mint);
        let mint_url: MintUrl = raw
            .parse()
            .with_context(|| format!("invalid mint URL {raw}"))?;
        let wallet = st
            .repo
            .get_or_create_wallet(mint_url.clone(), CurrencyUnit::Sat, None)
            .await
            .context("wallet creation failed")?;
        let info = wallet
            .fetch_mint_info()
            .await
            .context("mint unreachable")?
            .context("mint returned no info")?;
        let keysets = wallet
            .mint_connector()
            .get_mint_keysets()
            .await
            .context("keyset fetch failed")?;
        let url = mint_url.to_string();
        let version = info
            .version
            .map(|v| format!("{} {}", v.name, v.version))
            .unwrap_or_default();
        let contact = info
            .contact
            .map(|contacts| {
                contacts
                    .iter()
                    .map(|c| format!("{}:{}", c.method, c.info))
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default();
        Ok::<Json<Value>, anyhow::Error>(Json(json!({
            "ok": true,
            "name": info.name.unwrap_or_default(),
            "pubkey": info.pubkey.map(|p| p.to_hex()).unwrap_or_default(),
            "version": version,
            "description": info.description.unwrap_or_default(),
            "contact": contact,
            "keysets": {
                "total": keysets.keysets.len(),
                "active": keysets.keysets.iter().filter(|k| k.active).count(),
            },
            "url": url.trim_end_matches('/'),
        })))
    };
    match result.await {
        Ok(v) => v,
        Err(e) => Json(json!({ "ok": false, "error": e.to_string() })),
    }
}

async fn invoice_create(State(st): State<AppState>, Json(body): Json<Value>) -> Json<Value> {
    // 0 must be rejected, not clamped to 1 — see mint_inner
    let Some(amount) = body["amount_sats"].as_u64().filter(|&v| v > 0) else {
        return Json(json!({ "ok": false, "error": "amount_sats must be a positive integer" }));
    };
    let amount = amount.clamp(1, 100_000);
    let description = body["description"].as_str().map(String::from);
    let mint = body["mint"].as_str();
    let wallet = match st.wallet_for(mint).await { Ok(w) => w, Err(e) => return Json(json!({"ok": false, "error": e.to_string()})) };
    let raw_mint = mint.unwrap_or(&st.default_mint).to_string();
    match wallet
        .mint_quote(PaymentMethod::BOLT11, Some(cdk::Amount::from(amount)), description, None)
        .await
    {
        Ok(q) => {
            let id = q.id.to_string();
            st.quote_mints.lock().await.insert(id.clone(), raw_mint);
            Json(json!({
                "ok": true,
                "id": id,
                "invoice": q.request.to_string(),
                "amount_sats": amount,
                "expiry": q.expiry,
            }))
        }
        Err(e) => Json(
            mint_error_response("creating the invoice", &raw_mint, e)
        ),
    }
}

/// Resolve which mint a Lightning quote belongs to: explicit request
/// override wins, then the map recorded at invoice creation, then the
/// default mint (pre-multi-mint behavior for quotes from older daemons).
async fn quote_mint(st: &AppState, explicit: Option<&str>, quote_id: &str) -> String {
    if let Some(mint) = explicit {
        return mint.to_string();
    }
    if let Some(mint) = st.quote_mints.lock().await.get(quote_id) {
        return mint.clone();
    }
    st.default_mint.clone()
}

async fn invoice_status(
    State(st): State<AppState>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Json<Value> {
    let Some(id) = params.get("id") else {
        return Json(json!({ "ok": false, "error": "id required" }));
    };
    let mint = quote_mint(&st, params.get("mint").map(String::as_str), id).await;
    let wallet = match st.wallet_for(Some(&mint)).await { Ok(w) => w, Err(_) => return Json(json!({"ok": false, "error": "wallet unavailable"})) };
    match wallet.check_mint_quote_status(id).await {
        Ok(q) => Json(json!({ "ok": true, "paid": q.state == MintQuoteState::Paid })),
        Err(e) => Json(
            mint_error_response("checking the invoice status", &mint, e)
        ),
    }
}

async fn invoice_complete(State(st): State<AppState>, Json(body): Json<Value>) -> Json<Value> {
    let Some(id) = body["quote_id"].as_str() else {
        return Json(json!({ "ok": false, "error": "quote_id required" }));
    };
    let mint = quote_mint(&st, body["mint"].as_str(), id).await;
    let wallet = match st.wallet_for(Some(&mint)).await { Ok(w) => w, Err(_) => return Json(json!({"ok": false, "error": "wallet unavailable"})) };
    match wallet.mint(id, SplitTarget::default(), None).await {
        Ok(proofs) => {
            let minted: u64 = proofs.iter().map(|p| u64::from(p.amount)).sum();
            st.quote_mints.lock().await.remove(id);
            Json(json!({ "ok": true, "minted_sats": minted }))
        }
        Err(e) => Json(
            mint_error_response("completing the paid invoice", &mint, e)
        ),
    }
}

async fn tollgate_pay(State(st): State<AppState>, Json(body): Json<Value>) -> Json<Value> {
    // Explicit gateway wins; otherwise, when we are sitting on a TollGate
    // AP, the default-route gateway IS the tollgate (TIP-03).
    let gateway = match body["gateway"].as_str() {
        Some(g) => g.to_string(),
        None => {
            let on_tg = wifi::active_ssid()
                .ok()
                .flatten()
                .map(|s| s.starts_with("TollGate"))
                .unwrap_or(false);
            if on_tg {
                match wifi::gateway_ip().ok().flatten() {
                    Some(ip) => ip,
                    None => {
                        return Json(
                            json!({ "ok": false, "error": "on a TollGate AP but no default-route gateway found" }),
                        )
                    }
                }
            } else {
                "127.0.0.1".to_string()
            }
        }
    };
    let steps = body["steps"].as_u64();
    match pay_auto(&st, &gateway, steps).await {
        Ok(session) => {
            let out = session.to_json();
            *st.session.lock().await = Some(session);
            *st.expired_gateway.lock().await = None;
            let _ = std::fs::write(data_dir().join("last_gateway"), &gateway);
            Json(json!({ "ok": true, "session": out }))
        }
        Err(e) => Json(json!({ "ok": false, "error": e.to_string() })),
    }
}

async fn stash_status(State(st): State<AppState>) -> Json<Value> {
    Json(json!({
        "ok": true,
        "count": st.stash.count(),
        "target": st.stash_target,
        "pending": st.stash.pending().is_some(),
        "quarantined": st.stash.quarantined_count(),
    }))
}

async fn stash_prime(State(st): State<AppState>, Json(body): Json<Value>) -> Json<Value> {
    let target = body["n"]
        .as_u64()
        .unwrap_or(st.stash_target as u64)
        .clamp(1, 100) as usize;
    let missing = target.saturating_sub(st.stash.count());
    match prime_stash(&st, missing).await {
        Ok(()) => Json(json!({ "ok": true, "primed": missing, "count": st.stash.count() })),
        Err(e) => Json(json!({ "ok": false, "error": e.to_string() })),
    }
}

async fn wifi_status(State(st): State<AppState>) -> Json<Value> {
    if !st.wifi.enabled {
        return Json(
            json!({ "ok": true, "enabled": false, "hint": "set CASHUD_WIFI=1 to enable nmcli integration" }),
        );
    }
    let tollgates = wifi::scan_tollgates()
        .unwrap_or_default()
        .into_iter()
        .map(|s| json!({ "ssid": s.ssid, "signal": s.signal }))
        .collect::<Vec<_>>();
    Json(json!({
        "ok": true,
        "enabled": true,
        "active_ssid": wifi::active_ssid().ok().flatten(),
        "tollgates": tollgates,
        "fallback": *st.wifi.fallback_profile.lock().unwrap(),
        "last_action": *st.wifi.last_action.lock().unwrap(),
    }))
}

async fn wifi_connect(State(st): State<AppState>, Json(body): Json<Value>) -> Json<Value> {
    if !st.wifi.enabled {
        return Json(json!({ "ok": false, "error": "wifi integration disabled (CASHUD_WIFI=1)" }));
    }
    let Some(ssid) = body["ssid"].as_str().map(String::from) else {
        return Json(json!({ "ok": false, "error": "ssid required" }));
    };
    st.wifi.note(&format!("manual connect to '{ssid}'"));
    st.wifi.begin_connection_grace();
    // Deliberate TollGate connection disarms any leftover fallback
    // autoconnect boost — persistent priority-200 state must not roam the
    // client back mid-session (S10).
    if ssid.starts_with("TollGate") {
        wifi::unboost_fallback(&st.wifi);
    }
    match wifi::connect_ssid(&ssid) {
        Ok(()) => Json(json!({ "ok": true, "connected": ssid })),
        Err(e) => Json(json!({ "ok": false, "error": e.to_string() })),
    }
}

async fn wifi_fallback(State(st): State<AppState>, Json(body): Json<Value>) -> Json<Value> {
    if !st.wifi.enabled {
        return Json(json!({ "ok": false, "error": "wifi integration disabled (CASHUD_WIFI=1)" }));
    }
    let profile = match body["profile"].as_str().map(String::from) {
        Some(p) => p,
        None if body["use_active"] == serde_json::Value::Bool(true) => {
            match wifi::active_ssid().ok().flatten() {
                Some(ssid) => ssid,
                None => {
                    return Json(
                        json!({ "ok": false, "error": "not connected to any wifi network" }),
                    )
                }
            }
        }
        None => {
            return Json(json!({ "ok": false, "error": "profile required (or use_active: true)" }))
        }
    };
    match wifi::set_fallback(&st.wifi, &profile) {
        Ok(()) => {
            let _ = std::fs::write(data_dir().join("fallback_profile"), &profile);
            Json(json!({ "ok": true, "fallback": profile }))
        }
        Err(e) => Json(json!({ "ok": false, "error": e.to_string() })),
    }
}

async fn tollgate_session(State(st): State<AppState>) -> Json<Value> {
    let guard = st.session.lock().await;
    match guard.as_ref() {
        Some(s) if s.remaining() > 0 => Json(json!({ "ok": true, "session": s.to_json() })),
        _ => Json(json!({ "ok": true, "session": null })),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let dir = data_dir();
    std::fs::create_dir_all(&dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    }
    let mint_url = std::env::var("CASHUD_MINT").unwrap_or_else(|_| DEFAULT_MINT.to_string());
    let listen = std::env::var("CASHUD_LISTEN").unwrap_or_else(|_| DEFAULT_LISTEN.to_string());
    let canonical_default_mint = mint_url
        .parse::<MintUrl>()
        .map(|m| m.to_string())
        .unwrap_or_else(|_| mint_url.trim_end_matches('/').to_string());

    let autopay = std::env::var("CASHUD_AUTOPAY")
        .map(|v| v != "0")
        .unwrap_or(false);
    let renewal_offset_ms: u64 = std::env::var("CASHUD_RENEWAL_OFFSET_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(15_000);
    let renewal_offset_bytes: u64 = std::env::var("CASHUD_RENEWAL_OFFSET_BYTES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20 * 1024 * 1024);
    let stash_target: usize = std::env::var("CASHUD_STASH_TARGET")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20);
    let max_blind_payments: u64 = std::env::var("CASHUD_MAX_BLIND_PAYMENTS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3)
        .max(1);
    let max_payment_sats: u64 = std::env::var("CASHUD_MAX_PAYMENT_SATS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20)
        .max(1);
    let wifi_enabled = std::env::var("CASHUD_WIFI")
        .map(|v| v != "0")
        .unwrap_or(false);
    let compaction_secs: u64 = std::env::var("CASHUD_COMPACTION_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(60)
        .max(1);
    tracing::info!(
        autopay,
        renewal_offset_ms,
        renewal_offset_bytes,
        stash_target,
        max_blind_payments,
        max_payment_sats,
        wifi_enabled,
        compaction_secs,
        "runtime config"
    );

    let (repo, localstore, _seed) = build_repo(&dir, &canonical_default_mint).await?;
    let default_wallet = repo
        .get_wallet(
            &canonical_default_mint.parse::<MintUrl>().expect("validated above"),
            &CurrencyUnit::Sat,
        )
        .await
        .context("default wallet missing after build")?;
    let balance: u64 = default_wallet.total_balance().await.unwrap_or_default().into();
    tracing::info!(mint = %canonical_default_mint, balance, stash = Stash::new(dir.join("stash.tokens")).count(), "wallet ready");

    let state = AppState {
        repo,
        localstore,
        default_mint: canonical_default_mint.clone(),
        session: Arc::new(Mutex::new(None)),
        expired_gateway: Arc::new(Mutex::new(None)),
        payment_lock: Arc::new(Mutex::new(())),
        stash: Stash::new(dir.join("stash.tokens")),
        wifi: WifiState::new(wifi_enabled),
        autopay,
        autopay_halted: Arc::new(AtomicBool::new(false)),
        renewal_offset_ms,
        renewal_offset_bytes,
        stash_target,
        max_payment_sats,
        renewals: Arc::new(AtomicU64::new(0)),
        blind_payments: Arc::new(AtomicU64::new(0)),
        max_blind_payments,
        refill_active: Arc::new(AtomicBool::new(false)),
        quote_mints: Arc::new(Mutex::new(HashMap::new())),
        open_sagas: Arc::new(AtomicU64::new(0)),
        expired_pay_attempt: Arc::new(std::sync::Mutex::new(None)),
        first_pay_attempt: Arc::new(std::sync::Mutex::new(None)),
        gateway_spent: Arc::new(Mutex::new(HashMap::new())),
    };

    {
        let st = state.clone();
        tokio::spawn(async move { session_supervisor(st).await });
        let st_resolver = state.clone();
        tokio::spawn(async move { pending_payment_resolver(st_resolver).await });
        let st_compactor = state.clone();
        tokio::spawn(async move {
            saga_compactor(st_compactor, Duration::from_secs(compaction_secs)).await
        });
    }

    if wifi_enabled {
        // Restore the persisted fallback designation so /status is
        // truthful after restarts and the watchdog's disarm has its
        // fast path back.
        if let Some(designated) = std::fs::read_to_string(dir.join("fallback_profile"))
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
        {
            *state.wifi.fallback_profile.lock().unwrap() = Some(designated.clone());
            tracing::info!(fallback = %designated, "fallback designation restored");
        }
        // Boot normalization: leftover armed autoconnect boosts are
        // persistent NM state from earlier daemons/stories — disarm now,
        // not on the next TollGate connect (the S10 trap).
        {
            let st = state.clone();
            tokio::spawn(async move {
                wifi::disarm_legacy_boosts(&st.wifi);
            });
        }
        let st = state.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(5)).await;
                if !wifi::watchdog_active() {
                    continue;
                }
                let (alive, session_gateway) = {
                    let guard = st.session.lock().await;
                    (
                        guard.as_ref().is_some_and(|s| s.remaining() > 0),
                        guard.as_ref().map(|s| s.gateway.clone()),
                    )
                };
                // A retained session (even at remaining==0, the renewal
                // boundary) or a journaled pending payment means money is
                // in flight — the gateway-aware decision needs to know.
                let renewal_pending =
                    session_gateway.is_some() || st.stash.pending().is_some();
                // Probe the gateway only when the session isn't vouching
                // (Ok(None) still proves the gateway answers).
                let gateway_answers = if alive {
                    true
                } else {
                    match session_gateway
                        .or_else(|| wifi::gateway_ip().ok().flatten())
                    {
                        Some(gateway) => fetch_usage(&gateway).await.is_ok(),
                        None => false,
                    }
                };
                wifi::check_fallback(&st.wifi, alive, renewal_pending, gateway_answers);
            }
        });
    }

    let app = Router::new()
        .route("/status", get(status))
        .route("/mint", post(mint_sats))
        .route("/restore", post(restore_wallet))
        .route("/send", post(send_sats))
        .route("/receive", post(receive_token))
        .route("/history", get(history))
        .route("/tollgate/pay", post(tollgate_pay))
        .route("/tollgate/session", get(tollgate_session))
        .route("/parse-invoice", post(parse_invoice))
        .route("/melt-quote", post(melt_quote_preview))
        .route("/mints", get(mints_list))
        .route("/mints/add", post(mints_add))
        .route("/qr-frames", post(qr_frames))
        .route("/seed", post(show_seed))
        .route("/token-info", post(token_info))
        .route("/mint-info", get(mint_info))
        .route("/pay-invoice", post(pay_invoice))
        .route("/invoice", post(invoice_create))
        .route("/invoice/status", get(invoice_status))
        .route("/invoice/complete", post(invoice_complete))
        .route("/stash", get(stash_status))
        .route("/stash/prime", post(stash_prime))
        .route("/wifi", get(wifi_status))
        .route("/wifi/connect", post(wifi_connect))
        .route("/wifi/fallback", post(wifi_fallback))
        .with_state(state.clone());

    let addr = parse_listen_spec(&listen)?;
    match addr {
        ListenSpec::Tcp(addr) => {
            let listener = tokio::net::TcpListener::bind(addr).await?;
            tracing::info!("cashud listening on http://{}", listener.local_addr()?);
            axum::serve(listener, app).await?;
        }
        ListenSpec::Unix(path) => {
            // The bridge for frontend-process wallets that reject network
            // listeners on principle (e.g. Chaumarchy's architecture):
            // a filesystem-permissioned socket (0600, user runtime dir)
            // carries the same API with no network stack involved.
            if path.exists() {
                #[cfg(unix)]
                use std::os::unix::fs::FileTypeExt as _;
                let meta = std::fs::symlink_metadata(&path)?;
                if meta.file_type().is_socket() {
                    std::fs::remove_file(&path)
                        .with_context(|| format!("removing stale socket {}", path.display()))?;
                } else {
                    bail!(
                        "CASHUD_LISTEN path {} exists and is not a socket — refusing to remove it",
                        path.display()
                    );
                }
            }
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating socket parent dir {}", parent.display()))?;
            }
            let listener = tokio::net::UnixListener::bind(&path)
                .with_context(|| format!("binding unix socket {}", path.display()))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
            }
            tracing::info!(
                "cashud listening on unix socket {} (0600)",
                path.display()
            );
            axum::serve(listener, app).await?;
        }
    }
    Ok(())
}

/// Where CASHUD_LISTEN points: a TCP address or a unix socket path
/// (`unix:///path/to.sock` or a bare absolute path).
#[derive(Debug, Clone, PartialEq, Eq)]
enum ListenSpec {
    Tcp(SocketAddr),
    Unix(PathBuf),
}

fn parse_listen_spec(spec: &str) -> Result<ListenSpec> {
    let spec = spec.trim();
    if let Some(path) = spec.strip_prefix("unix://") {
        if path.is_empty() {
            bail!("CASHUD_LISTEN unix:// needs a path");
        }
        return Ok(ListenSpec::Unix(PathBuf::from(path)));
    }
    if spec.starts_with('/') {
        return Ok(ListenSpec::Unix(PathBuf::from(spec)));
    }
    let addr: SocketAddr = spec
        .parse()
        .with_context(|| format!("bad CASHUD_LISTEN {spec:?} — use ip:port, unix:///path, or /abs/path"))?;
    Ok(ListenSpec::Tcp(addr))
}

#[cfg(test)]
mod seed_tests {
    use super::*;

    fn test_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("omarchy-cashu-seed-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn fresh_wallet_gets_bip39_mnemonic_and_matching_seed_hex() {
        let dir = test_dir("fresh");
        let (seed, mnemonic) = load_or_create_seed(&dir, false).unwrap();
        assert!(mnemonic.is_some(), "new wallets must record a mnemonic");
        let words = mnemonic.unwrap();
        assert_eq!(words.split_whitespace().count(), 12);
        let hex = std::fs::read_to_string(dir.join("seed.hex")).unwrap();
        assert_eq!(parse_seed_hex(&hex).unwrap(), seed);
        assert!(std::fs::metadata(dir.join("mnemonic.txt")).is_ok());
    }

    #[test]
    fn existing_seed_hex_wins_and_reports_no_mnemonic() {
        let dir = test_dir("legacy");
        let (original, _) = load_or_create_seed(&dir, false).unwrap();
        std::fs::write(
            dir.join("mnemonic.txt"),
            "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about\n",
        )
        .unwrap();
        let (seed, mnemonic) = load_or_create_seed(&dir, true).unwrap();
        assert_eq!(seed, original);
        assert!(mnemonic.is_none(), "legacy wallets have no derivable mnemonic");
    }

    #[test]
    fn mnemonic_file_recovers_seed_hex() {
        let dir = test_dir("recover");
        let mnemonic = bip39::Mnemonic::from_entropy(&[7u8; 16]).unwrap();
        let words = mnemonic.words().collect::<Vec<_>>().join(" ");
        std::fs::write(dir.join("mnemonic.txt"), &words).unwrap();
        let (seed, recovered) = load_or_create_seed(&dir, false).unwrap();
        assert_eq!(seed, mnemonic.to_seed_normalized(""));
        assert_eq!(recovered.as_deref(), Some(words.as_str()));
        assert!(dir.join("seed.hex").exists(), "seed.hex is rewritten from the mnemonic");
    }

    #[test]
    fn refuses_to_regenerate_seed_over_existing_wallet() {
        let dir = test_dir("orphan-guard");
        std::fs::write(dir.join("wallet.db"), b"").unwrap();
        let err = load_or_create_seed(&dir, true).unwrap_err();
        assert!(err.to_string().contains("refusing"));
    }

    #[test]
    fn malformed_seed_hex_is_refused() {
        let dir = test_dir("malformed");
        std::fs::write(dir.join("seed.hex"), "nothex").unwrap();
        let err = load_or_create_seed(&dir, true).unwrap_err();
        assert!(err.to_string().contains("malformed"));
    }
}

#[cfg(test)]
mod expiry_tests {
    use super::*;

    #[test]
    fn halted_autopay_drops_expired_session() {
        // issue #5: Ok(None) + autopay_halted + remaining_ms==0 stranded the
        // client — the halt latch must treat expiry as terminal.
        assert!(matches!(
            expiry_decision(true, true, true),
            ExpiryAction::Drop
        ));
    }

    #[test]
    fn manual_mode_drops_expired_session() {
        assert!(matches!(
            expiry_decision(false, false, true),
            ExpiryAction::Drop
        ));
        assert!(matches!(
            expiry_decision(false, true, true),
            ExpiryAction::Drop
        ));
    }

    #[test]
    fn live_autopay_renews_expired_sessions() {
        assert!(matches!(
            expiry_decision(true, false, true),
            ExpiryAction::Renew
        ));
    }

    // ---- the expired-resume middle path (estate-lead GO 2026-09-30) ----

    #[test]
    fn expired_gateway_autopays_when_stash_funded() {
        // A previously-paid gateway (expired marker set) renews itself
        // when autopay is on and the stash holds exact-change tokens —
        // no user click needed. Wallet balance funds it equally.
        assert!(expired_gateway_autopays(true, 5, 0));
        assert!(expired_gateway_autopays(true, 0, 42));
    }

    #[test]
    fn expired_gateway_autopay_is_gated_by_autopay_and_funds() {
        // Manual mode keeps the marker-only behavior (first connect and
        // manual renewals stay user-driven); no funds means nothing to
        // attempt — the panel affordance is the only path.
        assert!(!expired_gateway_autopays(false, 5, 42));
        assert!(!expired_gateway_autopays(true, 0, 0));
    }

    #[test]
    fn first_connect_autopays_when_on_ap_funded_and_opted_in() {
        // Sitting on a TollGate AP with funds and autopay opted in: pay
        // without a click (laptop-lane request 2026-09-30). Stash tokens
        // or wallet balance both qualify.
        assert!(first_connect_autopays(true, true, false, 5, 0));
        assert!(first_connect_autopays(true, true, false, 0, 42));
    }

    #[test]
    fn first_connect_autopay_requires_live_ap_and_opt_in_and_health() {
        // Never fire for a remembered gateway while off its AP (that
        // would pay for wifi we are not using), without the explicit
        // CASHUD_AUTOPAY=1 opt-in, when halted, or with no funds.
        assert!(!first_connect_autopays(false, true, false, 5, 42));
        assert!(!first_connect_autopays(true, false, false, 5, 42));
        assert!(!first_connect_autopays(true, true, true, 5, 42));
        assert!(!first_connect_autopays(true, true, false, 0, 0));
    }

    #[test]
    fn stash_tokens_are_exact_change_for_cost_one_gateways() {
        // The offline path pays cost_sats=1 gateways from the stash with
        // no mint roundtrip (S10) — the checkout branch and prime_stash
        // both key on this constant.
        assert_eq!(STASH_TOKEN_SATS, 1);
    }

    #[test]
    fn selection_failure_at_healthy_balance_is_a_denomination_mismatch() {
        // The 4071-sat field case: cdk reports "InsufficientFunds" for
        // cannot-make-exact-change too — classify by balance, not by the
        // error string alone.
        let err = cdk::error::Error::InsufficientFunds;
        assert!(selection_failure_is_denomination_mismatch(&err, 4071, 1));
        assert!(selection_failure_is_denomination_mismatch(&err, 4071, 2));
        // Genuinely broke: the plain insufficient-funds copy is honest.
        assert!(!selection_failure_is_denomination_mismatch(&err, 0, 1));
        assert!(!selection_failure_is_denomination_mismatch(&err, 1, 2));
        // Other errors never take the denomination copy.
        assert!(!selection_failure_is_denomination_mismatch(
            &cdk::error::Error::Timeout,
            4071,
            1
        ));
    }

    #[test]
    fn unobserved_session_is_governed_by_the_blind_cap_not_expiry() {
        assert!(matches!(
            expiry_decision(true, true, false),
            ExpiryAction::Ignore
        ));
        assert!(matches!(
            expiry_decision(true, false, false),
            ExpiryAction::Ignore
        ));
        assert!(matches!(
            expiry_decision(false, false, false),
            ExpiryAction::Ignore
        ));
    }
}

#[cfg(test)]
mod fee_tests {
    use super::*;

    #[test]
    fn exact_cost_at_zero_fee_is_accepted() {
        // The zero-fee path: token value == cost.
        assert!(token_covers_payment(1, 1, Some(0)).is_ok());
        assert!(token_covers_payment(5, 5, Some(0)).is_ok());
    }

    #[test]
    fn under_payment_is_rejected_regardless_of_fee_knowledge() {
        let err = token_covers_payment(1, 2, Some(0)).unwrap_err();
        assert!(err.contains("under-covered"), "{err}");
        let err = token_covers_payment(1, 2, None).unwrap_err();
        assert!(err.contains("under-covered"), "{err}");
    }

    #[test]
    fn cost_plus_fee_is_accepted_at_fee_bearing_mints() {
        // ppk=1000 → 1 sat per proof; a single-proof token carries cost+1.
        assert!(token_covers_payment(2, 1, Some(1)).is_ok());
        assert!(token_covers_payment(3, 2, Some(1)).is_ok());
    }

    #[test]
    fn over_payment_beyond_the_fee_is_rejected() {
        let err = token_covers_payment(3, 1, Some(1)).unwrap_err();
        assert!(err.contains("beyond cost"), "{err}");
        assert!(err.contains("wrong token"), "{err}");
    }

    #[test]
    fn unknown_fee_degrades_to_the_under_guard_only() {
        // Offline stash renewal with a dead mint / V3 token: over-pay is
        // tolerated (the token came from our own fee-inclusive primes).
        assert!(token_covers_payment(2, 1, None).is_ok());
        assert!(token_covers_payment(9, 1, None).is_ok());
    }

    #[test]
    fn v4_token_proof_counts_feed_the_fee_computation() {
        use cdk::nuts::{Id, Proof};
        use cdk::secret::Secret;
        use std::str::FromStr;
        let keyset = Id::from_str("0094d5a774c40a32").unwrap();
        let mint: MintUrl = "https://test-mint.example.com".parse().unwrap();
        let proofs: Vec<Proof> = (0..3)
            .map(|_| Proof {
                amount: cdk::Amount::from(1u64),
                keyset_id: keyset,
                secret: Secret::generate(),
                c: cdk::dhke::hash_to_curve(b"fee-fixture").unwrap(),
                witness: None,
                dleq: None,
                p2pk_e: None,
            })
            .collect();
        let token = Token::new(mint, proofs, None, cdk::nuts::CurrencyUnit::Sat);
        // A v1 (00-prefixed) short id reconstructs without any keyset info.
        let counts = token_proof_counts(&token, &[]).expect("v1 short id resolves standalone");
        assert_eq!(counts.get(&keyset), Some(&3), "one group of three proofs");
        // V3 tokens cannot be counted without mint keyset info.
        let v3 = Token::TokenV3(cdk::nuts::nut00::TokenV3 {
            token: vec![],
            memo: None,
            unit: None,
        });
        assert!(token_proof_counts(&v3, &[]).is_none());
    }
}

#[cfg(test)]
mod mint_error_tests {
    use super::*;

    fn transport_502() -> anyhow::Error {
        // shaped like cdk's reqwest transport errors: a bare Display with
        // the HTTP code and no mint, no operation
        anyhow::anyhow!("Http transport error Some(502): ")
    }

    fn rendered(err: &anyhow::Error) -> String {
        // how handlers surface the chain — the hermetic top-up contract
        format!("{err:#}")
    }

    #[test]
    fn mint_op_error_names_mint_operation_and_cause() {
        // the issue-#invoice ux lesson: during an upstream outage the panel
        // showed "Http transport error Some(502): " — no mint, no action.
        let err = mint_op_error(
            "creating the invoice",
            "https://testnut.cashu.space",
            transport_502(),
        );
        let err = rendered(&err);
        assert!(err.contains("testnut.cashu.space"), "names the mint: {err}");
        assert!(err.contains("creating the invoice"), "names the op: {err}");
        assert!(err.contains("502"), "keeps the causal chain: {err}");
        assert!(err.contains("unreachable"), "says what to do about it: {err}");
    }

    #[test]
    fn mint_op_error_keeps_nested_context_chain() {
        let inner = transport_502().context("mint_quote failed");
        let err = mint_op_error("quoting the payment", "http://127.0.0.1:9", inner);
        let err = rendered(&err);
        assert!(err.contains("mint_quote failed"), "chain preserved: {err}");
        assert!(err.contains("127.0.0.1:9"), "local mints named too: {err}");
    }

    // ---- the transport / HTTP-status / protocol split ---------------

    fn cdk_err(e: cdk::error::Error) -> anyhow::Error {
        mint_op_error("creating the invoice", "https://mint.example", e)
    }

    #[test]
    fn transport_failures_classify_as_unreachable() {
        // "balance is safe on this computer" is claimable ONLY here.
        let refused = cdk::error::Error::HttpError(None, "Connection refused".to_string());
        assert_eq!(
            classify_mint_failure(&cdk_err(refused)),
            Some(MintFailureKind::Unreachable)
        );
        assert_eq!(
            classify_mint_failure(&cdk_err(cdk::error::Error::Timeout)),
            Some(MintFailureKind::Unreachable)
        );
    }

    #[test]
    fn http_statuses_classify_by_class() {
        let not_found = cdk::error::Error::HttpError(Some(404), "Not Found".to_string());
        assert_eq!(
            classify_mint_failure(&cdk_err(not_found)),
            Some(MintFailureKind::HttpClientError(404))
        );
        let bad_gateway = cdk::error::Error::HttpError(Some(502), "Bad Gateway".to_string());
        assert_eq!(
            classify_mint_failure(&cdk_err(bad_gateway)),
            Some(MintFailureKind::HttpServerError(502))
        );
    }

    #[test]
    fn other_cdk_errors_classify_as_protocol() {
        let spent = cdk::error::Error::TokenAlreadySpent;
        assert_eq!(
            classify_mint_failure(&cdk_err(spent)),
            Some(MintFailureKind::Protocol)
        );
    }

    #[test]
    fn non_mint_errors_have_no_kind() {
        let err = anyhow::anyhow!("amount_sats must be a positive integer");
        assert_eq!(classify_mint_failure(&err), None);
    }

    #[test]
    fn response_carries_kind_and_status_fields() {
        let body = mint_error_response(
            "creating the invoice",
            "https://mint.example",
            cdk::error::Error::HttpError(Some(502), "Bad Gateway".to_string()),
        );
        assert_eq!(body["mint_failure_kind"], json!("http_server_error"));
        assert_eq!(body["http_status"], json!(502));
        assert!(body["error"].as_str().unwrap().contains("502"));
        // The string contract is unchanged in shape.
        assert!(body["error"].as_str().unwrap().starts_with("mint https://mint.example"));

        let body = mint_error_response(
            "creating the invoice",
            "https://mint.example",
            cdk::error::Error::HttpError(None, "Connection refused".to_string()),
        );
        assert_eq!(body["mint_failure_kind"], json!("unreachable"));
        assert!(body.get("http_status").is_none());
    }

    #[test]
    fn response_without_cdk_error_gets_no_kind_field() {
        let body = mint_error_response("creating the invoice", "https://mint.example", anyhow::anyhow!("local failure"));
        assert!(body.get("mint_failure_kind").is_none());
        assert!(body.get("http_status").is_none());
    }
}

#[cfg(test)]
mod listen_spec_tests {
    use super::*;

    #[test]
    fn tcp_address_parses() {
        assert_eq!(
            parse_listen_spec("127.0.0.1:3939").unwrap(),
            ListenSpec::Tcp("127.0.0.1:3939".parse().unwrap())
        );
    }

    #[test]
    fn unix_scheme_parses() {
        assert_eq!(
            parse_listen_spec("unix:///run/user/1000/cashud.sock").unwrap(),
            ListenSpec::Unix("/run/user/1000/cashud.sock".into())
        );
    }

    #[test]
    fn bare_absolute_path_parses_as_unix() {
        assert_eq!(
            parse_listen_spec("/tmp/cashud.sock").unwrap(),
            ListenSpec::Unix("/tmp/cashud.sock".into())
        );
    }

    #[test]
    fn empty_unix_path_is_rejected() {
        assert!(parse_listen_spec("unix://").is_err());
    }

    #[test]
    fn garbage_is_rejected() {
        assert!(parse_listen_spec("not an address").is_err());
        assert!(parse_listen_spec("tcp://weird").is_err());
    }
}
