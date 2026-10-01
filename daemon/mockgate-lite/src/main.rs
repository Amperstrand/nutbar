//! mockgate-lite — a dependency-free mock TollGate gateway for rehearsing
//! the laptop client over real WiFi against a phone: run it in Termux with
//! the hotspot on, connect the laptop to the hotspot, pay the gateway IP.
//!
//!   GET  /        -> kind 10021 advertisement (1 sat per 60s step, testnut)
//!   GET  /whoami  -> "mac=DE:AD:BE:EF:00:42"
//!   POST /        -> raw cashu token or kind-21000 event; first spend of a
//!                    token grants a session, replays get the spent notice.
//!
//! std-only on purpose: cross-compiles to aarch64 musl without any C
//! toolchain, and grants a fixed allotment per token (it does not decode
//! token amounts — that is mockgate's job on a real machine).

use std::collections::HashSet;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Instant;

const PORT: u16 = 2121;
const ALLOTMENT_MS: u64 = 60_000;

struct State {
    ad_pubkey: String,
    spent: Mutex<HashSet<String>>,
    session: Mutex<Option<(Instant, u64, u64)>>,
}

fn main() {
    let bind_all = std::env::args().any(|a| a == "--phone");
    let addr = if bind_all { "0.0.0.0" } else { "127.0.0.1" };
    let ad_pubkey = hex(rand_bytes(32));
    println!("mockgate-lite on {addr}:{PORT} — advertisement pubkey {ad_pubkey}");
    println!("no gating: everything on this AP has internet; payment flow only");

    let state = Arc::new(State {
        ad_pubkey,
        spent: Mutex::new(HashSet::new()),
        session: Mutex::new(None),
    });

    let listener = match TcpListener::bind((addr, PORT)) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("bind {addr}:{PORT} failed: {e}");
            std::process::exit(1);
        }
    };
    for stream in listener.incoming().flatten() {
        let state = state.clone();
        std::thread::spawn(move || {
            let _ = serve(stream, &state);
        });
    }
}

fn serve(mut stream: TcpStream, state: &State) -> std::io::Result<()> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    let header_end;
    loop {
        let n = stream.read(&mut tmp)?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(pos) = find(&buf, b"\r\n\r\n") {
            header_end = pos + 4;
            break;
        }
        if buf.len() > 64 * 1024 {
            return respond(&mut stream, 431, "text/plain", "headers too large");
        }
    }
    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let mut lines = head.lines();
    let request_line = lines.next().unwrap_or("");
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let path = parts.next().unwrap_or("");
    let path = path.split('?').next().unwrap_or(path);

    let mut content_length = 0usize;
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            if k.trim().eq_ignore_ascii_case("content-length") {
                content_length = v.trim().parse().unwrap_or(0);
            }
        }
    }
    if content_length > 1024 * 1024 {
        return respond(&mut stream, 413, "text/plain", "body too large");
    }
    while buf.len() < header_end + content_length {
        let n = stream.read(&mut tmp)?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
    }
    let body = String::from_utf8_lossy(&buf[header_end..])
        .trim()
        .to_string();

    match (method, path) {
        ("GET", "/") => {
            let ad = format!(
                "{{\"id\":\"{id}\",\"pubkey\":\"{pk}\",\"created_at\":{now},\"kind\":10021,\"tags\":[[\"metric\",\"milliseconds\"],[\"step_size\",\"60000\"],[\"price_per_step\",\"cashu\",\"1\",\"sat\",\"https://testnut.cashu.space\",\"1\"],[\"tips\",\"01\",\"02\",\"03\",\"04\"]],\"content\":\"\",\"sig\":\"{id}{id}\"}}",
                id = hex(rand_bytes(16)),
                pk = state.ad_pubkey,
                now = unix_now(),
            );
            respond(&mut stream, 200, "application/json", &ad)
        }
        ("GET", "/whoami") => respond(&mut stream, 200, "text/plain", "mac=DE:AD:BE:EF:00:42"),
        ("GET", "/usage") => {
            let mut guard = state.session.lock().unwrap();
            let response = match guard.as_ref() {
                Some((started, allotment, _))
                    if started.elapsed().as_millis() < *allotment as u128 =>
                {
                    format!("{}/{}", started.elapsed().as_millis(), allotment)
                }
                _ => {
                    *guard = None;
                    "-1/-1".to_string()
                }
            };
            respond(&mut stream, 200, "text/plain", &response)
        }
        ("POST", "/") => {
            let token = extract_token(&body);
            let Some(token) = token else {
                return respond(
                    &mut stream,
                    400,
                    "application/json",
                    "{\"error\":\"no cashu token in body\"}",
                );
            };
            let already = state.spent.lock().unwrap().contains(&token);
            if already {
                println!("REPLAY of spent token");
                let notice = format!(
                    "{{\"id\":\"{id}\",\"pubkey\":\"mockgate-lite\",\"created_at\":{now},\"kind\":21023,\"tags\":[[\"p\",\"customer\"],[\"code\",\"payment-error-token-spent\"],[\"message\",\"Cashu token already spent\"]],\"content\":\"payment-error-token-spent: Cashu token already spent\",\"sig\":\"{id}{id}\"}}",
                    id = hex(rand_bytes(16)),
                    now = unix_now(),
                );
                return respond(&mut stream, 200, "application/json", &notice);
            }
            state.spent.lock().unwrap().insert(token);
            let (allotment, start_time) = {
                let mut guard = state.session.lock().unwrap();
                let now_ms = unix_now().saturating_mul(1000);
                match guard.as_mut() {
                    Some((started, total, start_time))
                        if started.elapsed().as_millis() < *total as u128 =>
                    {
                        *total = total.saturating_add(ALLOTMENT_MS);
                        (*total, *start_time)
                    }
                    _ => {
                        *guard = Some((Instant::now(), ALLOTMENT_MS, now_ms));
                        (ALLOTMENT_MS, now_ms)
                    }
                }
            };
            println!("accepted token -> total allotment {allotment} ms");
            let session = format!(
                "{{\"id\":\"{id}\",\"pubkey\":\"mockgate-lite\",\"created_at\":{now},\"kind\":1022,\"tags\":[[\"p\",\"customer\"],[\"device-identifier\",\"mac\",\"DE:AD:BE:EF:00:42\"],[\"allotment\",\"{allotment}\"],[\"start-time\",\"{start_time}\"],[\"metric\",\"milliseconds\"]],\"content\":\"\",\"sig\":\"{id}{id}\"}}",
                id = hex(rand_bytes(16)),
                now = unix_now(),
            );
            respond(&mut stream, 200, "application/json", &session)
        }
        _ => respond(&mut stream, 404, "text/plain", "not found"),
    }
}

/// Raw token body, or the payment-tag token inside a kind-21000 event.
fn extract_token(body: &str) -> Option<String> {
    if body.starts_with("cashu") {
        return Some(body.to_string());
    }
    let start = body.find("\"cashu")? + 1;
    let rest = &body[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

fn respond(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &str,
) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        413 => "Payload Too Large",
        _ => "Error",
    };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n{body}",
        len = body.len(),
    );
    stream.write_all(response.as_bytes())
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn rand_bytes(n: usize) -> Vec<u8> {
    let mut buf = vec![0u8; n];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        let _ = f.read_exact(&mut buf);
    }
    buf
}

fn hex(bytes: Vec<u8>) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::extract_token;

    #[test]
    fn extracts_raw_token_body() {
        assert_eq!(
            extract_token("cashuAeyJ0b2tlbiI6W119"),
            Some("cashuAeyJ0b2tlbiI6W119".to_string())
        );
    }

    #[test]
    fn extracts_token_from_payment_tag() {
        let body = r#"{"kind":21000,"tags":[["p","x"],["payment","cashuBabc123"]],"content":""}"#;
        assert_eq!(extract_token(body), Some("cashuBabc123".to_string()));
    }

    #[test]
    fn returns_none_without_token() {
        assert_eq!(extract_token(r#"{"kind":21000,"tags":[["p","x"]]}"#), None);
    }
}
