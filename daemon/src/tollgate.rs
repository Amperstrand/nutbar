//! Current TollGate HTTP client.
//!
//! The March 2026 protocol sends the Cashu bearer token directly as
//! `text/plain`. Session lifetime comes from `GET /usage`, not a client clock:
//! renewal responses contain the gateway's total allotment.

use anyhow::{bail, Context, Result};
use serde_json::Value;

const PORT: u16 = 2121;
const HTTP_TIMEOUT_SECS: u64 = 5;
const PAYMENT_TIMEOUT_SECS: u64 = 30;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PricingOption {
    pub price_per_step: u64,
    pub price_unit: String,
    pub mint_url: String,
    pub min_steps: u64,
}

#[derive(Debug, Clone)]
pub struct TollGateAd {
    pub metric: String,
    pub step_size: u64,
    pub pricing: Vec<PricingOption>,
    pub tollgate_pubkey: String,
}

impl TollGateAd {
    pub fn offer_for_mint(&self, mint_url: &str) -> Result<&PricingOption> {
        self.pricing
            .iter()
            .find(|offer| canonical_url(&offer.mint_url) == canonical_url(mint_url))
            .with_context(|| {
                let accepted = self
                    .pricing
                    .iter()
                    .map(|offer| offer.mint_url.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("gateway does not accept wallet mint {mint_url}; accepted: {accepted}")
            })
    }
}

#[derive(Debug, Clone)]
pub struct ActiveSession {
    pub session_id: String,
    pub allotment: u64,
    pub used: u64,
    pub metric: String,
    pub cost_sats: u64,
    pub gateway: String,
    pub usage_observed: bool,
    /// The allotment seen immediately before this payment. The payment is
    /// only confirmed client-side when `/usage` grows beyond this baseline.
    pub credit_baseline: Option<u64>,
    /// The allotment the gateway's own 1022 response advertised for this
    /// payment. Real gateways renew by *replacing* the session with a fresh
    /// (possibly fee-diminished) allotment instead of growing the old one,
    /// so `credit_baseline` growth never happens there — but a `/usage`
    /// poll delivering at least what the 1022 promised is observable proof
    /// the gateway mapped the credit to this client (issue #5).
    pub promised_allotment: Option<u64>,
    pub credit_observed: bool,
    optimistic_ends_at_ms: Option<u64>,
}

impl ActiveSession {
    pub fn from_usage(gateway: &str, metric: &str, cost_sats: u64, usage: Usage) -> Self {
        let mut session = Self {
            session_id: "recovered-from-usage".to_string(),
            allotment: usage.allotment,
            used: usage.used,
            metric: metric.to_string(),
            cost_sats,
            gateway: gateway.to_string(),
            usage_observed: true,
            credit_baseline: None,
            promised_allotment: None,
            credit_observed: true,
            optimistic_ends_at_ms: None,
        };
        session.update_usage(usage);
        session
    }

    /// Remaining credit in the session's metric. For millisecond sessions
    /// this decays on the wall clock whenever a deadline is known: HTTP-03
    /// makes the gateway authoritative, but the client only learns usage on
    /// successful polls — a gateway that stops answering (or reports
    /// expiry) would otherwise freeze `remaining` at a stale positive value
    /// and pin the session, the bar countdown, and the wifi fallback
    /// watchdog open forever. Accepted tradeoff: if a gateway pauses ms
    /// metering, `remaining` decays for at most one poll interval and
    /// self-corrects on the next successful poll, which re-arms the
    /// deadline from fresh gateway data.
    pub fn remaining(&self) -> u64 {
        if self.metric == "milliseconds" {
            if let Some(deadline) = self.optimistic_ends_at_ms {
                return deadline.saturating_sub(now_ms());
            }
            return self.allotment.saturating_sub(self.used);
        }
        self.allotment.saturating_sub(self.used)
    }

    pub fn update_usage(&mut self, usage: Usage) {
        if !self.credit_observed {
            // Add-semantics gateways (mockgate): the payment grows /usage
            // beyond the pre-payment baseline.
            let grew_past_baseline = self
                .credit_baseline
                .map(|baseline| usage.allotment > baseline)
                .unwrap_or(true);
            // Replace-semantics gateways (real tollgate, issue #5): the
            // renewal swaps in a fresh allotment that never exceeds the old
            // baseline, and mint swap fees can shave it below the advertised
            // increment. The gateway delivering the allotment its own 1022
            // promised is still proof of credit — a fee shortfall is normal
            // on real gateways and must not count as an uncredited payment.
            let delivered_promise = self
                .promised_allotment
                .map(|promised| usage.allotment >= promised)
                .unwrap_or(false);
            if grew_past_baseline || delivered_promise {
                self.credit_observed = true;
            }
        }
        self.used = usage.used;
        self.allotment = usage.allotment;
        self.usage_observed = true;
        // compute from the raw fields, not remaining(): remaining() decays
        // against the stale deadline this very call is replacing
        self.optimistic_ends_at_ms = (self.metric == "milliseconds")
            .then(|| now_ms().saturating_add(self.allotment.saturating_sub(self.used)));
    }

    pub fn to_json(&self) -> Value {
        let remaining = self.remaining();
        serde_json::json!({
            "session_id": self.session_id,
            "allotment": self.allotment,
            "used": self.used,
            "metric": self.metric,
            "cost_sats": self.cost_sats,
            "gateway": self.gateway,
            "usage_observed": self.usage_observed,
            "credit_observed": self.credit_observed,
            "remaining": remaining,
            "remaining_ms": if self.metric == "milliseconds" { Some(remaining) } else { None },
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Usage {
    pub used: u64,
    pub allotment: u64,
}

/// TollGate 0.6 `GET /session-state` — distinguishes a client that never
/// paid from one whose paid session ran out. `/usage` answers `-1/-1` for
/// both, so only this endpoint can drive a "renew" affordance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    None,
    Active,
    Expired,
}

#[derive(Debug)]
pub enum PayOutcome {
    Session(Box<ActiveSession>),
    Spent,
    /// The gateway explicitly says the outcome may already have been applied.
    /// Retrying would risk a double spend, so the token must be quarantined.
    OutcomeUnknown(String),
    /// A definitive terminal rejection. The token is retained for manual
    /// recovery unless it is known to be spent.
    Terminal {
        code: String,
        message: String,
    },
    /// A non-terminal rejection. Preserve and reuse the pending token.
    Rejected {
        code: Option<String>,
        message: String,
    },
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn gateway_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(HTTP_TIMEOUT_SECS))
        .build()
        .expect("reqwest client")
}

fn base_url(gateway: &str) -> String {
    if gateway.contains(':') {
        format!("http://{gateway}")
    } else {
        format!("http://{gateway}:{PORT}")
    }
}

fn canonical_url(url: &str) -> String {
    url.trim().trim_end_matches('/').to_ascii_lowercase()
}

/// Best-effort captive-portal request. NoDogSplash must have observed the
/// client before it can authorize the socket-derived MAC.
pub async fn register_with_portal(gateway: &str) {
    if gateway.contains(':') {
        return; // test gateway with an explicit port
    }
    let _ = gateway_client()
        .get(format!("http://{gateway}/"))
        .timeout(std::time::Duration::from_secs(3))
        .send()
        .await;
}

pub async fn fetch_advertisement(gateway: &str) -> Result<TollGateAd> {
    let ad: Value = gateway_client()
        .get(format!("{}/", base_url(gateway)))
        .timeout(std::time::Duration::from_secs(HTTP_TIMEOUT_SECS))
        .send()
        .await
        .context("gateway unreachable")?
        .error_for_status()
        .context("gateway returned error status")?
        .json()
        .await
        .context("gateway advertisement is not valid JSON")?;
    parse_advertisement(&ad)
}

pub fn parse_advertisement(ad: &Value) -> Result<TollGateAd> {
    if ad["kind"].as_u64() != Some(10021) {
        bail!("expected kind 10021 advertisement, got {:?}", ad["kind"]);
    }
    let tollgate_pubkey = ad["pubkey"]
        .as_str()
        .context("advertisement missing pubkey")?
        .to_string();
    if tollgate_pubkey.len() != 64 || !tollgate_pubkey.chars().all(|c| c.is_ascii_hexdigit()) {
        bail!("advertisement pubkey is not 32-byte hex");
    }

    let mut metric = None;
    let mut step_size = 0u64;
    let mut pricing = Vec::new();
    for tag in ad["tags"]
        .as_array()
        .context("advertisement missing tags")?
    {
        let Some(parts) = tag.as_array() else {
            continue;
        };
        match parts.first().and_then(|value| value.as_str()).unwrap_or("") {
            "metric" => metric = parts.get(1).and_then(|v| v.as_str()).map(String::from),
            "step_size" => {
                step_size = parts
                    .get(1)
                    .and_then(|v| v.as_str())
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);
            }
            "price_per_step" if parts.len() >= 5 => {
                let payment_method = parts.get(1).and_then(|v| v.as_str()).unwrap_or("");
                let price_per_step = parts
                    .get(2)
                    .and_then(|v| v.as_str())
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);
                let price_unit = parts.get(3).and_then(|v| v.as_str()).unwrap_or("");
                let mint_url = parts.get(4).and_then(|v| v.as_str()).unwrap_or("");
                let min_steps = parts
                    .get(5)
                    .and_then(|v| v.as_str())
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(1);
                if payment_method == "cashu"
                    && matches!(price_unit, "sat" | "sats")
                    && price_per_step > 0
                    && min_steps > 0
                    && !mint_url.is_empty()
                {
                    pricing.push(PricingOption {
                        price_per_step,
                        price_unit: price_unit.to_string(),
                        mint_url: mint_url.to_string(),
                        min_steps,
                    });
                }
            }
            _ => {}
        }
    }

    let metric = metric.context("advertisement missing metric tag")?;
    if !matches!(metric.as_str(), "milliseconds" | "bytes") {
        bail!("unsupported gateway metric {metric:?}");
    }
    if step_size == 0 {
        bail!("advertisement has zero step_size");
    }
    if pricing.is_empty() {
        bail!("advertisement has no valid Cashu/sat pricing option");
    }

    Ok(TollGateAd {
        metric,
        step_size,
        pricing,
        tollgate_pubkey,
    })
}

pub fn payment_cost(ad: &TollGateAd, mint_url: &str, steps: Option<u64>) -> Result<(u64, u64)> {
    let offer = ad.offer_for_mint(mint_url)?;
    let steps = steps.unwrap_or(offer.min_steps).max(offer.min_steps);
    let cost = steps
        .checked_mul(offer.price_per_step)
        .context("payment cost overflow")?;
    Ok((steps, cost))
}

pub async fn fetch_usage(gateway: &str) -> Result<Option<Usage>> {
    let body = gateway_client()
        .get(format!("{}/usage", base_url(gateway)))
        .timeout(std::time::Duration::from_secs(HTTP_TIMEOUT_SECS))
        .send()
        .await
        .context("gateway /usage unreachable")?
        .error_for_status()
        .context("gateway /usage returned error status")?
        .text()
        .await
        .context("could not read /usage response")?;
    parse_usage(&body)
}

pub fn parse_usage(body: &str) -> Result<Option<Usage>> {
    let (used, allotment) = body
        .trim()
        .split_once('/')
        .context("unexpected /usage response (expected used/allotment)")?;
    let used: i128 = used.parse().context("invalid used value from /usage")?;
    let allotment: i128 = allotment
        .parse()
        .context("invalid allotment value from /usage")?;
    if used == -1 && allotment == -1 {
        return Ok(None);
    }
    if used < 0 || allotment < 0 {
        bail!("negative /usage values: {used}/{allotment}");
    }
    Ok(Some(Usage {
        used: u64::try_from(used).context("usage overflow")?,
        allotment: u64::try_from(allotment).context("allotment overflow")?,
    }))
}

pub fn parse_whoami_mac(body: &str) -> Result<String> {
    let line = body.trim();
    let (_, mac) = line
        .split_once('=')
        .context("whoami response is not key=value")?;
    let mac = mac.trim();
    let octets = mac.split(':').filter(|o| o.len() == 2).count();
    if octets != 6 {
        bail!("whoami returned a non-MAC value {mac:?}");
    }
    Ok(mac.to_string())
}

pub fn parse_session_state(body: &str) -> Result<SessionState> {
    let v: Value =
        serde_json::from_str(body).context("session-state response is not JSON")?;
    match v["state"].as_str() {
        Some("none") => Ok(SessionState::None),
        Some("active") => Ok(SessionState::Active),
        Some("expired") => Ok(SessionState::Expired),
        other => bail!("unknown session-state {other:?}"),
    }
}

/// HTTP-02: the gateway names us by the connection's socket MAC.
pub async fn fetch_client_mac(gateway: &str) -> Result<String> {
    let body = gateway_client()
        .get(format!("{}/whoami", base_url(gateway)))
        .timeout(std::time::Duration::from_secs(HTTP_TIMEOUT_SECS))
        .send()
        .await
        .context("gateway /whoami unreachable")?
        .error_for_status()
        .context("gateway /whoami returned error status")?
        .text()
        .await
        .context("could not read /whoami response")?;
    parse_whoami_mac(&body)
}

/// TollGate 0.6 `GET /session-state?mac=…`. Pre-0.6 gateways 404 — callers
/// must treat errors as "unknown" and keep their /usage-based behavior.
pub async fn fetch_session_state(gateway: &str, mac: &str) -> Result<SessionState> {
    let query_mac = mac.replace(':', "%3A");
    let body = gateway_client()
        .get(format!("{}/session-state?mac={query_mac}", base_url(gateway)))
        .timeout(std::time::Duration::from_secs(HTTP_TIMEOUT_SECS))
        .send()
        .await
        .context("gateway /session-state unreachable")?
        .error_for_status()
        .context("gateway /session-state returned error status")?
        .text()
        .await
        .context("could not read /session-state response")?;
    parse_session_state(&body)
}

/// Send a journaled raw bearer token. A transport error is deliberately
/// returned as an error: the caller must keep the pending token and retry the
/// same token, never mint a replacement.
pub async fn post_token(
    gateway: &str,
    token: &str,
    ad: &TollGateAd,
    cost: u64,
) -> Result<PayOutcome> {
    register_with_portal(gateway).await;
    let response = gateway_client()
        .post(format!("{}/", base_url(gateway)))
        .timeout(std::time::Duration::from_secs(PAYMENT_TIMEOUT_SECS))
        .header("Content-Type", "text/plain")
        .body(token.to_string())
        .send()
        .await
        .context("payment POST failed; pending token preserved for retry")?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    let body: Value = serde_json::from_str(&text).with_context(|| {
        format!(
            "gateway response not JSON; pending token preserved: {}",
            text.chars().take(160).collect::<String>()
        )
    })?;

    if status.is_success() && body["kind"].as_u64() == Some(1022) {
        return session_from_response(&body, ad, gateway, cost)
            .map(|session| PayOutcome::Session(Box::new(session)));
    }

    let (code, message) = notice_details(&body);
    match code.as_deref() {
        Some("payment-error-token-spent") => Ok(PayOutcome::Spent),
        Some("payment-outcome-unknown") => Ok(PayOutcome::OutcomeUnknown(message)),
        Some(
            "payment-error-below-swap-fee"
            | "payment-error-invalid-token"
            | "payment-error-keyset-expired"
            | "payment-error-mint-not-accepted",
        ) => Ok(PayOutcome::Terminal {
            code: code.unwrap(),
            message,
        }),
        _ => Ok(PayOutcome::Rejected { code, message }),
    }
}

fn notice_details(body: &Value) -> (Option<String>, String) {
    let mut code = None;
    let mut message = body["content"].as_str().unwrap_or("").to_string();
    if let Some(tags) = body["tags"].as_array() {
        for tag in tags {
            let Some(parts) = tag.as_array() else {
                continue;
            };
            match parts.first().and_then(|value| value.as_str()).unwrap_or("") {
                "code" => code = parts.get(1).and_then(|v| v.as_str()).map(String::from),
                "message" => {
                    if let Some(value) = parts.get(1).and_then(|v| v.as_str()) {
                        message = value.to_string();
                    }
                }
                _ => {}
            }
        }
    }
    if message.is_empty() {
        message = "gateway rejected payment".to_string();
    }
    (code, message)
}

fn session_from_response(
    session: &Value,
    ad: &TollGateAd,
    gateway: &str,
    cost: u64,
) -> Result<ActiveSession> {
    if session["kind"].as_u64() != Some(1022) {
        bail!("expected kind 1022 session, got {:?}", session["kind"]);
    }
    let mut allotment = None;
    let mut metric = None;
    for tag in session["tags"].as_array().context("session missing tags")? {
        let Some(parts) = tag.as_array() else {
            continue;
        };
        match parts.first().and_then(|value| value.as_str()).unwrap_or("") {
            "allotment" => {
                allotment = parts
                    .get(1)
                    .and_then(|v| v.as_str())
                    .and_then(|s| s.parse::<u64>().ok());
            }
            "metric" => metric = parts.get(1).and_then(|v| v.as_str()),
            _ => {}
        }
    }
    let allotment = allotment.context("session missing or invalid allotment")?;
    if allotment == 0 {
        bail!("session has zero allotment");
    }
    if metric.is_some_and(|value| value != ad.metric) {
        bail!("session metric does not match advertisement");
    }
    let optimistic_ends_at_ms =
        (ad.metric == "milliseconds").then(|| now_ms().saturating_add(allotment));

    Ok(ActiveSession {
        session_id: session["id"].as_str().unwrap_or("session").to_string(),
        allotment,
        used: 0,
        metric: ad.metric.clone(),
        cost_sats: cost,
        gateway: gateway.to_string(),
        usage_observed: false,
        credit_baseline: None,
        promised_allotment: Some(allotment),
        credit_observed: false,
        optimistic_ends_at_ms,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ad_fixture() -> Value {
        json!({
            "kind": 10021,
            "pubkey": "aa".repeat(32),
            "tags": [
                ["metric", "milliseconds"],
                ["step_size", "60000"],
                ["price_per_step", "cashu", "2", "sat", "https://other.example", "1"],
                ["price_per_step", "cashu", "1", "sat", "https://testnut.cashu.space/", "1"]
            ]
        })
    }

    #[test]
    fn selects_the_wallets_offer_and_calculates_cost() {
        let ad = parse_advertisement(&ad_fixture()).unwrap();
        assert_eq!(ad.pricing.len(), 2);
        let (steps, cost) = payment_cost(&ad, "https://testnut.cashu.space", Some(3)).unwrap();
        assert_eq!((steps, cost), (3, 3));
    }

    #[test]
    fn parses_whoami_mac() {
        assert_eq!(
            parse_whoami_mac("mac=DE:AD:BE:EF:00:42\n").unwrap(),
            "DE:AD:BE:EF:00:42"
        );
        assert!(parse_whoami_mac("mac=not-a-mac").is_err());
        assert!(parse_whoami_mac("no key value").is_err());
    }

    #[test]
    fn parses_all_three_session_states() {
        assert_eq!(
            parse_session_state(r#"{"status":1,"mac":"de:ad:be:ef:00:42","state":"none"}"#)
                .unwrap(),
            SessionState::None
        );
        assert_eq!(
            parse_session_state(r#"{"status":1,"mac":"de:ad:be:ef:00:42","state":"active"}"#)
                .unwrap(),
            SessionState::Active
        );
        assert_eq!(
            parse_session_state(r#"{"status":1,"mac":"de:ad:be:ef:00:42","state":"expired"}"#)
                .unwrap(),
            SessionState::Expired
        );
        assert!(parse_session_state(r#"{"state":"bogus"}"#).is_err());
        assert!(parse_session_state("not json").is_err());
    }

    #[test]
    fn parses_usage_and_no_session() {        assert_eq!(
            parse_usage("125/1000\n").unwrap(),
            Some(Usage {
                used: 125,
                allotment: 1000
            })
        );
        assert_eq!(parse_usage("-1/-1").unwrap(), None);
        assert!(parse_usage("-1/10").is_err());
    }

    #[test]
    fn parses_current_notice_codes() {
        let notice = json!({
            "kind": 21023,
            "tags": [
                ["code", "payment-outcome-unknown"],
                ["message", "do not retry this note"]
            ],
            "content": "fallback"
        });
        assert_eq!(
            notice_details(&notice),
            (
                Some("payment-outcome-unknown".to_string()),
                "do not retry this note".to_string()
            )
        );
    }

    #[test]
    fn session_uses_observed_usage_after_initial_response() {
        let ad = parse_advertisement(&ad_fixture()).unwrap();
        let response = json!({
            "kind": 1022,
            "id": "abc",
            "tags": [["allotment", "60000"], ["metric", "milliseconds"]]
        });
        let mut session = session_from_response(&response, &ad, "gateway", 1).unwrap();
        session.update_usage(Usage {
            used: 10_000,
            allotment: 120_000,
        });
        let remaining = session.remaining();
        assert!(
            (109_000..=110_000).contains(&remaining),
            "wall-clock decay may shave sub-seconds, got {remaining}"
        );
        assert!(session.usage_observed);
    }

    #[test]
    fn stale_usage_does_not_confirm_a_payment() {
        let ad = parse_advertisement(&ad_fixture()).unwrap();
        let response = json!({
            "kind": 1022,
            "id": "abc",
            "tags": [["allotment", "120000"], ["metric", "milliseconds"]]
        });
        let mut session = session_from_response(&response, &ad, "gateway", 1).unwrap();
        session.credit_baseline = Some(60_000);

        session.update_usage(Usage {
            used: 59_000,
            allotment: 60_000,
        });
        assert!(!session.credit_observed);

        session.update_usage(Usage {
            used: 60_000,
            allotment: 120_000,
        });
        assert!(session.credit_observed);
    }

    #[test]
    fn rejects_bad_advertisements() {
        let mut ad = ad_fixture();
        ad["tags"][0][1] = json!("bananas");
        assert!(parse_advertisement(&ad).is_err());
        ad = ad_fixture();
        ad["pubkey"] = json!("not-a-key");
        assert!(parse_advertisement(&ad).is_err());
    }
}

#[cfg(test)]
mod extra_tests {
    use super::*;
    use serde_json::json;

    fn two_mint_fixture() -> Value {
        json!({
            "kind": 10021,
            "pubkey": "ab".repeat(32),
            "tags": [
                ["metric", "bytes"],
                ["step_size", "100000"],
                ["price_per_step", "cashu", "9", "sat", "https://other.example/", "3"],
                ["price_per_step", "cashu", "2", "sats", "https://TESTNUT.cashu.space", "1"],
                ["price_per_step", "lightning", "1", "msat", "https://mint.example", "1"],
                ["price_per_step", "cashu", "0", "sat", "https://zero.example", "1"]
            ]
        })
    }

    #[test]
    fn selects_our_mint_by_canonical_url_case_and_slash() {
        let ad = parse_advertisement(&two_mint_fixture()).unwrap();
        assert_eq!(ad.pricing.len(), 2, "lightning + zero-sat offers dropped");
        let offer = ad.offer_for_mint("https://testnut.cashu.space").unwrap();
        assert_eq!(offer.price_per_step, 2);
        assert_eq!(offer.min_steps, 1);
    }

    #[test]
    fn payment_cost_floors_at_min_steps_and_multiplies() {
        let ad = parse_advertisement(&two_mint_fixture()).unwrap();
        let (steps, cost) = payment_cost(&ad, "https://testnut.cashu.space", None).unwrap();
        assert_eq!((steps, cost), (1, 2));
        let (steps, cost) = payment_cost(&ad, "https://testnut.cashu.space", Some(7)).unwrap();
        assert_eq!((steps, cost), (7, 14));
    }

    #[test]
    fn unknown_mint_is_a_clear_error() {
        let ad = parse_advertisement(&two_mint_fixture()).unwrap();
        let err = payment_cost(&ad, "https://wallet.example", None).unwrap_err();
        assert!(err.to_string().contains("does not accept wallet mint"));
    }

    #[test]
    fn bytes_sessions_report_null_remaining_ms() {
        let ad = parse_advertisement(&two_mint_fixture()).unwrap();
        let response = json!({
            "kind": 1022,
            "id": "x",
            "tags": [["allotment", "500000"], ["metric", "bytes"]]
        });
        let session = session_from_response(&response, &ad, "g", 2).unwrap();
        assert_eq!(session.metric, "bytes");
        assert!(session.to_json()["remaining_ms"].is_null());
        assert_eq!(session.to_json()["remaining"], json!(500_000));
    }

    #[test]
    fn metric_mismatch_between_ad_and_session_is_rejected() {
        let ad = parse_advertisement(&two_mint_fixture()).unwrap();
        let response = json!({
            "kind": 1022,
            "tags": [["allotment", "500000"], ["metric", "milliseconds"]]
        });
        assert!(session_from_response(&response, &ad, "g", 2).is_err());
    }

    #[test]
    fn notice_message_tag_wins_over_content() {
        let notice = json!({
            "kind": 21023,
            "tags": [["code", "payment-error-below-swap-fee"], ["message", "swap fee exceeds payment"]],
            "content": "ignored"
        });
        let (code, message) = notice_details(&notice);
        assert_eq!(code.as_deref(), Some("payment-error-below-swap-fee"));
        assert_eq!(message, "swap fee exceeds payment");
    }

    #[test]
    fn usage_parser_rejects_negatives_other_than_no_session() {
        assert!(parse_usage("5/-3").is_err());
        assert!(parse_usage("abc/10").is_err());
        assert!(parse_usage("10").is_err());
        assert_eq!(parse_usage("0/0").unwrap(), Some(Usage { used: 0, allotment: 0 }));
    }

    #[test]
    fn resume_from_usage_marks_observed() {
        let session = ActiveSession::from_usage(
            "g",
            "milliseconds",
            0,
            Usage { used: 10_000, allotment: 60_000 },
        );
        assert!(session.usage_observed);
        let remaining = session.remaining();
        assert!(
            (49_000..=50_000).contains(&remaining),
            "wall-clock decay may shave sub-seconds, got {remaining}"
        );
        let shown = session.to_json()["remaining_ms"].as_u64().unwrap_or_default();
        assert!((49_000..=50_000).contains(&shown));
    }

    fn ms_session_with_deadline(allotment: u64, used: u64, deadline: u64) -> ActiveSession {
        let mut session = ActiveSession::from_usage(
            "g",
            "milliseconds",
            1,
            Usage { used, allotment },
        );
        session.optimistic_ends_at_ms = Some(deadline);
        session
    }

    #[test]
    fn ms_session_with_past_deadline_has_no_time_left() {
        let session = ms_session_with_deadline(60_000, 10_000, now_ms().saturating_sub(1_000));
        assert_eq!(session.remaining(), 0, "expired deadline must drain remaining");
    }

    #[test]
    fn ms_session_with_future_deadline_decays_by_wall_clock() {
        let deadline = now_ms() + 30_000;
        let session = ms_session_with_deadline(60_000, 10_000, deadline);
        let remaining = session.remaining();
        assert!(remaining > 0, "future deadline keeps time left");
        assert!(
            remaining <= 30_000,
            "remaining bounded by the deadline delta, got {remaining}"
        );
    }

    #[test]
    fn bytes_session_stays_frozen_at_allotment_minus_used() {
        let session = ActiveSession::from_usage(
            "g",
            "bytes",
            1,
            Usage { used: 100_000, allotment: 500_000 },
        );
        assert_eq!(session.remaining(), 400_000);
        // wall-clock passage must not erode a metered bytes session
        std::thread::sleep(std::time::Duration::from_millis(20));
        assert_eq!(session.remaining(), 400_000);
    }

    #[test]
    fn update_usage_re_arms_the_deadline_upward() {
        let mut session = ms_session_with_deadline(25_000, 0, now_ms() + 25_000);
        session.update_usage(Usage { used: 0, allotment: 120_000 });
        let Some(deadline) = session.optimistic_ends_at_ms else {
            panic!("update_usage must maintain the ms deadline");
        };
        assert!(
            deadline >= now_ms() + 119_000,
            "deadline follows gateway-side growth, got {}",
            deadline.saturating_sub(now_ms())
        );
        assert!(session.remaining() > 25_000);
    }

    #[test]
    fn replacement_renewal_confirms_credit_against_the_promised_allotment() {
        // issue #5 rig finding: the real gateway renews by replacing the
        // session with a fresh allotment — /usage never grows past the old
        // baseline, so every renewal counted as blind and tripped the cap
        // within ~3 renewals of normal operation.
        let ad = parse_advertisement(&two_mint_fixture()).unwrap();
        let response = json!({
            "kind": 1022,
            "id": "renewal",
            "tags": [["allotment", "100000"], ["metric", "bytes"]]
        });
        let mut session = session_from_response(&response, &ad, "g", 2).unwrap();
        session.credit_baseline = Some(100_000);
        assert_eq!(session.promised_allotment, Some(100_000));

        session.update_usage(Usage { used: 500, allotment: 100_000 });
        assert!(
            session.credit_observed,
            "delivering the promised allotment must confirm credit without baseline growth"
        );
    }

    #[test]
    fn fee_shortfall_below_advertised_increment_still_confirms_credit() {
        let ad = parse_advertisement(&two_mint_fixture()).unwrap();
        let response = json!({
            "kind": 1022,
            "id": "renewal",
            "tags": [["allotment", "99000"], ["metric", "bytes"]]
        });
        let mut session = session_from_response(&response, &ad, "g", 2).unwrap();
        session.credit_baseline = Some(100_000);

        session.update_usage(Usage { used: 300, allotment: 99_000 });
        assert!(
            session.credit_observed,
            "a mint-swap-fee shortfall vs the advertised step must not count as uncredited"
        );
    }

    #[test]
    fn usage_above_the_promise_confirms_credit() {
        let ad = parse_advertisement(&two_mint_fixture()).unwrap();
        let response = json!({
            "kind": 1022,
            "id": "renewal",
            "tags": [["allotment", "100000"], ["metric", "bytes"]]
        });
        let mut session = session_from_response(&response, &ad, "g", 2).unwrap();
        session.credit_baseline = Some(100_000);

        session.update_usage(Usage { used: 0, allotment: 150_000 });
        assert!(session.credit_observed);
    }

    #[test]
    fn usage_below_the_promise_stays_unconfirmed() {
        let ad = parse_advertisement(&two_mint_fixture()).unwrap();
        let response = json!({
            "kind": 1022,
            "id": "renewal",
            "tags": [["allotment", "200000"], ["metric", "bytes"]]
        });
        let mut session = session_from_response(&response, &ad, "g", 2).unwrap();
        session.credit_baseline = Some(100_000);

        session.update_usage(Usage { used: 99_000, allotment: 100_000 });
        assert!(
            !session.credit_observed,
            "credit the gateway never delivered (below the 1022 promise) must stay blind"
        );
    }

    #[test]
    fn resumed_sessions_carry_no_promise() {
        let session = ActiveSession::from_usage(
            "g",
            "milliseconds",
            0,
            Usage { used: 1_000, allotment: 60_000 },
        );
        assert_eq!(session.promised_allotment, None);
        assert!(session.credit_observed);
    }
}
