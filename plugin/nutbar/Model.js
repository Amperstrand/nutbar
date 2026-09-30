// Model.js — parsing/formatting helpers for the NutBar plugin.
// The daemon speaks plain JSON on 127.0.0.1:3939.

function parseJson(raw) {
  try {
    return JSON.parse(String(raw || ""))
  } catch (e) {
    return null
  }
}

// GET /status -> { up, balanceSats, session, mint }
function parseStatus(raw) {
  var parsed = parseJson(raw)
  if (!parsed || !parsed.ok) return { up: false, balanceSats: -1, session: null, mint: "", stashTokens: 0, pendingPayment: false, quarantinedPayments: 0, autopay: false, autopayHalted: false, renewals: 0 }
  return {
    up: true,
    balanceSats: Number(parsed.balance_sats || 0),
    session: parseSession(parsed.session),
    expiredGateway: parsed.expired_gateway || "",
    mint: String(parsed.mint || ""),
    stashTokens: Number(parsed.stash_tokens || 0),
    pendingPayment: parsed.pending_payment === true,
    quarantinedPayments: Number(parsed.quarantined_payments || 0),
    autopay: parsed.autopay === true,
    autopayHalted: parsed.autopay_halted === true,
    renewals: Number(parsed.renewals || 0),
    wifi: parsed.wifi || null
  }
}

function parseSession(s) {
  if (!s) return null
  var remaining = Number(s.remaining || s.remaining_ms || 0)
  if (remaining <= 0) return null
  return {
    sessionId: String(s.session_id || ""),
    remaining: remaining,
    remainingMs: String(s.metric || "") === "milliseconds" ? remaining : 0,
    used: Number(s.used || 0),
    allotment: Number(s.allotment || 0),
    costSats: Number(s.cost_sats || 0),
    gateway: String(s.gateway || ""),
    metric: String(s.metric || ""),
    creditObserved: s.credit_observed === true
  }
}

function parseAction(raw) {
  var parsed = parseJson(raw)
  if (!parsed) return { ok: false, error: "daemon returned invalid JSON" }
  if (!parsed.ok) return { ok: false, error: String(parsed.error || "unknown error") }
  return { ok: true, parsed: parsed }
}

// Map daemon error strings to plain language. Money-safety statements are
// only made where they are certainly true (Cashu proofs live on this
// computer, so a unreachable mint cannot spend them). Unmapped errors
// return an empty title and the UI shows the raw string alone.
function friendlyError(raw) {
  var e = String(raw || "").toLowerCase()
  if (e === "") return { title: "", detail: "" }
  if (e.indexOf("already spent") >= 0 || e.indexOf("already-spent") >= 0)
    return { title: "This payment code was already used. Nothing was added to your wallet.", detail: raw }
  if (e.indexOf("invalid token") >= 0 || e.indexOf("token parse") >= 0 || e.indexOf("invalid last symbol") >= 0 || e.indexOf("invalid byte") >= 0)
    return { title: "This is not a valid Cashu payment code. Check that you copied all of it.", detail: raw }
  if (e.indexOf("missing token") >= 0)
    return { title: "Paste a Cashu payment code first.", detail: raw }
  if (e.indexOf("insufficient") >= 0)
    return { title: "Not enough balance for this payment.", detail: raw }
  if (e.indexOf("unreachable") >= 0 || e.indexOf("mint_quote") >= 0 || e.indexOf("mint info") >= 0 || e.indexOf("transport") >= 0 || e.indexOf("502") >= 0 || e.indexOf("no wallet for") >= 0)
    return {
      title: "The mint is not responding. Your balance is safe on this computer. Try again later.",
      detail: raw
    }
  if (e.indexOf("expire") >= 0)
    return { title: "This invoice expired. Ask for a new one.", detail: raw }
  if (e.indexOf("invoice not paid within") >= 0)
    return { title: "The payment did not arrive in time. Nothing was spent — try again.", detail: raw }
  if (e.indexOf("daemon returned invalid json") >= 0)
    return { title: "The wallet app replied in an unexpected way. Try again.", detail: raw }
  return { title: "", detail: raw }
}

function barLabel(balanceSats, session) {
  var text = "🥜 " + (balanceSats >= 0
    ? fmtSats(balanceSats) + (balanceSats === 1 ? " sat" : " sats")
    : "–")
  if (session) text += session.metric === "bytes"
    ? " ⇅" + humanBytes(session.remaining)
    : " ⏱" + sessionMinutes(session.remainingMs)
  return text
}

function humanBytes(bytes) {
  var value = Math.max(0, Number(bytes || 0))
  var units = ["B", "KiB", "MiB", "GiB"]
  var i = 0
  while (value >= 1024 && i < units.length - 1) { value /= 1024; i++ }
  return (i === 0 ? Math.floor(value) : value.toFixed(1)) + units[i]
}

function sessionRemaining(session) {
  if (!session) return ""
  return session.metric === "bytes"
    ? humanBytes(session.remaining)
    : sessionMinutes(session.remainingMs)
}

function sessionMinutes(remainingMs) {
  var totalSec = Math.max(0, Math.floor(remainingMs / 1000))
  var m = Math.floor(totalSec / 60)
  var s = totalSec % 60
  if (m >= 60) {
    var h = Math.floor(m / 60)
    return h + "h" + (m % 60)
  }
  return m + ":" + (s < 10 ? "0" : "") + s
}

function tokenPreview(token) {
  var t = String(token || "")
  if (t.length <= 42) return t
  return t.substring(0, 24) + "…" + t.substring(t.length - 12)
}

function hostOf(url) {
  var m = String(url || "").replace(/^https?:\/\//, "")
  return m.split("/")[0] || url
}

function timeOf(ts) {
  var d = new Date(Number(ts || 0) * 1000)
  var h = d.getHours()
  var m = d.getMinutes()
  return (h < 10 ? "0" : "") + h + ":" + (m < 10 ? "0" : "") + m
}

function detectPayload(text) {
  var t = String(text || "").trim()
  if (t.indexOf("cashuA") === 0 || t.indexOf("cashuB") === 0 || t.indexOf("cashuC") === 0) return "token"
  if (t.indexOf("lnbc") === 0 || t.indexOf("lno") === 0 || t.indexOf("lnurl") === 0) return "invoice"
  return "unknown"
}

function fmtSats(n) {
  return Number(n || 0).toLocaleString(Qt.locale("en_US"), "f", 0)
}

function historyIncoming(direction) {
  return String(direction) === "Incoming"
}

function historyArrow(direction) {
  return historyIncoming(direction) ? "↓" : "↑"
}

function historyType(direction) {
  return historyIncoming(direction) ? "in" : "out"
}
