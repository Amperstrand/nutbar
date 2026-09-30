//! Durable offline payment stash.
//!
//! Cashu tokens are bearer money. A token is journaled to `payment.pending`
//! before it leaves the process and is only removed after a definitive gateway
//! response. An interrupted or ambiguous payment reuses the same token.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Delivery context journaled beside `payment.pending` so a restarted
/// daemon can resolve an ambiguous payment: re-POST the same token to the
/// same gateway (the gateway answers `payment-error-token-spent` on
/// replay, which is the protocol's own idempotency). Pendings created
/// before this field existed have no meta and are surfaced, never
/// auto-retried blind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingMeta {
    pub gateway: String,
    pub cost_sats: u64,
    pub created_unix: u64,
}

#[derive(Clone)]
pub struct Stash {
    path: PathBuf,
    pending_path: PathBuf,
    meta_path: PathBuf,
    lock: Arc<Mutex<()>>,
}

#[derive(Debug)]
pub enum StashError {
    Io(std::io::Error),
    Empty,
    PendingExists,
}

impl std::fmt::Display for StashError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StashError::Io(e) => write!(f, "stash io error: {e}"),
            StashError::Empty => write!(f, "stash empty"),
            StashError::PendingExists => write!(
                f,
                "a payment token is already pending; reconcile or retry it first"
            ),
        }
    }
}

impl From<std::io::Error> for StashError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

impl Stash {
    pub fn new(path: PathBuf) -> Self {
        let pending_path = path.with_file_name("payment.pending");
        let meta_path = path.with_file_name("payment.pending.meta");
        Self {
            path,
            pending_path,
            meta_path,
            lock: Arc::new(Mutex::new(())),
        }
    }

    pub fn count(&self) -> usize {
        let _guard = self.lock.lock().unwrap();
        read_tokens(&self.path).len()
    }

    pub fn pending(&self) -> Option<String> {
        let _guard = self.lock.lock().unwrap();
        read_token(&self.pending_path)
    }

    /// Journal the delivery context for the current pending token. Written
    /// after `set_pending` and before the POST: a crash between the two
    /// writes leaves a surfaced-only pending (the safe fallback).
    pub fn set_pending_meta(&self, meta: &PendingMeta) -> Result<(), StashError> {
        let _guard = self.lock.lock().unwrap();
        let body = format!("{}\n{}\n{}\n", meta.gateway, meta.cost_sats, meta.created_unix);
        atomic_write_private(&self.meta_path, &body)
    }

    pub fn pending_meta(&self) -> Option<PendingMeta> {
        let _guard = self.lock.lock().unwrap();
        let raw = fs::read_to_string(&self.meta_path).ok()?;
        let mut lines = raw.lines();
        let gateway = lines.next()?.trim().to_string();
        let cost_sats = lines.next()?.trim().parse().ok()?;
        let created_unix = lines.next()?.trim().parse().ok()?;
        if gateway.is_empty() {
            return None;
        }
        Some(PendingMeta {
            gateway,
            cost_sats,
            created_unix,
        })
    }

    pub fn quarantined_count(&self) -> usize {
        let Some(parent) = self.path.parent() else {
            return 0;
        };
        fs::read_dir(parent)
            .map(|entries| {
                entries
                    .flatten()
                    .filter(|entry| {
                        entry
                            .file_name()
                            .to_string_lossy()
                            .starts_with("payment.quarantine.")
                    })
                    .count()
            })
            .unwrap_or(0)
    }

    /// Return the existing pending token, or journal the first available
    /// stash token as pending. The journal is durable before the stash is
    /// rewritten, so a crash can duplicate a token but cannot lose one.
    pub fn checkout(&self) -> Result<(String, bool), StashError> {
        let _guard = self.lock.lock().unwrap();
        if let Some(token) = read_token(&self.pending_path) {
            return Ok((token, true));
        }

        let mut tokens = read_tokens(&self.path);
        if tokens.is_empty() {
            return Err(StashError::Empty);
        }
        let token = tokens.remove(0);
        atomic_write_private(&self.pending_path, &(token.clone() + "\n"))?;
        atomic_write_private(&self.path, &tokens_body(&tokens))?;
        Ok((token, false))
    }

    /// Journal a freshly-created wallet token before sending it.
    pub fn set_pending(&self, token: &str) -> Result<(), StashError> {
        let _guard = self.lock.lock().unwrap();
        if read_token(&self.pending_path).is_some() {
            return Err(StashError::PendingExists);
        }
        atomic_write_private(&self.pending_path, &(token.trim().to_string() + "\n"))
    }

    /// Definitive acknowledgement: remove the pending journal and any
    /// duplicate left in the available file by a crash between atomic writes.
    pub fn complete(&self, token: &str) -> Result<(), StashError> {
        let _guard = self.lock.lock().unwrap();
        let tokens: Vec<String> = read_tokens(&self.path)
            .into_iter()
            .filter(|candidate| candidate != token)
            .collect();
        atomic_write_private(&self.path, &tokens_body(&tokens))?;
        remove_if_exists(&self.pending_path)?;
        remove_if_exists(&self.meta_path)?;
        Ok(())
    }

    /// Preserve a token whose outcome cannot safely be retried. It is removed
    /// from the active queue but retained for operator reconciliation.
    pub fn quarantine(&self, token: &str, reason: &str) -> Result<PathBuf, StashError> {
        let _guard = self.lock.lock().unwrap();
        let tokens: Vec<String> = read_tokens(&self.path)
            .into_iter()
            .filter(|candidate| candidate != token)
            .collect();
        atomic_write_private(&self.path, &tokens_body(&tokens))?;

        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let safe_reason: String = reason
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' {
                    c
                } else {
                    '-'
                }
            })
            .collect();
        let path = self
            .path
            .with_file_name(format!("payment.quarantine.{safe_reason}.{stamp}.token"));
        atomic_write_private(&path, &(token.trim().to_string() + "\n"))?;
        remove_if_exists(&self.pending_path)?;
        remove_if_exists(&self.meta_path)?;
        Ok(path)
    }

    pub fn put(&self, token: &str) -> Result<(), StashError> {
        let _guard = self.lock.lock().unwrap();
        let mut tokens = read_tokens(&self.path);
        let token = token.trim();
        if !token.is_empty() && !tokens.iter().any(|existing| existing == token) {
            tokens.push(token.to_string());
        }
        atomic_write_private(&self.path, &tokens_body(&tokens))
    }
}

fn read_token(path: &Path) -> Option<String> {
    fs::read_to_string(path)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn read_tokens(path: &Path) -> Vec<String> {
    fs::read_to_string(path)
        .map(|body| {
            body.lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(ToOwned::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn tokens_body(tokens: &[String]) -> String {
    if tokens.is_empty() {
        String::new()
    } else {
        tokens.join("\n") + "\n"
    }
}

fn remove_if_exists(path: &Path) -> std::io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

fn atomic_write_private(path: &Path, body: &str) -> Result<(), StashError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
        }
    }

    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    let mut options = OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&tmp)?;
    file.write_all(body.as_bytes())?;
    file.sync_all()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))?;
    }
    fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_stash(name: &str) -> (PathBuf, Stash) {
        let dir =
            std::env::temp_dir().join(format!("omarchy-cashu-stash-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        (dir.clone(), Stash::new(dir.join("stash.tokens")))
    }

    #[test]
    fn pending_token_is_reused_until_completed() {
        let (dir, stash) = test_stash("reuse");
        stash.put("cashuA-one").unwrap();
        let (first, reused) = stash.checkout().unwrap();
        assert_eq!(first, "cashuA-one");
        assert!(!reused);
        let (second, reused) = stash.checkout().unwrap();
        assert_eq!(second, first);
        assert!(reused);
        stash.complete(&first).unwrap();
        assert!(stash.pending().is_none());
        assert_eq!(stash.count(), 0);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn unknown_outcome_is_quarantined() {
        let (dir, stash) = test_stash("unknown");
        stash.put("cashuB-unknown").unwrap();
        let (token, _) = stash.checkout().unwrap();
        let path = stash.quarantine(&token, "outcome-unknown").unwrap();
        assert!(path.exists());
        assert_eq!(stash.quarantined_count(), 1);
        assert!(stash.pending().is_none());
        fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn bearer_files_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let (dir, stash) = test_stash("permissions");
        stash.put("cashuA-secret").unwrap();
        assert_eq!(
            fs::metadata(&stash.path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        stash.checkout().unwrap();
        assert_eq!(
            fs::metadata(&stash.pending_path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        fs::remove_dir_all(dir).unwrap();
    }
}

#[cfg(test)]
mod extra_tests {
    use super::*;

    fn stash_in(name: &str) -> (PathBuf, Stash) {
        let dir = std::env::temp_dir().join(format!("stash-extra-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        (dir.clone(), Stash::new(dir.join("stash.tokens")))
    }

    #[test]
    fn crash_between_writes_leaves_duplicate_that_complete_removes() {
        let (dir, stash) = stash_in("crash-dup");
        stash.put("cashuA-dup").unwrap();
        // simulate the crash window: token journaled as pending AND still
        // present in the available file
        stash.checkout().unwrap();
        let mut tokens = read_tokens(&stash.path);
        tokens.push("cashuA-dup".to_string());
        atomic_write_private(&stash.path, &tokens_body(&tokens)).unwrap();

        stash.complete("cashuA-dup").unwrap();
        assert_eq!(stash.count(), 0);
        assert!(stash.pending().is_none());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn put_deduplicates_identical_tokens() {
        let (dir, stash) = stash_in("dedup");
        stash.put("cashuB-same").unwrap();
        stash.put("cashuB-same").unwrap();
        assert_eq!(stash.count(), 1);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn set_pending_refuses_when_one_exists() {
        let (dir, stash) = stash_in("pending-guard");
        stash.set_pending("cashuA-first").unwrap();
        assert!(matches!(
            stash.set_pending("cashuA-second"),
            Err(StashError::PendingExists)
        ));
        assert_eq!(stash.pending().as_deref(), Some("cashuA-first"));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn quarantine_reason_is_sanitized_into_the_filename() {
        let (dir, stash) = stash_in("quarantine");
        stash.set_pending("cashuA-q").unwrap();
        let path = stash.quarantine("cashuA-q", "outcome unknown/../../etc").unwrap();
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        assert!(name.starts_with("payment.quarantine.outcome-unknown-"));
        assert!(!name.contains('/'), "no path separators may survive: {name}");
        assert!(path.exists());
        assert!(stash.pending().is_none());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn checkout_on_empty_stash_errors() {
        let (dir, stash) = stash_in("empty");
        assert!(matches!(stash.checkout(), Err(StashError::Empty)));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn pending_meta_round_trips_and_is_private() {
        let (dir, stash) = stash_in("meta-roundtrip");
        stash.set_pending("cashuA-meta").unwrap();
        assert!(stash.pending_meta().is_none(), "no meta before it is written");
        let meta = PendingMeta {
            gateway: "10.99.98.2".to_string(),
            cost_sats: 3,
            created_unix: 1_791_000_000,
        };
        stash.set_pending_meta(&meta).unwrap();
        assert_eq!(stash.pending_meta(), Some(meta));
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(dir.join("payment.pending.meta"))
            .expect("meta file exists")
            .permissions()
            .mode();
        assert!(mode & 0o077 == 0, "meta must be owner-private, got {mode:o}");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn completing_or_quarantining_clears_pending_meta() {
        let (dir, stash) = stash_in("meta-clear");
        stash.set_pending("cashuA-clear").unwrap();
        stash
            .set_pending_meta(&PendingMeta {
                gateway: "10.99.98.2".to_string(),
                cost_sats: 1,
                created_unix: 1,
            })
            .unwrap();
        stash.complete("cashuA-clear").unwrap();
        assert!(stash.pending_meta().is_none());

        stash.set_pending("cashuA-q2").unwrap();
        stash
            .set_pending_meta(&PendingMeta {
                gateway: "10.99.98.2".to_string(),
                cost_sats: 1,
                created_unix: 2,
            })
            .unwrap();
        stash.quarantine("cashuA-q2", "outcome-unknown").unwrap();
        assert!(stash.pending_meta().is_none());
        let _ = fs::remove_dir_all(dir);
    }
}
