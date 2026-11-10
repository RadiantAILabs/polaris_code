use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::{ChatGptError, PLAN_SCOPE};
use crate::private_file;

/// Account details for display, excluding authentication tokens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountSummary {
    /// Registration ID. Separate registrations may share an email address.
    pub id: String,
    /// Email and registration suffix.
    pub label: String,
    /// Whether the account has granted `ChatGPT` plan usage.
    pub plan_enabled: bool,
    /// Whether this is the active registration.
    pub active: bool,
    /// Whether credentials remain available locally.
    pub signed_in: bool,
}

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Tokens {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub id_token: Option<String>,
    pub scope: String,
    pub expires_at: u64,
    pub earliest_refresh_at: u64,
}

impl Tokens {
    pub fn plan_enabled(&self) -> bool {
        self.scope
            .split_whitespace()
            .any(|scope| scope == PLAN_SCOPE)
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Registration {
    pub client_id: String,
    pub subject: String,
    pub email: String,
    pub tokens: Option<Tokens>,
    #[serde(default)]
    pub usage_acknowledged: bool,
    #[serde(default)]
    pub identity_pending: bool,
}

impl Registration {
    pub fn summary(&self, active: Option<&str>) -> AccountSummary {
        let suffix: String = self
            .client_id
            .chars()
            .rev()
            .take(8)
            .collect::<String>()
            .chars()
            .rev()
            .collect();
        let name = if self.tokens.is_none() || self.email.trim().is_empty() {
            "ChatGPT account".to_string()
        } else {
            super::safe_label(&self.email)
        };
        AccountSummary {
            id: self.client_id.clone(),
            label: format!("{name} · {suffix}"),
            plan_enabled: self.tokens.as_ref().is_some_and(Tokens::plan_enabled),
            active: active == Some(self.client_id.as_str()),
            signed_in: self.tokens.is_some(),
        }
    }
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub(super) struct Records {
    pub host_id: String,
    pub active: Option<String>,
    pub accounts: Vec<Registration>,
}

impl Records {
    fn summaries(&self) -> Vec<AccountSummary> {
        self.accounts
            .iter()
            .map(|account| account.summary(self.active.as_deref()))
            .collect()
    }

    /// Clear an account's tokens and deselect it, returning its refresh token.
    pub fn sign_out(&mut self, index: usize) -> Option<String> {
        let account = &mut self.accounts[index];
        let tokens = account.tokens.take();
        account.identity_pending = false;
        if self.active.as_deref() == Some(account.client_id.as_str()) {
            self.active = None;
        }
        tokens.and_then(|tokens| tokens.refresh_token)
    }
}

/// How long to wait for another process to release the credential lock.
const LOCK_TIMEOUT: Duration = Duration::from_secs(30);

/// Stores account registrations and tokens in a private file.
///
/// Requires Unix file permissions. A cross-process lock protects token refreshes
/// and account updates.
#[derive(Debug, Clone)]
pub struct ChatGptStore {
    path: PathBuf,
}

impl ChatGptStore {
    /// Use `chatgpt.json` in the user configuration directory.
    ///
    /// # Errors
    /// Returns an error if no configuration directory can be resolved.
    pub fn user() -> Result<Self, ChatGptError> {
        crate::config_dir::config_file("chatgpt.json")
            .map(Self::at)
            .ok_or_else(|| {
                ChatGptError::message(
                    "no configuration directory found; set POLARIS_CONFIG_DIR to choose where credentials are stored"
                        .into(),
                )
            })
    }

    /// Use the given credential file.
    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Read account summaries synchronously. Prefer [`Self::accounts_async`]
    /// on async tasks.
    ///
    /// # Errors
    /// Returns an error for inaccessible, exposed, or malformed private storage.
    pub fn accounts(&self) -> Result<Vec<AccountSummary>, ChatGptError> {
        Ok(self.read()?.summaries())
    }

    /// Read account summaries without blocking the async task.
    ///
    /// # Errors
    /// Returns an error for inaccessible, exposed, or malformed private storage.
    pub async fn accounts_async(&self) -> Result<Vec<AccountSummary>, ChatGptError> {
        Ok(self.read_async().await?.summaries())
    }

    pub(super) async fn read_async(&self) -> Result<Records, ChatGptError> {
        let store = self.clone();
        tokio::task::spawn_blocking(move || store.read())
            .await
            .map_err(|_| ChatGptError::message("credential read task stopped".into()))?
    }

    pub(super) async fn write_async(
        &self,
        records: &Records,
        lock: &std::fs::File,
    ) -> Result<(), ChatGptError> {
        let lock = lock.try_clone()?;
        let store = self.clone();
        let records = records.clone();
        // A cancelled caller must not release the lock while this write is queued or running.
        tokio::task::spawn_blocking(move || {
            let result = store.write(&records);
            drop(lock);
            result
        })
        .await
        .map_err(|_| ChatGptError::message("credential write task stopped".into()))?
    }

    pub(super) fn read(&self) -> Result<Records, ChatGptError> {
        crate::private_file::require_unix()?;
        match private_file::read(&self.path)? {
            None => Ok(Records::default()),
            Some(bytes) => serde_json::from_slice(&bytes).map_err(|_| {
                ChatGptError::message(
                    "ChatGPT credentials are malformed; restore the private credential file or choose another connection"
                        .into(),
                )
            }),
        }
    }

    pub(super) fn write(&self, records: &Records) -> Result<(), ChatGptError> {
        crate::private_file::require_unix()?;
        let bytes = serde_json::to_vec_pretty(records)
            .map_err(|_| ChatGptError::message("could not encode ChatGPT credentials".into()))?;
        private_file::write(&self.path, &bytes)?;
        Ok(())
    }

    /// Take the cross-process credential lock, waiting up to [`LOCK_TIMEOUT`].
    pub(super) async fn lock(&self) -> Result<std::fs::File, ChatGptError> {
        self.lock_within(LOCK_TIMEOUT).await
    }

    async fn lock_within(&self, timeout: Duration) -> Result<std::fs::File, ChatGptError> {
        let path = self.path.with_extension("lock");
        let file = tokio::task::spawn_blocking(move || {
            crate::private_file::require_unix()?;
            private_file::directory(
                path.parent()
                    .ok_or_else(|| std::io::Error::other("missing credential directory"))?,
            )?;
            private_file::open(&path, true)
        })
        .await
        .map_err(|_| ChatGptError::message("credential lock task stopped".into()))??;
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(file),
                Err(std::fs::TryLockError::WouldBlock)
                    if tokio::time::Instant::now() < deadline =>
                {
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
                Err(std::fs::TryLockError::WouldBlock) => {
                    return Err(ChatGptError::message(format!(
                        "timed out after {} s waiting for another process to release the ChatGPT credential lock",
                        timeout.as_secs()
                    )));
                }
                Err(std::fs::TryLockError::Error(error)) => return Err(error.into()),
            }
        }
    }

    pub(super) async fn host_id(&self) -> Result<String, ChatGptError> {
        let lock = self.lock().await?;
        let mut records = self.read_async().await?;
        if records.host_id.is_empty() {
            records.host_id = format!("urn:uuid:{}", uuid::Uuid::new_v4());
            self.write_async(&records, &lock).await?;
        }
        Ok(records.host_id)
    }

    /// Record that the user has acknowledged this account's plan usage notice.
    ///
    /// # Errors
    /// Returns an error if the registration is missing or private storage fails.
    pub async fn acknowledge_usage(&self, id: &str) -> Result<(), ChatGptError> {
        let lock = self.lock().await?;
        let mut records = self.read_async().await?;
        let account = records
            .accounts
            .iter_mut()
            .find(|a| a.client_id == id)
            .ok_or_else(|| ChatGptError::message("ChatGPT account is no longer saved".into()))?;
        account.usage_acknowledged = true;
        self.write_async(&records, &lock).await
    }

    /// Check whether the plan usage notice still needs acknowledgement.
    ///
    /// # Errors
    /// Returns an error if private storage is inaccessible or malformed.
    pub async fn needs_usage_acknowledgement_async(&self, id: &str) -> Result<bool, ChatGptError> {
        Ok(self
            .read_async()
            .await?
            .accounts
            .iter()
            .any(|account| account.client_id == id && !account.usage_acknowledged))
    }

    /// Make a saved registration with plan usage permission active.
    ///
    /// # Errors
    /// Returns an error for an unknown, signed-out, unverified, or plan-disabled
    /// registration, or if private storage cannot be updated.
    pub async fn select(&self, id: &str) -> Result<(), ChatGptError> {
        let lock = self.lock().await?;
        let mut records = self.read_async().await?;
        if !records.accounts.iter().any(|a| {
            a.client_id == id
                && !a.identity_pending
                && a.tokens.as_ref().is_some_and(Tokens::plan_enabled)
        }) {
            return Err(ChatGptError::message(
                "sign in and enable ChatGPT plan usage for this account first".into(),
            ));
        }
        records.active = Some(id.to_string());
        self.write_async(&records, &lock).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registration(id: &str) -> Registration {
        Registration {
            client_id: id.into(),
            subject: "same-user".into(),
            email: "person@example.test".into(),
            usage_acknowledged: false,
            identity_pending: false,
            tokens: Some(Tokens {
                access_token: format!("access-{id}"),
                refresh_token: Some(format!("refresh-{id}")),
                id_token: None,
                scope: PLAN_SCOPE.into(),
                expires_at: super::super::now() + 300,
                earliest_refresh_at: 0,
            }),
        }
    }

    async fn two_account_store() -> (tempfile::TempDir, ChatGptStore) {
        let dir = tempfile::tempdir().expect("temporary directory");
        let store = ChatGptStore::at(dir.path().join("chatgpt.json"));
        store
            .write(&Records {
                accounts: vec![registration("client-one"), registration("client-two")],
                ..Records::default()
            })
            .expect("seed two registrations");
        (dir, store)
    }

    #[test]
    fn cancelled_write_keeps_the_lock_until_storage_finishes() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .max_blocking_threads(1)
            .build()
            .expect("test runtime");
        let dir = tempfile::tempdir().expect("temporary directory");
        let store = ChatGptStore::at(dir.path().join("chatgpt.json"));
        let lock = runtime.block_on(store.lock()).expect("credential lock");
        let (started, started_rx) = std::sync::mpsc::channel();
        let (release, release_rx) = std::sync::mpsc::channel();
        let blocker = runtime.spawn_blocking(move || {
            started.send(()).expect("signal blocking worker");
            release_rx.recv().expect("release blocking worker");
        });
        started_rx.recv().expect("blocking worker is occupied");
        let records = Records {
            host_id: "queued-write".into(),
            ..Records::default()
        };
        runtime.block_on(async {
            let mut write = Box::pin(store.write_async(&records, &lock));
            assert!(futures_util::poll!(&mut write).is_pending());
            drop(write);
            drop(lock);
            let contender = private_file::open(&store.path.with_extension("lock"), true)
                .expect("independent lock file");
            let attempt = contender.try_lock();
            release.send(()).expect("release storage worker");
            assert!(
                matches!(attempt, Err(std::fs::TryLockError::WouldBlock)),
                "cancelling the caller must not unlock a queued write"
            );
            blocker.await.expect("blocking worker finishes");
            let _lock = store
                .lock_within(Duration::from_secs(2))
                .await
                .expect("write releases its lock after completion");
            assert_eq!(
                store.read().expect("written records").host_id,
                "queued-write"
            );
        });
    }

    #[tokio::test]
    async fn select_marks_only_the_chosen_registration_active() {
        let (_dir, store) = two_account_store().await;
        store
            .select("client-two")
            .await
            .expect("a signed-in plan account can be selected");
        let accounts = store.accounts_async().await.expect("read summaries");
        assert_ne!(
            accounts[0].label, accounts[1].label,
            "registrations sharing an email are told apart by suffix"
        );
        assert!(!accounts[0].active, "the other registration stays inactive");
        assert!(accounts[1].active, "the selected registration is active");
    }

    #[tokio::test]
    async fn select_refuses_ineligible_accounts_without_changing_records() {
        for reason in ["unknown", "signed-out", "identity-pending", "missing-plan"] {
            let (_dir, store) = two_account_store().await;
            store
                .select("client-one")
                .await
                .expect("select original account");
            let mut records = store.read().expect("read accounts");
            let candidate = &mut records.accounts[1];
            match reason {
                "signed-out" => candidate.tokens = None,
                "identity-pending" => candidate.identity_pending = true,
                "missing-plan" => {
                    candidate.tokens.as_mut().unwrap().scope = "openid profile".into()
                }
                _ => {}
            }
            store.write(&records).expect("save candidate state");
            let before = std::fs::read(&store.path).expect("saved records");
            let id = if reason == "unknown" {
                "unknown-client"
            } else {
                "client-two"
            };
            let error = store
                .select(id)
                .await
                .expect_err("ineligible account is refused");
            assert_eq!(
                error.to_string(),
                "sign in and enable ChatGPT plan usage for this account first",
                "{reason}"
            );
            assert_eq!(
                store.read().unwrap().active.as_deref(),
                Some("client-one"),
                "{reason}: previous selection is retained"
            );
            assert_eq!(
                std::fs::read(&store.path).unwrap(),
                before,
                "{reason}: every stored record is unchanged"
            );
        }
    }

    #[tokio::test]
    async fn acknowledge_usage_is_per_registration() {
        let (_dir, store) = two_account_store().await;
        store
            .acknowledge_usage("client-two")
            .await
            .expect("acknowledge a saved registration");
        assert!(
            store
                .needs_usage_acknowledgement_async("client-one")
                .await
                .expect("read acknowledgement"),
            "the unacknowledged registration still needs the notice"
        );
        assert!(
            !store
                .needs_usage_acknowledgement_async("client-two")
                .await
                .expect("read acknowledgement"),
            "the acknowledged registration does not"
        );
    }

    #[tokio::test]
    async fn host_id_is_stable_across_calls() {
        let dir = tempfile::tempdir().expect("temporary directory");
        let store = ChatGptStore::at(dir.path().join("chatgpt.json"));
        let host = store.host_id().await.expect("create host id");
        assert_eq!(
            store.host_id().await.expect("read host id"),
            host,
            "the host id is generated once and reused"
        );
    }

    #[tokio::test]
    async fn debug_output_omits_tokens() {
        let (_dir, store) = two_account_store().await;
        let accounts = store.accounts_async().await.expect("read summaries");
        let debug = format!("{accounts:?} {store:?}");
        assert!(
            !debug.contains("access-client") && !debug.contains("refresh-client"),
            "summaries and the store never print tokens"
        );
    }

    #[test]
    fn read_rejects_exposed_files_without_echoing_contents() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("temporary directory");
        let path = dir.path().join("chatgpt.json");
        std::fs::write(&path, b"secret-token").expect("write exposed file");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))
            .expect("loosen permissions");
        let error = ChatGptStore::at(&path)
            .read()
            .err()
            .expect("an exposed file is refused")
            .to_string();
        assert!(error.contains("chmod 600"), "the message names the fix");
        assert!(
            !error.contains("secret-token"),
            "the contents are not echoed"
        );
    }

    #[tokio::test]
    async fn lock_excludes_independent_stores_until_released() {
        let dir = tempfile::tempdir().expect("temporary directory");
        let path = dir.path().join("chatgpt.json");
        let first = ChatGptStore::at(&path)
            .lock()
            .await
            .expect("take the first lock");
        let other = ChatGptStore::at(&path);
        let error = other
            .lock_within(Duration::from_millis(50))
            .await
            .expect_err("a second store cannot take a held lock");
        assert!(
            error.to_string().contains("timed out"),
            "waiting for a held lock times out with a clear error"
        );
        drop(first);
        other
            .lock_within(Duration::from_millis(50))
            .await
            .expect("the lock is available once released");
    }
}
