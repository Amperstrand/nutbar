//! wifi.rs — nmcli integration, entirely gated behind CASHUD_WIFI=1 so the
//! daemon never touches networking unless explicitly enabled.
//!
//! Responsibilities during a field test:
//! - scan for `TollGate-*` SSIDs
//! - connect to a tollgate AP on demand
//! - remember a fallback profile (the phone hotspot / home AP) and watch: if
//!   we are sitting on a tollgate AP with no live session, autoconnect back
//!   to the fallback so the laptop never strands itself offline.

use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

#[derive(Clone)]
pub struct WifiState {
    pub enabled: bool,
    pub fallback_profile: Arc<Mutex<Option<String>>>,
    pub last_action: Arc<Mutex<String>>,
    grace_until: Arc<Mutex<Option<Instant>>>,
}

impl WifiState {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            fallback_profile: Arc::new(Mutex::new(None)),
            last_action: Arc::new(Mutex::new("idle".to_string())),
            grace_until: Arc::new(Mutex::new(None)),
        }
    }

    pub fn note(&self, msg: &str) {
        *self.last_action.lock().unwrap() = msg.to_string();
        tracing::info!("wifi: {msg}");
    }

    /// Give discovery and a bounded payment POST time to complete before the
    /// fallback watchdog interprets the absent session as failure.
    pub fn begin_connection_grace(&self) {
        *self.grace_until.lock().unwrap() = Some(Instant::now() + Duration::from_secs(45));
    }
}

fn nmcli(args: &[&str]) -> Result<String> {
    let out = Command::new("nmcli")
        .args(args)
        .output()
        .context("nmcli not found")?;
    if !out.status.success() {
        anyhow::bail!(
            "nmcli {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

pub struct ScanResult {
    pub ssid: String,
    pub signal: i32,
}

/// One `TollGate-*` entry per SSID, strongest signal kept.
pub fn scan_tollgates() -> Result<Vec<ScanResult>> {
    let out = nmcli(&["-t", "-f", "SSID,SIGNAL", "dev", "wifi"])?;
    Ok(parse_scan_tollgates(&out))
}

/// Pure transform over `nmcli -t -f SSID,SIGNAL dev wifi` output: keep the
/// strongest signal per `TollGate*` SSID, sorted by signal.
pub fn parse_scan_tollgates(output: &str) -> Vec<ScanResult> {
    let mut best: std::collections::HashMap<String, i32> = std::collections::HashMap::new();
    for line in output.lines() {
        let Some((ssid, sig)) = line.rsplit_once(':') else {
            continue;
        };
        let Ok(sig) = sig.parse::<i32>() else {
            continue;
        };
        if ssid.starts_with("TollGate") {
            let e = best.entry(ssid.to_string()).or_insert(i32::MIN);
            if sig > *e {
                *e = sig;
            }
        }
    }
    let mut v: Vec<ScanResult> = best
        .into_iter()
        .map(|(ssid, signal)| ScanResult { ssid, signal })
        .collect();
    v.sort_by_key(|entry| std::cmp::Reverse(entry.signal));
    v
}

/// Currently active WiFi SSID, if any.
pub fn active_ssid() -> Result<Option<String>> {
    let out = nmcli(&["-t", "-f", "ACTIVE,SSID", "dev", "wifi"])?;
    Ok(parse_active_ssid(&out))
}

/// Pure transform over `nmcli -t -f ACTIVE,SSID dev wifi` output.
pub fn parse_active_ssid(output: &str) -> Option<String> {
    for line in output.lines() {
        let Some((active, ssid)) = line.split_once(':') else {
            continue;
        };
        if active == "yes" && !ssid.is_empty() {
            return Some(ssid.to_string());
        }
    }
    None
}

/// Default-route gateway — on a TollGate AP this is the gateway serving
/// :2121 (the payment target when the client has no explicit one).
pub fn gateway_ip() -> Result<Option<String>> {
    let out = Command::new("ip")
        .args(["route", "show", "default"])
        .output()
        .context("ip route failed")?;
    if !out.status.success() {
        anyhow::bail!(
            "ip route failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        if let Some(rest) = line.trim().strip_prefix("default via ") {
            if let Some(ip) = rest.split_whitespace().next() {
                return Ok(Some(ip.to_string()));
            }
        }
    }
    Ok(None)
}

/// Existing connection profile names (wifi type only).
pub fn wifi_profiles() -> Result<Vec<String>> {
    let out = nmcli(&["-t", "-f", "NAME,TYPE", "con", "show"])?;
    Ok(out
        .lines()
        .filter_map(|l| {
            let (name, typ) = l.split_once(':')?;
            (typ == "802-11-wireless").then(|| name.to_string())
        })
        .collect())
}

pub fn connect_profile(profile: &str) -> Result<()> {
    nmcli(&["con", "up", profile]).map(|_| ())
}

pub fn connect_ssid(ssid: &str) -> Result<()> {
    // Prefer an existing profile (keeps any stored PSK); fall back to a
    // fresh open connection.
    if let Ok(profiles) = wifi_profiles() {
        if profiles.iter().any(|p| p == ssid) {
            return connect_profile(ssid);
        }
    }
    nmcli(&["dev", "wifi", "connect", ssid]).map(|_| ())
}

/// Designate the fallback (second) SSID: autoconnect on, NM-default
/// priority. Field-verified on the rig (2026-09-29): with no priority
/// boost, NetworkManager roams to the fallback on AP loss even with the
/// daemon dead — while a boosted priority left armed is the S10
/// mid-outage roam trap (persistent NM state racing link blips). The
/// daemon-alive path is the watchdog's explicit, gateway-aware connect.
/// Only profiles that already exist may be designated — this must never
/// invent networks.
pub fn set_fallback(state: &WifiState, profile: &str) -> Result<()> {
    let profiles = wifi_profiles()?;
    if !profiles.iter().any(|p| p == profile) {
        anyhow::bail!("no wifi profile named '{profile}' — connect once manually first");
    }
    nmcli(&[
        "con",
        "modify",
        profile,
        "connection.autoconnect",
        "yes",
        "connection.autoconnect-priority",
        "0",
    ])?;
    *state.fallback_profile.lock().unwrap() = Some(profile.to_string());
    state.note(&format!(
        "fallback set to '{profile}' (autoconnect, default priority)"
    ));
    Ok(())
}

/// Disarm ONLY leftover armed boosts (the NM scan), regardless of any
/// in-memory designation — the boot-time normalizer. Called at daemon
/// start so persistent NM state from earlier daemons or stories dies the
/// moment a fixed cashud boots, without waiting for a TollGate connect.
pub fn disarm_legacy_boosts(state: &WifiState) {
    let targets = leftover_boosted_profiles().unwrap_or_default();
    for fallback in targets {
        if let Err(e) = nmcli(&[
            "con",
            "modify",
            &fallback,
            "connection.autoconnect-priority",
            "0",
        ]) {
            state.note(&format!("legacy boost disarm failed for '{fallback}': {e}"));
            continue;
        }
        state.note(&format!(
            "legacy fallback boost on '{fallback}' disarmed at boot (priority 0)"
        ));
    }
}

/// Disarm the autoconnect boost on the fallback. Two paths, because the
/// boost is PERSISTENT NetworkManager state while the designation is
/// in-memory: the designated profile (fast path), and — across daemon
/// restarts, when no designation is loaded — any wifi profile carrying
/// priority exactly 200, the value pre-2026-09-29 daemons wrote (the
/// rig's mgmt profile uses 300 and is never touched). S10: an armed
/// boost outliving the daemon roamed the client back to the fallback
/// mid-outage while renewals were healthy.
pub fn unboost_fallback(state: &WifiState) {
    let designated = state.fallback_profile.lock().unwrap().clone();
    let targets: Vec<String> = match designated {
        Some(profile) => vec![profile],
        None => match leftover_boosted_profiles() {
            Ok(found) if !found.is_empty() => found,
            _ => return,
        },
    };
    for fallback in targets {
        if let Err(e) = nmcli(&[
            "con",
            "modify",
            &fallback,
            "connection.autoconnect-priority",
            "0",
        ]) {
            state.note(&format!("fallback un-boost failed for '{fallback}': {e}"));
            continue;
        }
        state.note(&format!(
            "fallback '{fallback}' autoconnect boost disarmed (priority 0)"
        ));
    }
}

/// Pure transform over `nmcli -g NAME,AUTOCONNECT-PRIORITY con show`:
/// wifi profiles still carrying the reserved fallback boost (200).
pub fn parse_leftover_boosted(output: &str) -> Vec<String> {
    output
        .lines()
        .filter_map(|l| {
            let (name, priority) = l.split_once(':')?;
            (priority == "200").then(|| name.to_string())
        })
        .collect()
}

fn leftover_boosted_profiles() -> Result<Vec<String>> {
    let out = nmcli(&["-g", "NAME,AUTOCONNECT-PRIORITY", "con", "show"])?;
    Ok(parse_leftover_boosted(&out))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FallbackDecision {
    /// Stay on the TollGate AP.
    Hold,
    /// Autoconnect back to the fallback profile.
    FallBack,
}

/// Gateway-aware fallback decision while sitting on a TollGate AP.
///
/// Hold when the session is alive, and — the S10 fix — when the gateway
/// still answers while a payment outcome is unresolved (a retained
/// session at a renewal boundary, or a journaled pending payment): the
/// renewal machinery owns that window, and roaming away mid-flight
/// strands a healthy paying client. Fall back when the gateway is dead
/// (the stranded-client escape) or when it answers but nothing is
/// running (the classic unpaid-captive case).
pub fn fallback_decision(
    on_tollgate_ap: bool,
    session_alive: bool,
    renewal_pending: bool,
    gateway_answers: bool,
) -> FallbackDecision {
    if !on_tollgate_ap || session_alive {
        return FallbackDecision::Hold;
    }
    if gateway_answers && renewal_pending {
        return FallbackDecision::Hold;
    }
    FallbackDecision::FallBack
}

/// Watchdog: on a tollgate AP with no live session, fall back — unless the
/// gateway still answers while a payment outcome is unresolved (see
/// [`fallback_decision`]). The priority boost is one-shot: once the
/// fallback fires, it is disarmed so NM cannot keep roaming back.
/// Called from the daemon's periodic loop, never spawns its own threads.
pub fn check_fallback(
    state: &WifiState,
    session_alive: bool,
    renewal_pending: bool,
    gateway_answers: bool,
) {
    if !state.enabled {
        return;
    }
    let fallback = state.fallback_profile.lock().unwrap().clone();
    let Some(fallback) = fallback else { return };
    if state
        .grace_until
        .lock()
        .unwrap()
        .is_some_and(|deadline| Instant::now() < deadline)
    {
        return;
    }
    if let Ok(Some(ssid)) = active_ssid() {
        if ssid.starts_with("TollGate") {
            let decision = fallback_decision(true, session_alive, renewal_pending, gateway_answers);
            if decision == FallbackDecision::Hold {
                return;
            }
            state.note(&format!(
                "on tollgate AP '{ssid}' with no session (gateway answers: {gateway_answers}) -> falling back to '{fallback}'"
            ));
            if let Err(e) = connect_profile(&fallback) {
                state.note(&format!("fallback connect failed: {e}"));
            } else {
                unboost_fallback(state);
            }
        }
    }
}

/// Global kill flag so the watchdog can be disabled at runtime.
pub static WIFI_WATCHDOG_PAUSED: AtomicBool = AtomicBool::new(false);

pub fn watchdog_active() -> bool {
    !WIFI_WATCHDOG_PAUSED.load(Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_keeps_strongest_per_tollgate_ssid() {
        let out = "TollGate-Rust:29\nTollGate-Rust:52\nHomeNet:90\nTollGate-Alpha:41\n";
        let found = parse_scan_tollgates(out);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].ssid, "TollGate-Rust");
        assert_eq!(found[0].signal, 52);
        assert_eq!(found[1].ssid, "TollGate-Alpha");
    }

    #[test]
    fn scan_ignores_non_tollgate_and_garbage() {
        let out = "stargate:100\n:44\nKINGKASSA:42\nnot-a-signal\nTollGate:\n";
        assert!(parse_scan_tollgates(out).is_empty());
    }

    #[test]
    fn scan_ssid_with_colon_keeps_suffix_parse() {
        // rsplit_once: an SSID containing colons still parses off the signal
        let found = parse_scan_tollgates("TollGate-A:B:37\n");
        assert_eq!(found[0].ssid, "TollGate-A:B");
        assert_eq!(found[0].signal, 37);
    }

    #[test]
    fn active_ssid_picks_the_yes_row() {
        let out = "no:HomeNet\nyes:TollGate-Rust\nno:Other\n";
        assert_eq!(parse_active_ssid(out).as_deref(), Some("TollGate-Rust"));
        assert_eq!(parse_active_ssid("no:HomeNet\nyes:\n"), None);
        assert_eq!(parse_active_ssid(""), None);
    }

    // ---- gateway-aware fallback decision (S10: the mid-outage roam) ---

    use super::FallbackDecision::*;

    #[test]
    fn live_session_never_falls_back() {
        assert_eq!(fallback_decision(true, true, false, false), Hold);
        // Even with a dead gateway the session object vouches — the
        // supervisor's own drop paths own that transition.
        assert_eq!(fallback_decision(true, true, true, false), Hold);
    }

    #[test]
    fn off_tollgate_never_triggers() {
        assert_eq!(fallback_decision(false, false, false, false), Hold);
        assert_eq!(fallback_decision(false, false, true, true), Hold);
    }

    #[test]
    fn renewal_boundary_with_live_gateway_holds() {
        // The S10 shape: session retained at remaining==0 (renewal in
        // backoff) or a journaled pending payment, gateway healthy —
        // roaming away strands a healthy paying client.
        assert_eq!(fallback_decision(true, false, true, true), Hold);
    }

    #[test]
    fn dead_gateway_escapes_even_with_pending_payment() {
        // AP/gateway gone: the pending payment cannot settle here —
        // get the client to the fallback (the 30-poll escape semantics).
        assert_eq!(fallback_decision(true, false, true, false), FallBack);
    }

    #[test]
    fn healthy_gateway_with_nothing_running_falls_back() {
        // The classic unpaid-captive case: nothing live, nothing pending,
        // gateway answers — nothing keeps us here.
        assert_eq!(fallback_decision(true, false, false, true), FallBack);
        assert_eq!(fallback_decision(true, false, false, false), FallBack);
    }

    #[test]
    fn leftover_boost_scan_finds_only_the_reserved_value() {
        // 200 is the reserved fallback boost; other priorities (mgmt=300,
        // defaults=0) are never touched — even across daemon restarts,
        // when the in-memory designation is gone.
        let out = "mgmt:300\nHomeNet:200\nTollGate-VM:0\nother:\nbroken\n";
        assert_eq!(parse_leftover_boosted(out), vec!["HomeNet".to_string()]);
        assert!(parse_leftover_boosted("mgmt:300\nTollGate-VM:0\n").is_empty());
    }
}
