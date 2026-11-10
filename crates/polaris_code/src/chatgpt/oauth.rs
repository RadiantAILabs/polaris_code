use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use jsonwebtoken::{DecodingKey, Validation, decode, decode_header, jwk::JwkSet};
use reqwest::{Client, Url};
use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use super::http::{bounded_json, response_error};
use super::store::{Records, Registration, Tokens};
use super::{AccountSummary, ChatGptError, ChatGptStore, ISSUER, RESOURCE, now};

const SCOPES: &str =
    "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct";
const MAX_CALLBACK_CONNECTIONS: usize = 8;

/// Handles `ChatGPT` authorization and token refresh against fixed `OpenAI` endpoints.
#[derive(Clone)]
pub struct ChatGptClient {
    pub(super) http: Client,
    pub(super) inference: Client,
    pub(super) store: ChatGptStore,
    identity_keys: Arc<Mutex<IdentityKeys>>,
    #[cfg(test)]
    token_endpoint: Option<String>,
}

/// OIDC discovery and signing keys, cached for the client's lifetime.
/// The key set is refetched when a token names a key it does not hold.
#[derive(Default)]
struct IdentityKeys {
    discovery: Option<Arc<Discovery>>,
    keys: Option<Arc<JwkSet>>,
}

/// A pending browser sign-in. Dropping it closes the callback listener.
/// Debug output omits the sign-in URL and callback credentials.
pub struct PendingLogin {
    listener: TcpListener,
    url: Url,
    redirect_uri: String,
    state: String,
    nonce: String,
    verifier: String,
    returning: Option<Registration>,
    issued_client_id: Option<String>,
}

#[derive(Deserialize)]
struct Discovery {
    jwks_uri: String,
    revocation_endpoint: String,
}

#[derive(Clone, Deserialize)]
struct Identity {
    sub: String,
    email: Option<String>,
    nonce: Option<String>,
}

#[derive(Clone, Copy)]
enum IdentityValidity {
    CurrentLogin,
    PendingRotation,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    id_token: Option<String>,
    scope: Option<String>,
    token_type: String,
    expires_in: u64,
    #[serde(default)]
    earliest_refresh_at: u64,
}

impl std::fmt::Debug for ChatGptClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChatGptClient").finish_non_exhaustive()
    }
}

impl std::fmt::Debug for PendingLogin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingLogin").finish_non_exhaustive()
    }
}

impl ChatGptClient {
    /// Create clients for authorization and model requests. Redirects are disabled.
    ///
    /// # Errors
    /// Returns an error if the HTTP clients cannot be initialized.
    pub fn new(store: ChatGptStore) -> Result<Self, ChatGptError> {
        let http = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(60))
            .build()
            .map_err(|_| {
                ChatGptError::message("could not initialize the ChatGPT HTTP client".into())
            })?;
        let inference = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(10))
            .read_timeout(Duration::from_secs(300))
            .build()
            .map_err(|_| {
                ChatGptError::message("could not initialize the ChatGPT inference client".into())
            })?;
        Ok(Self {
            http,
            inference,
            store,
            identity_keys: Arc::default(),
            #[cfg(test)]
            token_endpoint: None,
        })
    }

    fn token_endpoint(&self) -> &str {
        #[cfg(test)]
        if let Some(endpoint) = &self.token_endpoint {
            return endpoint;
        }
        "https://auth.openai.com/api/accounts/oauth/token"
    }

    /// Start browser sign-in for a saved registration, or a new one if `account` is `None`.
    /// Set `consent` only when the user has asked to enable plan usage.
    ///
    /// # Errors
    /// Returns an error for inaccessible account storage, an unknown registration,
    /// unavailable secure randomness, or failure to bind the loopback callback.
    pub async fn begin(
        &self,
        account: Option<&str>,
        consent: bool,
    ) -> Result<PendingLogin, ChatGptError> {
        self.begin_with_client(account, consent, None).await
    }

    /// Start a fresh attempt using the issued client from a rejected code exchange.
    pub(crate) async fn begin_with_client(
        &self,
        account: Option<&str>,
        consent: bool,
        retry_client_id: Option<&str>,
    ) -> Result<PendingLogin, ChatGptError> {
        let host_id = self.store.host_id().await?;
        let returning = match account {
            Some(id) => Some(
                self.store
                    .read_async()
                    .await?
                    .accounts
                    .into_iter()
                    .find(|a| a.client_id == id)
                    .ok_or_else(|| {
                        ChatGptError::message(
                            "the selected ChatGPT account is no longer saved".into(),
                        )
                    })?,
            ),
            None => None,
        };
        let issued_client_id = returning
            .as_ref()
            .map(|account| account.client_id.clone())
            .or_else(|| retry_client_id.map(str::to_string));
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
        let redirect_uri = format!(
            "http://127.0.0.1:{}/auth/callback",
            listener.local_addr()?.port()
        );
        let state = random()?;
        let nonce = random()?;
        let verifier = random()?;
        use sha2::{Digest, Sha256};
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        let mut url =
            Url::parse(&format!("{ISSUER}/api/accounts/authorize")).expect("fixed OAuth URL");
        {
            let mut query = url.query_pairs_mut();
            query.extend_pairs([
                (
                    "client_id",
                    issued_client_id
                        .as_deref()
                        .unwrap_or("dynamic_agent_client"),
                ),
                ("ext_agent_host_id", host_id.as_str()),
                ("response_type", "code"),
                ("redirect_uri", redirect_uri.as_str()),
                ("scope", SCOPES),
                ("resource", RESOURCE),
                ("state", state.as_str()),
                ("nonce", nonce.as_str()),
                ("code_challenge_method", "S256"),
                ("code_challenge", challenge.as_str()),
            ]);
            if let Some(account) = &returning
                && account.tokens.is_some()
                && !account.email.trim().is_empty()
            {
                query.append_pair("login_hint", &account.email);
            } else if issued_client_id.is_none() {
                query.append_pair("agent_name_hint", "Polaris Code");
            }
            if consent {
                query.append_pair("prompt", "consent");
            }
        }
        Ok(PendingLogin {
            listener,
            url,
            redirect_uri,
            state,
            nonce,
            verifier,
            returning,
            issued_client_id,
        })
    }

    /// Complete sign-in and save the verified credentials.
    /// The callback expires after five minutes. Call [`ChatGptStore::select`]
    /// to make the signed-in account active.
    ///
    /// # Errors
    /// Returns an error for a cancelled or expired login, failed token exchange,
    /// invalid identity claims, or inaccessible private storage.
    pub async fn complete(&self, pending: PendingLogin) -> Result<AccountSummary, ChatGptError> {
        let callback = tokio::time::timeout(Duration::from_secs(300), pending.callback())
            .await
            .map_err(|_| {
                ChatGptError::message("sign-in timed out; start a new sign-in attempt".into())
            })??;
        let client_id = match &pending.issued_client_id {
            Some(issued_client_id) => {
                if callback
                    .get("client_id")
                    .is_some_and(|id| id != issued_client_id)
                {
                    return Err(ChatGptError::message(
                        "the sign-in returned a different client registration".into(),
                    ));
                }
                issued_client_id.clone()
            }
            None => callback
                .get("client_id")
                .filter(|id| !id.is_empty() && id.as_str() != "dynamic_agent_client")
                .cloned()
                .ok_or_else(|| {
                    ChatGptError::message(
                        "registration did not return an issued client ID; try signing in again"
                            .into(),
                    )
                })?,
        };
        let code = callback
            .get("code")
            .filter(|code| !code.is_empty())
            .ok_or_else(|| {
                ChatGptError::message(
                    "the sign-in callback did not include an authorization code".into(),
                )
            })?;
        let response = self
            .http
            .post(self.token_endpoint())
            .form(&[
                ("grant_type", "authorization_code"),
                ("client_id", client_id.as_str()),
                ("code", code),
                ("code_verifier", pending.verifier.as_str()),
                ("redirect_uri", pending.redirect_uri.as_str()),
                ("resource", RESOURCE),
            ])
            .send()
            .await
            .map_err(ChatGptError::network)?;
        let token = token_response(
            response,
            TokenGrant::AuthorizationCode {
                client_id: &client_id,
            },
        )
        .await?;
        let id_token = token
            .id_token
            .as_deref()
            .ok_or_else(|| ChatGptError::message("sign-in returned no identity token".into()))?;
        let identity = self
            .identity(id_token, &client_id, IdentityValidity::CurrentLogin)
            .await?;
        if identity.nonce.as_deref() != Some(pending.nonce.as_str()) {
            return Err(ChatGptError::message(
                "sign-in identity nonce did not match this attempt".into(),
            ));
        }
        if pending
            .returning
            .as_ref()
            .is_some_and(|a| a.subject != identity.sub)
        {
            return Err(ChatGptError::message(
                "a different ChatGPT identity was returned; add it as a separate account".into(),
            ));
        }
        let registration = Registration {
            client_id: client_id.clone(),
            subject: identity.sub,
            email: identity.email.unwrap_or_else(|| "ChatGPT account".into()),
            tokens: Some(token.into_tokens(None)),
            identity_pending: false,
            usage_acknowledged: pending
                .returning
                .as_ref()
                .is_some_and(|a| a.usage_acknowledged),
        };
        let lock = self.store.lock().await?;
        let mut records = self.store.read_async().await?;
        if let Some(existing) = records
            .accounts
            .iter_mut()
            .find(|a| a.client_id == client_id)
        {
            *existing = registration.clone();
        } else {
            records.accounts.push(registration.clone());
        }
        self.store.write_async(&records, &lock).await?;
        Ok(registration.summary(records.active.as_deref()))
    }

    /// OIDC discovery, fetched once per client.
    async fn discovery(&self) -> Result<Arc<Discovery>, ChatGptError> {
        if let Some(discovery) = self.cached_keys().discovery.clone() {
            return Ok(discovery);
        }
        let result = self
            .http
            .get(format!("{ISSUER}/.well-known/openid-configuration"))
            .send()
            .await
            .map_err(ChatGptError::network)?;
        let discovery: Discovery = identity_document(result).await?;
        for endpoint in [&discovery.jwks_uri, &discovery.revocation_endpoint] {
            let url = Url::parse(endpoint).map_err(ChatGptError::identity_unavailable)?;
            if url.scheme() != "https"
                || url.host_str() != Some("auth.openai.com")
                || !url.username().is_empty()
            {
                return Err(ChatGptError::identity_unavailable(std::io::Error::other(
                    "identity discovery returned an unexpected endpoint",
                )));
            }
        }
        let discovery = Arc::new(discovery);
        self.cached_keys().discovery = Some(discovery.clone());
        Ok(discovery)
    }

    /// Signing keys that include `kid`, refetched only when the cache lacks it.
    async fn signing_keys(&self, kid: Option<&str>) -> Result<Arc<JwkSet>, ChatGptError> {
        if let Some(keys) = self.cached_keys().keys.clone()
            && kid.is_none_or(|kid| keys.find(kid).is_some())
        {
            return Ok(keys);
        }
        let discovery = self.discovery().await?;
        let response = self
            .http
            .get(&discovery.jwks_uri)
            .send()
            .await
            .map_err(ChatGptError::identity_unavailable)?;
        let keys: Arc<JwkSet> = Arc::new(identity_document(response).await?);
        self.cached_keys().keys = Some(keys.clone());
        Ok(keys)
    }

    fn cached_keys(&self) -> std::sync::MutexGuard<'_, IdentityKeys> {
        self.identity_keys
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    async fn identity(
        &self,
        token: &str,
        client_id: &str,
        validity: IdentityValidity,
    ) -> Result<Identity, ChatGptError> {
        let kid = decode_header(token).ok().and_then(|header| header.kid);
        let keys = self.signing_keys(kid.as_deref()).await?;
        validate_identity(token, client_id, &keys, validity)
    }

    /// Return a valid access token, refreshing it under a cross-process lock if needed.
    ///
    /// # Panics
    /// Panics if polled outside a `tokio` runtime.
    pub(super) async fn access_token(&self, id: &str) -> Result<String, ChatGptError> {
        // Finish saving a rotated refresh token even if the requesting turn is
        // cancelled: the previous token may already be invalid.
        let client = self.clone();
        let id = id.to_string();
        tokio::spawn(async move { client.refresh_or_read(&id).await })
            .await
            .map_err(|_| ChatGptError::message("credential refresh task stopped".into()))?
    }

    async fn refresh_or_read(&self, id: &str) -> Result<String, ChatGptError> {
        let records = self.store.read_async().await?;
        if let Some(access) = valid_access(&records, id)? {
            return Ok(access);
        }
        let lock = self.store.lock().await?;
        let mut records = self.store.read_async().await?;
        if let Some(access) = valid_access(&records, id)? {
            return Ok(access);
        }
        let index = account_index(&records, id)?;
        if records.accounts[index].identity_pending {
            self.verify_pending_identity(&mut records, index, &lock)
                .await?;
            if let Some(access) = valid_access(&records, id)? {
                return Ok(access);
            }
        }
        let tokens = records.accounts[index]
            .tokens
            .as_ref()
            .ok_or_else(signed_out)?;
        let refresh = tokens.refresh_token.as_deref().ok_or_else(|| {
            ChatGptError::message("ChatGPT access expired; sign in to ChatGPT again".into())
        })?;
        let response = self
            .http
            .post(self.token_endpoint())
            .form(&[
                ("grant_type", "refresh_token"),
                ("client_id", id),
                ("refresh_token", refresh),
                ("resource", RESOURCE),
            ])
            .send()
            .await
            .map_err(ChatGptError::network)?;
        let token = match token_response(response, TokenGrant::RefreshToken).await {
            Ok(token) => token,
            Err(error) => {
                if error.is_expired() {
                    let refresh = records.sign_out(index);
                    self.store.write_async(&records, &lock).await?;
                    self.revoke_in_background(id, refresh);
                }
                return Err(error);
            }
        };
        self.persist_rotation(&mut records, index, token, &lock)
            .await?;
        if records.accounts[index].identity_pending {
            self.verify_pending_identity(&mut records, index, &lock)
                .await?;
        }
        refreshed_access(&records.accounts[index])
    }

    /// Force the next [`Self::access_token`] call to refresh, if `rejected` is
    /// still the saved access token. Call this after the service rejects it.
    pub(super) async fn invalidate_access(
        &self,
        id: &str,
        rejected: &str,
    ) -> Result<(), ChatGptError> {
        let lock = self.store.lock().await?;
        let mut records = self.store.read_async().await?;
        let index = account_index(&records, id)?;
        if let Some(tokens) = records.accounts[index].tokens.as_mut()
            && tokens.access_token == rejected
        {
            tokens.expires_at = 0;
            tokens.earliest_refresh_at = 0;
            self.store.write_async(&records, &lock).await?;
        }
        Ok(())
    }

    async fn persist_rotation(
        &self,
        records: &mut Records,
        index: usize,
        token: TokenResponse,
        lock: &std::fs::File,
    ) -> Result<(), ChatGptError> {
        let identity_pending = token.id_token.is_some();
        let updated = token.into_tokens(records.accounts[index].tokens.as_ref());
        records.accounts[index].tokens = Some(updated);
        records.accounts[index].identity_pending = identity_pending;
        // Save the replacement before identity verification, since the old refresh
        // token may be invalid. identity_pending blocks its use until verified.
        self.store.write_async(records, lock).await
    }

    async fn verify_pending_identity(
        &self,
        records: &mut Records,
        index: usize,
        lock: &std::fs::File,
    ) -> Result<(), ChatGptError> {
        let account = &records.accounts[index];
        let id_token = account
            .tokens
            .as_ref()
            .and_then(|tokens| tokens.id_token.as_deref())
            .ok_or_else(|| {
                ChatGptError::message(
                    "refreshed credentials have no identity token; sign in to ChatGPT again".into(),
                )
            })?;
        let verification = self
            .identity(
                id_token,
                &account.client_id,
                IdentityValidity::PendingRotation,
            )
            .await;
        let client_id = account.client_id.clone();
        let (outcome, rejected) = self
            .finish_identity_check(records, index, verification, lock)
            .await?;
        self.revoke_in_background(&client_id, rejected);
        outcome
    }

    /// Record the identity check's result. A rejected identity signs the
    /// account out and returns its refresh token for revocation.
    async fn finish_identity_check(
        &self,
        records: &mut Records,
        index: usize,
        verification: Result<Identity, ChatGptError>,
        lock: &std::fs::File,
    ) -> Result<(Result<(), ChatGptError>, Option<String>), ChatGptError> {
        let outcome = match verification {
            Ok(identity) if identity.sub == records.accounts[index].subject => Ok(()),
            Ok(_) => Err(ChatGptError::message(
                "refreshed ChatGPT identity changed; sign in to ChatGPT again".into(),
            )),
            Err(error) if error.is_network() => return Err(error),
            Err(error) => Err(error),
        };
        records.accounts[index].identity_pending = false;
        let rejected = if outcome.is_err() {
            records.sign_out(index)
        } else {
            None
        };
        self.store.write_async(records, lock).await?;
        Ok((outcome, rejected))
    }

    /// Remove the selected account and active selection, then attempt token revocation.
    /// Returns `true` if revocation succeeds or the account was already signed out.
    ///
    /// # Errors
    /// Returns an error when the account or private storage cannot be accessed.
    pub async fn sign_out(&self, id: &str) -> Result<bool, ChatGptError> {
        let refresh = {
            let lock = self.store.lock().await?;
            let mut records = self.store.read_async().await?;
            let index = account_index(&records, id)?;
            let signed_in = records.accounts[index].tokens.is_some();
            let refresh = records.sign_out(index);
            records.accounts.remove(index);
            self.store.write_async(&records, &lock).await?;
            if !signed_in {
                return Ok(true);
            }
            refresh
        };
        match refresh {
            Some(refresh) => Ok(self.revoke(id, &refresh).await),
            None => Ok(false),
        }
    }

    /// Revoke a refresh token without delaying the caller. Best effort.
    ///
    /// # Panics
    /// Panics if a token is supplied outside a `tokio` runtime.
    fn revoke_in_background(&self, id: &str, refresh: Option<String>) {
        if let Some(refresh) = refresh {
            let client = self.clone();
            let id = id.to_string();
            tokio::spawn(async move { client.revoke(&id, &refresh).await });
        }
    }

    /// Revoke a refresh token, retrying server errors. Returns whether it succeeded.
    async fn revoke(&self, id: &str, refresh: &str) -> bool {
        let Ok(discovery) = self.discovery().await else {
            return false;
        };
        for attempt in 0..3 {
            let result = self
                .http
                .post(&discovery.revocation_endpoint)
                .form(&[
                    ("token", refresh),
                    ("token_type_hint", "refresh_token"),
                    ("client_id", id),
                ])
                .send()
                .await;
            if let Ok(response) = &result {
                if response.status() == reqwest::StatusCode::OK {
                    return true;
                }
                if !response.status().is_server_error() {
                    break;
                }
            }
            if attempt < 2 {
                tokio::time::sleep(Duration::from_millis(250 * (attempt + 1))).await;
            }
        }
        false
    }

    /// Fetch the account's available models in server order.
    ///
    /// # Errors
    /// Returns an error if authorization, transport, catalog validation, or
    /// private storage fails, or if no eligible models are available.
    pub async fn models(&self, id: &str) -> Result<Vec<(String, String)>, ChatGptError> {
        self.models_at(id, &format!("{RESOURCE}/models")).await
    }

    async fn models_at(&self, id: &str, url: &str) -> Result<Vec<(String, String)>, ChatGptError> {
        let send = async |access: &str| {
            self.http
                .get(url)
                .bearer_auth(access)
                .send()
                .await
                .map_err(ChatGptError::network)
        };
        let access = self.access_token(id).await?;
        let mut response = send(&access).await?;
        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            self.invalidate_access(id, &access).await?;
            let access = self.access_token(id).await?;
            response = send(&access).await?;
        }
        if !response.status().is_success() {
            return Err(ChatGptError::message(response_error(response).await));
        }
        let body: serde_json::Value = bounded_json(response)
            .await
            .map_err(ChatGptError::from_generation)?;
        let models: Vec<_> = body["models"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|model| model["visibility"] == "list")
            .filter_map(|model| {
                Some((
                    model["slug"].as_str()?.to_string(),
                    super::safe_label(model["display_name"].as_str()?),
                ))
            })
            .collect();
        if models.is_empty() {
            return Err(ChatGptError::message(
                "no eligible models were returned for this ChatGPT account".into(),
            ));
        }
        Ok(models)
    }
}

impl PendingLogin {
    /// Browser URL for this attempt. Do not log or persist it.
    pub fn authorization_url(&self) -> &str {
        self.url.as_str()
    }

    /// Wait for the browser redirect. Connections are served concurrently with
    /// a fixed cap and per-connection timeouts.
    async fn callback(&self) -> Result<BTreeMap<String, String>, ChatGptError> {
        let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
        // Dropping the set aborts connections still being served.
        let mut connections = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                accepted = self.listener.accept() => {
                    let (socket, _) = accepted?;
                    while connections.try_join_next().is_some() {}
                    if connections.len() >= MAX_CALLBACK_CONNECTIONS {
                        continue;
                    }
                    let state = self.state.clone();
                    let sender = sender.clone();
                    connections.spawn(async move {
                        if let Some(callback) = serve_callback(socket, &state).await {
                            let _ = sender.send(callback).await;
                        }
                    });
                }
                Some(callback) = receiver.recv() => return callback_outcome(callback),
            }
        }
    }
}

/// Read one callback request and answer it. Returns the parameters of a
/// request that matches this attempt's state.
async fn serve_callback(
    mut socket: tokio::net::TcpStream,
    state: &str,
) -> Option<BTreeMap<String, String>> {
    let read = async {
        let mut bytes = Vec::new();
        let mut buffer = [0; 1024];
        while bytes.len() < 16 * 1024 {
            let count = socket.read(&mut buffer).await?;
            if count == 0 {
                break;
            }
            bytes.extend_from_slice(&buffer[..count]);
            if bytes.windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }
        Ok::<_, std::io::Error>(bytes)
    };
    let Ok(Ok(bytes)) = tokio::time::timeout(Duration::from_secs(2), read).await else {
        return None;
    };
    let parsed = parse_callback(&bytes, state).ok();
    let response = if parsed.is_some() {
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nCache-Control: no-store\r\nConnection: close\r\n\r\nReturn to Polaris Code to finish signing in."
    } else {
        "HTTP/1.1 400 Bad Request\r\nContent-Type: text/plain\r\nCache-Control: no-store\r\nConnection: close\r\n\r\nThis callback does not match the active Polaris Code sign-in."
    };
    let _ = tokio::time::timeout(
        Duration::from_secs(2),
        socket.write_all(response.as_bytes()),
    )
    .await;
    parsed
}

fn parse_callback(bytes: &[u8], state: &str) -> Result<BTreeMap<String, String>, ()> {
    let line = std::str::from_utf8(bytes)
        .map_err(|_| ())?
        .lines()
        .next()
        .ok_or(())?;
    let mut parts = line.split_whitespace();
    if parts.next() != Some("GET") {
        return Err(());
    }
    let target = parts.next().ok_or(())?;
    if !target.starts_with("/auth/callback?") {
        return Err(());
    }
    let url = Url::parse(&format!("http://127.0.0.1{target}")).map_err(|_| ())?;
    let mut pairs = BTreeMap::new();
    for (key, value) in url.query_pairs() {
        if pairs.insert(key.into_owned(), value.into_owned()).is_some() {
            return Err(());
        }
    }
    let supplied = pairs.get("state").ok_or(())?.as_bytes();
    if supplied.len() != state.len()
        || supplied
            .iter()
            .zip(state.as_bytes())
            .fold(0u8, |diff, (left, right)| diff | (left ^ right))
            != 0
    {
        return Err(());
    }
    Ok(pairs)
}

/// Accept a state-matched callback unless it reports an error or, per
/// RFC 9207, names an issuer other than [`ISSUER`].
fn callback_outcome(
    callback: BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>, ChatGptError> {
    if callback.get("iss").is_some_and(|iss| iss != ISSUER) {
        return Err(ChatGptError::message(
            "the sign-in response came from an unexpected issuer; start a new sign-in attempt"
                .into(),
        ));
    }
    if callback
        .get("error")
        .is_some_and(|code| code == "subscription_sharing_user_not_eligible")
    {
        return Err(ChatGptError::message(super::responses::error_message(
            "subscription_sharing_user_not_eligible",
        )));
    }
    if callback.contains_key("error") {
        return Err(ChatGptError::message(
            "sign-in was declined or cancelled; choose a connection to continue".into(),
        ));
    }
    Ok(callback)
}

fn random() -> Result<String, ChatGptError> {
    let mut bytes = [0; 32];
    getrandom::fill(&mut bytes)
        .map_err(|_| ChatGptError::message("secure randomness is unavailable".into()))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

async fn identity_document<T: serde::de::DeserializeOwned>(
    response: reqwest::Response,
) -> Result<T, ChatGptError> {
    if !response.status().is_success() {
        return Err(ChatGptError::identity_unavailable(std::io::Error::other(
            "identity service returned an unexpected HTTP status",
        )));
    }
    bounded_json(response)
        .await
        .map_err(ChatGptError::identity_unavailable)
}

fn validate_identity(
    token: &str,
    client_id: &str,
    keys: &JwkSet,
    validity: IdentityValidity,
) -> Result<Identity, ChatGptError> {
    let invalid = || {
        ChatGptError::message(
            "ChatGPT identity verification failed; sign in to ChatGPT again".into(),
        )
    };
    let header = decode_header(token).map_err(|_| invalid())?;
    if header.alg != jsonwebtoken::Algorithm::RS256 {
        return Err(invalid());
    }
    let kid = header.kid.as_deref().ok_or_else(invalid)?;
    let key = keys.find(kid).ok_or_else(|| {
        ChatGptError::identity_unavailable(std::io::Error::other(
            "identity signing key is unavailable",
        ))
    })?;
    let key = DecodingKey::from_jwk(key).map_err(ChatGptError::identity_unavailable)?;
    let mut validation = Validation::new(jsonwebtoken::Algorithm::RS256);
    validation.set_issuer(&[ISSUER]);
    validation.set_audience(&[client_id]);
    validation.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
    validation.leeway = 30;
    // A saved token rotation can be verified after its ID token expires.
    // Signature and account checks still apply; access-token expiry is checked separately.
    validation.validate_exp = matches!(validity, IdentityValidity::CurrentLogin);
    let identity = decode::<Identity>(token, &key, &validation)
        .map_err(|error| {
            use jsonwebtoken::errors::ErrorKind;
            match error.kind() {
                ErrorKind::InvalidToken
                | ErrorKind::InvalidSignature
                | ErrorKind::MissingRequiredClaim(_)
                | ErrorKind::InvalidClaimFormat(_)
                | ErrorKind::ExpiredSignature
                | ErrorKind::InvalidIssuer
                | ErrorKind::InvalidAudience
                | ErrorKind::InvalidSubject
                | ErrorKind::ImmatureSignature
                | ErrorKind::InvalidAlgorithm
                | ErrorKind::Base64(_)
                | ErrorKind::Json(_)
                | ErrorKind::Utf8(_) => invalid(),
                _ => ChatGptError::identity_unavailable(error),
            }
        })?
        .claims;
    if identity.sub.is_empty() {
        return Err(invalid());
    }
    Ok(identity)
}

#[derive(Clone, Copy)]
enum TokenGrant<'a> {
    AuthorizationCode { client_id: &'a str },
    RefreshToken,
}

async fn token_response(
    response: reqwest::Response,
    grant: TokenGrant<'_>,
) -> Result<TokenResponse, ChatGptError> {
    if !response.status().is_success() {
        let status = response.status();
        let body: serde_json::Value = bounded_json(response).await.unwrap_or_default();
        let code = body["error"]["code"]
            .as_str()
            .or_else(|| body["error"].as_str())
            .unwrap_or("");
        return Err(token_failure(status, code, grant));
    }
    let token: TokenResponse = bounded_json(response)
        .await
        .map_err(ChatGptError::from_generation)?;
    if !token.token_type.eq_ignore_ascii_case("bearer")
        || token.access_token.is_empty()
        || token.expires_in == 0
    {
        return Err(ChatGptError::message(
            "invalid ChatGPT bearer credentials".into(),
        ));
    }
    Ok(token)
}

fn token_failure(status: reqwest::StatusCode, code: &str, grant: TokenGrant<'_>) -> ChatGptError {
    if code == "subscription_sharing_user_not_eligible" {
        return ChatGptError::message(super::responses::error_message(code));
    }
    let rejected = matches!(
        code,
        "invalid_grant"
            | "invalid_refresh_token"
            | "token_expired"
            | "refresh_token_expired"
            | "refresh_token_invalidated"
            | "refresh_token_reused"
    );
    if rejected {
        return match grant {
            TokenGrant::AuthorizationCode { client_id } if code == "invalid_grant" =>
                ChatGptError::code_rejected(client_id),
            TokenGrant::AuthorizationCode { .. } => ChatGptError::message(
                "ChatGPT rejected the authorization code during sign-in; start a fresh sign-in attempt".into(),
            ),
            TokenGrant::RefreshToken => ChatGptError::expired(),
        };
    }
    if status.is_server_error() || matches!(status.as_u16(), 408 | 429) {
        ChatGptError::network(std::io::Error::other(
            "token service temporarily unavailable",
        ))
    } else {
        ChatGptError::message(
            "ChatGPT authorization failed; retry sign-in (saved credentials were kept)".into(),
        )
    }
}

fn account_index(records: &Records, id: &str) -> Result<usize, ChatGptError> {
    records
        .accounts
        .iter()
        .position(|account| account.client_id == id)
        .ok_or_else(|| ChatGptError::message("ChatGPT account is no longer saved".into()))
}

fn signed_out() -> ChatGptError {
    ChatGptError::message("ChatGPT account is signed out; sign in to ChatGPT again".into())
}

fn refreshed_access(account: &Registration) -> Result<String, ChatGptError> {
    if account.identity_pending {
        return Err(ChatGptError::message(
            "ChatGPT identity still needs verification; retry shortly".into(),
        ));
    }
    let tokens = account.tokens.as_ref().ok_or_else(signed_out)?;
    if !tokens.plan_enabled() {
        return Err(plan_disabled());
    }
    if tokens.expires_at <= now() {
        return Err(ChatGptError::message(
            "refreshed ChatGPT access has already expired; retry sign-in".into(),
        ));
    }
    Ok(tokens.access_token.clone())
}

fn plan_disabled() -> ChatGptError {
    ChatGptError::message(
        "ChatGPT plan usage is not enabled for this account; enable it or choose another connection"
            .into(),
    )
}

fn valid_access(records: &Records, id: &str) -> Result<Option<String>, ChatGptError> {
    let account = &records.accounts[account_index(records, id)?];
    let tokens = account.tokens.as_ref().ok_or_else(signed_out)?;
    if !tokens.plan_enabled() {
        return Err(plan_disabled());
    }
    let current = now();
    Ok((!account.identity_pending
        && (tokens.expires_at > current.saturating_add(60)
            || (tokens.expires_at > current && tokens.earliest_refresh_at > current)))
        .then(|| tokens.access_token.clone()))
}

impl TokenResponse {
    fn into_tokens(self, previous: Option<&Tokens>) -> Tokens {
        Tokens {
            access_token: self.access_token,
            refresh_token: self
                .refresh_token
                .or_else(|| previous.and_then(|t| t.refresh_token.clone())),
            id_token: self
                .id_token
                .or_else(|| previous.and_then(|t| t.id_token.clone())),
            scope: self
                .scope
                .or_else(|| previous.map(|t| t.scope.clone()))
                .unwrap_or_default(),
            expires_at: now().saturating_add(self.expires_in),
            earliest_refresh_at: self.earliest_refresh_at,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ineligible_accounts_stop_without_retrying_or_reporting_expiry() {
        let code = "subscription_sharing_user_not_eligible";
        for grant in [
            TokenGrant::AuthorizationCode {
                client_id: "issued-client",
            },
            TokenGrant::RefreshToken,
        ] {
            let error = token_failure(reqwest::StatusCode::FORBIDDEN, code, grant);
            assert!(error.to_string().contains("unavailable for this account"));
            assert!(error.retry_client_id().is_none());
            assert!(!error.is_expired());
            assert!(!error.is_network());
            assert!(!crate::agents::failure::classify(&error.into_generation()).is_transient());
        }
        let error = callback_outcome(BTreeMap::from([("error".into(), code.into())])).unwrap_err();
        assert!(error.to_string().contains("choose another connection"));
        assert!(error.retry_client_id().is_none());
    }

    #[test]
    fn rejected_sign_in_codes_are_not_reported_as_expired_sessions() {
        let status = reqwest::StatusCode::BAD_REQUEST;
        let error = token_failure(
            status,
            "invalid_grant",
            TokenGrant::AuthorizationCode {
                client_id: "issued-client",
            },
        );
        assert!(!error.is_expired());
        assert!(
            error
                .to_string()
                .contains("authorization code during sign-in")
        );
        assert!(token_failure(status, "invalid_grant", TokenGrant::RefreshToken).is_expired());
        for grant in [
            TokenGrant::AuthorizationCode {
                client_id: "issued-client",
            },
            TokenGrant::RefreshToken,
        ] {
            assert!(
                token_failure(reqwest::StatusCode::SERVICE_UNAVAILABLE, "", grant).is_network()
            );
            let error = token_failure(status, "secret-server-payload", grant);
            assert!(!error.is_expired());
            assert!(!error.to_string().contains("secret-server-payload"));
        }
    }

    fn saved_account() -> Registration {
        Registration {
            client_id: "registered-client".into(),
            subject: "original-user".into(),
            email: "person@example.test".into(),
            tokens: Some(Tokens {
                access_token: "old-access".into(),
                refresh_token: Some("old-refresh".into()),
                id_token: None,
                scope: super::super::PLAN_SCOPE.into(),
                expires_at: now() + 300,
                earliest_refresh_at: 0,
            }),
            usage_acknowledged: true,
            identity_pending: false,
        }
    }

    async fn client_with(records: &Records) -> (tempfile::TempDir, ChatGptStore, ChatGptClient) {
        let dir = tempfile::tempdir().expect("temporary directory");
        let store = ChatGptStore::at(dir.path().join("chatgpt.json"));
        store.write(records).expect("seed records");
        let client = ChatGptClient::new(store.clone()).expect("build client");
        (dir, store, client)
    }

    struct TokenFixture {
        task: tokio::task::JoinHandle<String>,
        received: tokio::sync::oneshot::Receiver<()>,
        respond: Option<tokio::sync::oneshot::Sender<()>>,
    }

    impl Drop for TokenFixture {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    impl TokenFixture {
        fn release(&mut self) {
            self.respond
                .take()
                .unwrap()
                .send(())
                .expect("release response");
        }

        async fn finish(&mut self) -> BTreeMap<String, String> {
            let request = within(&mut self.task).await.expect("token server finishes");
            let (head, body) = request.split_once("\r\n\r\n").expect("HTTP request");
            assert!(head.starts_with("POST /token HTTP/1.1\r\n"));
            url_form(body)
        }
    }

    fn url_form(body: &str) -> BTreeMap<String, String> {
        Url::parse(&format!("http://localhost/?{body}"))
            .unwrap()
            .query_pairs()
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect()
    }

    async fn within<T>(future: impl std::future::Future<Output = T>) -> T {
        tokio::time::timeout(Duration::from_secs(5), future)
            .await
            .expect("operation completes promptly")
    }

    async fn token_service(
        client: &mut ChatGptClient,
        status: &str,
        body: &serde_json::Value,
    ) -> TokenFixture {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let endpoint = format!("http://{address}/token");
        client.token_endpoint = Some(endpoint.clone());
        client.http = Client::builder().no_proxy().build().unwrap();
        // Invalid grants may trigger background revocation; keep that on loopback too.
        client.cached_keys().discovery = Some(Arc::new(Discovery {
            jwks_uri: endpoint.clone(),
            revocation_endpoint: endpoint,
        }));
        let body = body.to_string();
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let (received_tx, received) = tokio::sync::oneshot::channel();
        let (respond, respond_rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buffer = [0; 4096];
            loop {
                let count = socket.read(&mut buffer).await.unwrap();
                assert_ne!(count, 0, "request must be complete");
                request.extend_from_slice(&buffer[..count]);
                assert!(request.len() < 32 * 1024, "bounded fixture request");
                if let Some(end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                    let head = std::str::from_utf8(&request[..end]).unwrap();
                    let length: usize = head
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|value| value.trim().parse().unwrap())
                        })
                        .unwrap();
                    if request.len() >= end + 4 + length {
                        break;
                    }
                }
            }
            let _ = received_tx.send(());
            respond_rx.await.expect("test releases response");
            socket.write_all(response.as_bytes()).await.unwrap();
            String::from_utf8(request).unwrap()
        });
        TokenFixture {
            task,
            received,
            respond: Some(respond),
        }
    }

    fn expired_records() -> Records {
        let mut account = saved_account();
        account.tokens.as_mut().unwrap().expires_at = now().saturating_sub(120);
        Records {
            active: Some(account.client_id.clone()),
            accounts: vec![account],
            ..Records::default()
        }
    }

    fn assert_permanent(error: ChatGptError) {
        assert!(
            !error.is_network(),
            "identity or credential rejection is permanent: {error}"
        );
        assert!(!crate::agents::failure::classify(&error.into_generation()).is_transient());
    }

    async fn browser_callback(pending: &PendingLogin) -> tokio::net::TcpStream {
        let mut socket = tokio::net::TcpStream::connect(pending.listener.local_addr().unwrap())
            .await
            .unwrap();
        socket.write_all(format!("GET /auth/callback?state={}&code=one-use-code&client_id=registered-client HTTP/1.1\r\n\r\n", pending.state).as_bytes()).await.unwrap();
        socket
    }

    #[tokio::test]
    async fn complete_exchanges_code_and_persists_only_the_verified_account() {
        let (jwks, encoding) = signing_key();
        for case in ["new", "returning", "nonce-mismatch", "subject-mismatch"] {
            let returning = matches!(case, "returning" | "subject-mismatch");
            let records = if returning {
                Records {
                    accounts: vec![saved_account()],
                    ..Records::default()
                }
            } else {
                Records::default()
            };
            let (_dir, store, mut client) = client_with(&records).await;
            client.cached_keys().keys = Some(Arc::new(jwks.clone()));
            let pending = client
                .begin(returning.then_some("registered-client"), false)
                .await
                .unwrap();
            let before = serde_json::to_value(store.read().unwrap()).unwrap();
            let nonce = if case == "nonce-mismatch" {
                "other-attempt"
            } else {
                &pending.nonce
            };
            let subject = if case == "subject-mismatch" {
                "different-user"
            } else {
                "original-user"
            };
            let id_token = sign(
                &serde_json::json!({"sub":subject,"email":"verified@example.test","aud":"registered-client","iss":ISSUER,"exp":now()+300,"nonce":nonce}),
                &encoding,
            );
            let mut server = token_service(
                &mut client,
                "200 OK",
                &serde_json::json!({
                    "access_token":"new-access","refresh_token":"new-refresh","id_token":id_token,
                    "scope":SCOPES,"token_type":"Bearer","expires_in":300
                }),
            )
            .await;
            server.release();
            let verifier = pending.verifier.clone();
            let redirect = pending.redirect_uri.clone();
            let _browser = browser_callback(&pending).await;
            let result = within(client.complete(pending)).await;
            let form = server.finish().await;
            assert_eq!(
                form,
                BTreeMap::from([
                    ("grant_type".into(), "authorization_code".into()),
                    ("client_id".into(), "registered-client".into()),
                    ("code".into(), "one-use-code".into()),
                    ("code_verifier".into(), verifier),
                    ("redirect_uri".into(), redirect),
                    ("resource".into(), RESOURCE.into()),
                ])
            );
            let persisted = store.read_async().await.unwrap();
            if matches!(case, "nonce-mismatch" | "subject-mismatch") {
                let error = result.expect_err("unbound identity must be rejected");
                assert!(error.to_string().contains(if case == "nonce-mismatch" {
                    "nonce did not match"
                } else {
                    "different ChatGPT identity"
                }));
                assert_permanent(error);
                assert_eq!(
                    serde_json::to_value(&persisted).unwrap(),
                    before,
                    "{case}: no credentials are persisted"
                );
                continue;
            }
            let summary = result.expect("verified login completes");
            assert_eq!(summary.id, "registered-client");
            assert!(summary.signed_in && summary.plan_enabled);
            assert!(
                !summary.active,
                "sign-in does not implicitly select the account"
            );
            assert_eq!(
                persisted.accounts.len(),
                1,
                "returning sign-in replaces its registration"
            );
            assert_eq!(persisted.active, None);
            let account = &persisted.accounts[0];
            assert_eq!(account.subject, "original-user");
            assert_eq!(account.email, "verified@example.test");
            assert!(!account.identity_pending);
            assert_eq!(account.usage_acknowledged, returning);
            let tokens = account.tokens.as_ref().unwrap();
            assert_eq!(tokens.access_token, "new-access");
            assert_eq!(tokens.refresh_token.as_deref(), Some("new-refresh"));
            assert_eq!(tokens.id_token.as_deref(), Some(id_token.as_str()));
            assert_eq!(tokens.scope, SCOPES);
            assert!(tokens.expires_at > now());
        }
    }

    #[tokio::test]
    async fn expired_access_refreshes_and_persists_replaced_or_omitted_fields() {
        let (jwks, encoding) = signing_key();
        for replacement in [Some("rotated-refresh"), None] {
            let mut records = expired_records();
            records.accounts[0].tokens.as_mut().unwrap().id_token = Some("old-id-token".into());
            let (_dir, store, mut client) = client_with(&records).await;
            client.cached_keys().keys = Some(Arc::new(jwks.clone()));
            let mut body = serde_json::json!({"access_token":"rotated-access","token_type":"Bearer","expires_in":300,"earliest_refresh_at":now()+120});
            let id_token = sign(
                &serde_json::json!({"sub":"original-user","aud":"registered-client","iss":ISSUER,"exp":now()+300}),
                &encoding,
            );
            if let Some(refresh) = replacement {
                body["refresh_token"] = serde_json::json!(refresh);
                body["id_token"] = serde_json::json!(id_token);
                body["scope"] = serde_json::json!(SCOPES);
            }
            let mut server = token_service(&mut client, "200 OK", &body).await;
            server.release();
            assert_eq!(
                within(client.access_token("registered-client"))
                    .await
                    .unwrap(),
                "rotated-access"
            );
            assert_eq!(
                server.finish().await,
                BTreeMap::from([
                    ("grant_type".into(), "refresh_token".into()),
                    ("client_id".into(), "registered-client".into()),
                    ("refresh_token".into(), "old-refresh".into()),
                    ("resource".into(), RESOURCE.into()),
                ])
            );
            let persisted = store.read_async().await.unwrap();
            assert_eq!(persisted.active, records.active);
            let account = &persisted.accounts[0];
            assert!(
                !account.identity_pending,
                "rotated identity was verified before serving access"
            );
            assert_eq!(account.subject, "original-user");
            let tokens = account.tokens.as_ref().unwrap();
            assert_eq!(tokens.access_token, "rotated-access");
            assert_eq!(
                tokens.refresh_token.as_deref(),
                Some(replacement.unwrap_or("old-refresh"))
            );
            assert_eq!(
                tokens.id_token.as_deref(),
                Some(if replacement.is_some() {
                    id_token.as_str()
                } else {
                    "old-id-token"
                })
            );
            assert_eq!(
                tokens.scope,
                if replacement.is_some() {
                    SCOPES
                } else {
                    super::super::PLAN_SCOPE
                }
            );
            assert!(tokens.expires_at > now() + 60);
            assert_eq!(
                tokens.earliest_refresh_at,
                body["earliest_refresh_at"].as_u64().unwrap()
            );
        }
    }

    #[tokio::test]
    async fn refresh_failure_keeps_old_credentials_or_signs_out_on_invalid_grant() {
        for (status, code, transient) in [
            ("503 Service Unavailable", "server_error", true),
            ("429 Too Many Requests", "rate_limited", true),
            ("400 Bad Request", "invalid_grant", false),
        ] {
            let records = expired_records();
            let before = serde_json::to_value(&records).unwrap();
            let (_dir, store, mut client) = client_with(&records).await;
            let mut server =
                token_service(&mut client, status, &serde_json::json!({"error":code})).await;
            server.release();
            let error = within(client.access_token("registered-client"))
                .await
                .unwrap_err();
            assert_eq!(error.is_expired(), !transient);
            assert_eq!(error.is_network(), transient);
            assert_eq!(
                crate::agents::failure::classify(&error.into_generation()).is_transient(),
                transient
            );
            server.finish().await;
            let persisted = store.read_async().await.unwrap();
            if transient {
                assert_eq!(
                    serde_json::to_value(&persisted).unwrap(),
                    before,
                    "all old credentials are kept"
                );
            } else {
                assert_eq!(persisted.active, None);
                assert_eq!(persisted.accounts.len(), 1);
                assert_eq!(persisted.accounts[0].subject, "original-user");
                assert_eq!(persisted.accounts[0].client_id, "registered-client");
                assert!(persisted.accounts[0].tokens.is_none());
                assert!(!persisted.accounts[0].identity_pending);
            }
        }
    }

    #[tokio::test]
    async fn concurrent_expired_access_callers_share_one_refresh() {
        let (_dir, store, mut client) = client_with(&expired_records()).await;
        let mut server = token_service(&mut client, "200 OK", &serde_json::json!({"access_token":"rotated-access","refresh_token":"rotated-refresh","token_type":"Bearer","expires_in":300})).await;
        server.release();
        let other = client.clone();
        let (first, second) = within(async {
            tokio::join!(
                client.access_token("registered-client"),
                other.access_token("registered-client")
            )
        })
        .await;
        assert_eq!(first.unwrap(), "rotated-access");
        assert_eq!(second.unwrap(), "rotated-access");
        server.finish().await;
        assert_eq!(
            refresh_token(&store.read_async().await.unwrap()),
            Some("rotated-refresh")
        );
    }

    #[tokio::test]
    async fn cancelling_access_caller_still_saves_server_rotation() {
        let (_dir, store, mut client) = client_with(&expired_records()).await;
        let mut server = token_service(&mut client, "200 OK", &serde_json::json!({"access_token":"rotated-access","refresh_token":"rotated-refresh","token_type":"Bearer","expires_in":300})).await;
        let caller = tokio::spawn(async move { client.access_token("registered-client").await });
        within(&mut server.received).await.unwrap();
        // The service consumed the old refresh token; delay delivery of its replacement.
        caller.abort();
        assert!(caller.await.unwrap_err().is_cancelled());
        server.release();
        server.finish().await;
        within(async {
            loop {
                let records = store.read_async().await.unwrap();
                if refresh_token(&records) == Some("rotated-refresh") {
                    assert_eq!(
                        valid_access(&records, "registered-client")
                            .unwrap()
                            .as_deref(),
                        Some("rotated-access")
                    );
                    assert!(!records.accounts[0].identity_pending);
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
    }

    fn pending_records() -> Records {
        let mut account = saved_account();
        account.identity_pending = true;
        Records {
            accounts: vec![account],
            active: Some("registered-client".into()),
            ..Records::default()
        }
    }

    fn identity(sub: &str) -> Identity {
        Identity {
            sub: sub.into(),
            email: None,
            nonce: None,
        }
    }

    fn refresh_token(records: &Records) -> Option<&str> {
        records.accounts[0]
            .tokens
            .as_ref()
            .and_then(|tokens| tokens.refresh_token.as_deref())
    }

    /// An RSA key set holding `kid = "test"` and a matching signing key.
    fn signing_key() -> (JwkSet, jsonwebtoken::EncodingKey) {
        use rsa::pkcs1::EncodeRsaPrivateKey;
        use rsa::traits::PublicKeyParts;
        let key = rsa::PrivateKey::new(&mut rand::rng(), 2048).expect("generate RSA key");
        let public = key.to_public_key();
        let jwks = serde_json::from_value(serde_json::json!({"keys":[{
            "kty":"RSA","kid":"test","alg":"RS256","use":"sig",
            "n":URL_SAFE_NO_PAD.encode(public.n().to_be_bytes_trimmed_vartime()),
            "e":URL_SAFE_NO_PAD.encode(public.e().to_be_bytes_trimmed_vartime()),
        }]}))
        .expect("valid JWK set");
        let der = key.to_pkcs1_der().expect("encode RSA key");
        (
            jwks,
            jsonwebtoken::EncodingKey::from_rsa_der(der.as_bytes()),
        )
    }

    fn sign(claims: &serde_json::Value, key: &jsonwebtoken::EncodingKey) -> String {
        let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256);
        header.kid = Some("test".into());
        jsonwebtoken::encode(&header, claims, key).expect("sign identity token")
    }

    #[tokio::test]
    async fn models_retries_rejected_access_once_with_current_credentials() {
        for second_status in ["200 OK", "401 Unauthorized"] {
            let records = Records {
                accounts: vec![saved_account()],
                ..Records::default()
            };
            let (_dir, store, client) = client_with(&records).await;
            let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
                .await
                .expect("bind model service");
            let address = listener.local_addr().expect("model service address");
            let server = tokio::spawn(async move {
                for (attempt, token) in ["old-access", "new-access"].into_iter().enumerate() {
                    let (mut socket, _) = listener.accept().await.expect("accept catalog request");
                    let mut bytes = [0; 4096];
                    let received = socket.read(&mut bytes).await.expect("read catalog request");
                    let request = String::from_utf8_lossy(&bytes[..received]).to_ascii_lowercase();
                    assert!(request.contains(&format!("authorization: bearer {token}")));
                    if attempt == 0 {
                        // Another process can rotate credentials before this request is rejected.
                        let lock = store.lock().await.expect("credential lock");
                        let mut records = store.read_async().await.expect("read credentials");
                        records.accounts[0]
                            .tokens
                            .as_mut()
                            .expect("signed in")
                            .access_token = "new-access".into();
                        store
                            .write_async(&records, &lock)
                            .await
                            .expect("save rotation");
                    }
                    let status = if attempt == 0 {
                        "401 Unauthorized"
                    } else {
                        second_status
                    };
                    let body = r#"{"models":[{"visibility":"list","slug":"test-model","display_name":"Test model"}]}"#;
                    let response = format!(
                        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    socket
                        .write_all(response.as_bytes())
                        .await
                        .expect("catalog response");
                }
            });
            let result = tokio::time::timeout(
                Duration::from_secs(5),
                client.models_at("registered-client", &format!("http://{address}/models")),
            )
            .await
            .expect("catalog request terminates after at most one retry");
            if second_status == "200 OK" {
                assert_eq!(
                    result.expect("catalog after retry"),
                    vec![("test-model".into(), "Test model".into())]
                );
            } else {
                assert!(
                    result
                        .expect_err("second rejection is returned")
                        .to_string()
                        .contains("401")
                );
            }
            server.await.expect("model service finishes");
        }
    }

    #[tokio::test]
    async fn finish_identity_check_keeps_rotation_pending_on_network_failure() {
        let mut records = Records {
            accounts: vec![saved_account()],
            active: Some("registered-client".into()),
            ..Records::default()
        };
        let (_dir, store, client) = client_with(&records).await;
        let lock = store.lock().await.expect("credential lock");
        client
            .persist_rotation(
                &mut records,
                0,
                TokenResponse {
                    access_token: "rotated-access".into(),
                    refresh_token: Some("rotated-refresh".into()),
                    id_token: Some("identity-to-verify".into()),
                    scope: None,
                    token_type: "Bearer".into(),
                    expires_in: 300,
                    earliest_refresh_at: 0,
                },
                &lock,
            )
            .await
            .expect("persist the rotation");
        let error = client
            .finish_identity_check(
                &mut records,
                0,
                Err(ChatGptError::network(std::io::Error::other(
                    "JWKS unavailable",
                ))),
                &lock,
            )
            .await
            .expect_err("a network failure is returned unchanged");
        assert!(error.is_network(), "the failure stays retryable");
        let mut restarted = store.read_async().await.expect("read records");
        assert_eq!(
            refresh_token(&restarted),
            Some("rotated-refresh"),
            "the rotated refresh token is durable"
        );
        assert_eq!(
            valid_access(&restarted, "registered-client").expect("account is valid"),
            None,
            "unverified access is not served"
        );
        assert!(
            store.accounts_async().await.expect("read summaries")[0].plan_enabled,
            "pending verification preserves the grant for restart discovery"
        );
        let (outcome, revocable) = client
            .finish_identity_check(&mut restarted, 0, Ok(identity("original-user")), &lock)
            .await
            .expect("record the verification");
        outcome.expect("the same subject verifies");
        assert_eq!(
            revocable, None,
            "a verified account keeps its refresh token"
        );
        let verified = store.read_async().await.expect("read records");
        assert_eq!(
            valid_access(&verified, "registered-client")
                .expect("account is valid")
                .as_deref(),
            Some("rotated-access"),
            "verified rotated access is served"
        );
    }

    #[tokio::test]
    async fn identity_document_treats_unusable_metadata_as_unavailable() {
        let mut records = pending_records();
        records.accounts[0]
            .tokens
            .as_mut()
            .expect("seeded tokens")
            .refresh_token = Some("durable-rotated-refresh".into());
        let (_dir, store, client) = client_with(&records).await;
        let lock = store.lock().await.expect("credential lock");
        for (status, body) in [
            ("200 OK", "<html>upstream unavailable</html>"),
            ("302 Found", "{}"),
            ("503 Service Unavailable", "{}"),
        ] {
            let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
                .await
                .expect("bind test server");
            let address = listener.local_addr().expect("test server address");
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.expect("accept request");
                let mut request = [0; 2048];
                let received = socket.read(&mut request).await.expect("read request");
                assert!(received > 0, "identity metadata request must not be empty");
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket
                    .write_all(response.as_bytes())
                    .await
                    .expect("write response");
            });
            let response = client
                .http
                .get(format!("http://{address}"))
                .send()
                .await
                .expect("reach test server");
            let error = identity_document::<JwkSet>(response)
                .await
                .expect_err("unusable metadata is an error");
            assert!(
                error.is_network(),
                "{status}: metadata failure is retryable"
            );
            client
                .finish_identity_check(&mut records, 0, Err(error), &lock)
                .await
                .expect_err("a network failure is returned unchanged");
            let persisted = store.read_async().await.expect("read records");
            assert!(
                persisted.accounts[0].identity_pending,
                "{status}: the rotation stays pending"
            );
            assert_eq!(
                refresh_token(&persisted),
                Some("durable-rotated-refresh"),
                "{status}: the rotated refresh token is kept"
            );
            server.await.expect("test server finishes");
        }
    }

    #[tokio::test]
    async fn finish_identity_check_signs_out_a_changed_identity() {
        let mut records = pending_records();
        let (_dir, store, client) = client_with(&records).await;
        let lock = store.lock().await.expect("credential lock");
        let (outcome, revocable) = client
            .finish_identity_check(&mut records, 0, Ok(identity("different-user")), &lock)
            .await
            .expect("record the verification");
        let error = outcome.expect_err("a different subject is rejected");
        assert!(
            error.to_string().contains("identity changed"),
            "the message names the cause"
        );
        assert_eq!(
            revocable.as_deref(),
            Some("old-refresh"),
            "the rejected refresh token is returned for revocation"
        );
        let persisted = store.read_async().await.expect("read records");
        assert!(
            persisted.accounts[0].tokens.is_none(),
            "the tokens are cleared"
        );
        assert_eq!(persisted.active, None, "the account is deselected");
    }

    #[tokio::test]
    async fn access_token_does_not_wait_for_the_refresh_lock() {
        let (_dir, store, client) = client_with(&Records {
            accounts: vec![saved_account()],
            ..Records::default()
        })
        .await;
        let _lock = store.lock().await.expect("hold the lock");
        let token = tokio::time::timeout(
            Duration::from_secs(2),
            client.access_token("registered-client"),
        )
        .await
        .expect("a valid token is served without the lock")
        .expect("a valid token is available");
        assert_eq!(token, "old-access", "the saved token is served");
    }

    #[tokio::test]
    async fn invalidate_access_expires_only_the_rejected_token() {
        let records = Records {
            accounts: vec![saved_account()],
            ..Records::default()
        };
        let (_dir, store, client) = client_with(&records).await;
        client
            .invalidate_access("registered-client", "superseded-access")
            .await
            .expect("invalidate a stale token");
        assert_eq!(
            valid_access(
                &store.read_async().await.expect("read records"),
                "registered-client"
            )
            .expect("account is valid")
            .as_deref(),
            Some("old-access"),
            "a newer saved token is left alone"
        );
        client
            .invalidate_access("registered-client", "old-access")
            .await
            .expect("invalidate the rejected token");
        assert_eq!(
            valid_access(
                &store.read_async().await.expect("read records"),
                "registered-client"
            )
            .expect("account is valid"),
            None,
            "the rejected token is no longer served"
        );
    }

    #[test]
    fn refreshed_access_serves_short_lived_verified_tokens() {
        let mut account = saved_account();
        account.tokens.as_mut().expect("seeded tokens").expires_at = now() + 30;
        let records = Records {
            accounts: vec![account.clone()],
            ..Records::default()
        };
        assert_eq!(
            valid_access(&records, "registered-client").expect("account is valid"),
            None,
            "short lifetime requests a proactive refresh"
        );
        assert_eq!(
            refreshed_access(&account).expect("fresh token is usable"),
            "old-access",
            "the fresh token is nevertheless valid for this request"
        );
        account.identity_pending = true;
        assert!(
            refreshed_access(&account).is_err(),
            "an unverified token is not served"
        );
        account.identity_pending = false;
        account.tokens.as_mut().expect("seeded tokens").expires_at = now().saturating_sub(1);
        assert!(
            refreshed_access(&account).is_err(),
            "an expired token is not served"
        );
    }

    #[test]
    fn chatgpt_error_network_stays_retryable() {
        let error = ChatGptError::network(std::io::Error::other("offline"));
        assert!(!error.is_expired(), "a network error is not an expiry");
        assert!(
            std::error::Error::source(&error).is_some(),
            "the transport source is retained"
        );
        assert!(
            crate::agents::failure::classify(&error.into_generation()).is_transient(),
            "network errors are retried"
        );
        assert!(
            !crate::agents::failure::classify(&ChatGptError::expired().into_generation())
                .is_transient(),
            "an expired session is not retried"
        );
    }

    #[test]
    fn validate_identity_requires_client_issuer_expiry_and_signature() {
        let (jwks, encoding) = signing_key();
        let claims = serde_json::json!({"sub":"user-1","email":"person@example.test","aud":"issued-client","iss":ISSUER,"exp":now()+300,"nonce":"attempt-nonce"});
        let token = sign(&claims, &encoding);
        let current = IdentityValidity::CurrentLogin;
        let identity = validate_identity(&token, "issued-client", &jwks, current)
            .expect("a well-formed token verifies");
        assert_eq!(identity.sub, "user-1", "the subject is read");
        assert_eq!(
            identity.nonce.as_deref(),
            Some("attempt-nonce"),
            "the nonce is read"
        );
        assert_permanent(
            validate_identity(&token, "another-client", &jwks, current)
                .err()
                .expect("the audience must be the issued client"),
        );
        for (field, value) in [
            ("iss", serde_json::json!("https://example.test")),
            ("exp", serde_json::json!(now() - 120)),
            ("sub", serde_json::json!("")),
        ] {
            let mut invalid = claims.clone();
            invalid[field] = value;
            let signed = sign(&invalid, &encoding);
            assert_permanent(
                validate_identity(&signed, "issued-client", &jwks, current)
                    .err()
                    .unwrap_or_else(|| panic!("a sign-in must validate {field}")),
            );
            let recovered = validate_identity(
                &signed,
                "issued-client",
                &jwks,
                IdentityValidity::PendingRotation,
            );
            if field == "exp" {
                assert_eq!(
                    recovered.expect("expired rotation verifies").sub,
                    "user-1",
                    "pending rotations may recover after the ID token expires"
                );
            } else {
                assert_permanent(
                    recovered
                        .err()
                        .unwrap_or_else(|| panic!("recovery must still validate {field}")),
                );
            }
        }
        let mut altered = token.into_bytes();
        let index = altered.len() - 8;
        altered[index] = if altered[index] == b'A' { b'B' } else { b'A' };
        let altered = String::from_utf8(altered).expect("token stays ASCII");
        assert_permanent(
            validate_identity(&altered, "issued-client", &jwks, current)
                .err()
                .expect("a tampered signature is rejected"),
        );
    }

    #[tokio::test]
    async fn identity_uses_cached_signing_keys() {
        let (jwks, encoding) = signing_key();
        let (_dir, _store, client) = client_with(&Records::default()).await;
        client.cached_keys().keys = Some(Arc::new(jwks));
        let token = sign(
            &serde_json::json!({"sub":"user-1","aud":"issued-client","iss":ISSUER,"exp":now()+300}),
            &encoding,
        );
        let identity = client
            .identity(&token, "issued-client", IdentityValidity::CurrentLogin)
            .await
            .expect("cached keys verify without fetching");
        assert_eq!(identity.sub, "user-1", "the cached key verified the token");
    }

    #[test]
    fn parse_callback_requires_exact_path_unique_parameters_and_state() {
        for target in [
            "/callback?state=expected&code=x",
            "/auth/callback?state=wrong&code=x",
            "/auth/callback?state=expectee&code=x",
            "/auth/callback?state=expecte&code=x",
            "/auth/callback?state=expectedx&code=x",
            "/auth/callback?code=x",
            "/auth/callback?state=expected&state=wrong&code=x",
        ] {
            assert!(
                parse_callback(
                    format!("GET {target} HTTP/1.1\r\n\r\n").as_bytes(),
                    "expected"
                )
                .is_err(),
                "{target} is not this attempt's callback"
            );
        }
        let valid = parse_callback(
            b"GET /auth/callback?state=expected&code=a%2Bb&client_id=issued HTTP/1.1\r\n\r\n",
            "expected",
        )
        .expect("a matching callback parses");
        assert_eq!(valid["code"], "a+b", "parameters are percent-decoded");
    }

    #[test]
    fn callback_outcome_rejects_foreign_issuer_and_errors() {
        let callback = |pairs: &[(&str, &str)]| -> BTreeMap<String, String> {
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect()
        };
        assert!(
            callback_outcome(callback(&[("code", "x"), ("iss", ISSUER)])).is_ok(),
            "the expected issuer is accepted"
        );
        assert!(
            callback_outcome(callback(&[("code", "x")])).is_ok(),
            "an absent issuer is accepted"
        );
        let error = callback_outcome(callback(&[("code", "x"), ("iss", "https://evil.test")]))
            .expect_err("RFC 9207 rejects a foreign issuer");
        assert!(
            error.to_string().contains("unexpected issuer"),
            "the message names the cause"
        );
        assert!(
            callback_outcome(callback(&[("error", "access_denied")])).is_err(),
            "a declined sign-in is reported"
        );
    }

    #[tokio::test]
    async fn callback_is_not_blocked_by_a_stalled_connection() {
        let (_dir, _store, client) = client_with(&Records::default()).await;
        let pending = client.begin(None, false).await.expect("begin sign-in");
        let address = pending.listener.local_addr().expect("callback address");
        let state = pending.state.clone();
        // Hold a connection open without sending a request.
        let _stalled = tokio::net::TcpStream::connect(address)
            .await
            .expect("open a stalled connection");
        let browser = tokio::spawn(async move {
            let mut socket = tokio::net::TcpStream::connect(address)
                .await
                .expect("connect as the browser");
            let request = format!("GET /auth/callback?state={state}&code=c HTTP/1.1\r\n\r\n");
            socket
                .write_all(request.as_bytes())
                .await
                .expect("send the callback");
            let mut response = Vec::new();
            socket
                .read_to_end(&mut response)
                .await
                .expect("read the response");
            response
        });
        let callback = tokio::time::timeout(Duration::from_millis(1500), pending.callback())
            .await
            .expect("the callback is served before the stalled read times out")
            .expect("a matching callback is accepted");
        assert_eq!(callback["code"], "c", "the browser's code is returned");
        let response = browser.await.expect("browser task finishes");
        assert!(
            response.starts_with(b"HTTP/1.1 200"),
            "the browser is told sign-in succeeded"
        );
    }

    #[tokio::test]
    async fn callback_caps_stalled_connections_and_recovers_for_the_browser() {
        let (_dir, _store, client) = client_with(&Records::default()).await;
        let pending = client.begin(None, false).await.unwrap();
        let address = pending.listener.local_addr().unwrap();
        let state = pending.state.clone();
        let callback = tokio::spawn(async move { pending.callback().await });
        let mut stalled = Vec::new();
        for _ in 0..MAX_CALLBACK_CONNECTIONS {
            stalled.push(tokio::net::TcpStream::connect(address).await.unwrap());
        }
        for _ in 0..3 {
            let mut excess = tokio::net::TcpStream::connect(address).await.unwrap();
            let mut byte = [0];
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(1), excess.read(&mut byte))
                    .await
                    .expect("excess connections close before the two-second read timeout")
                    .unwrap(),
                0
            );
        }
        // Free one admitted slot while the other peers remain stalled.
        let mut finished = stalled.pop().unwrap();
        finished
            .write_all(b"GET /auth/callback?state=wrong&code=x HTTP/1.1\r\n\r\n")
            .await
            .unwrap();
        let mut response = Vec::new();
        within(finished.read_to_end(&mut response)).await.unwrap();
        assert!(response.starts_with(b"HTTP/1.1 400"));
        let mut browser = tokio::net::TcpStream::connect(address).await.unwrap();
        browser
            .write_all(
                format!("GET /auth/callback?state={state}&code=real-code HTTP/1.1\r\n\r\n")
                    .as_bytes(),
            )
            .await
            .unwrap();
        response.clear();
        within(browser.read_to_end(&mut response)).await.unwrap();
        let result = within(callback).await.unwrap().unwrap();
        assert_eq!(result["code"], "real-code");
        assert!(response.starts_with(b"HTTP/1.1 200"));
    }

    #[tokio::test]
    async fn explicit_logout_removes_only_the_selected_account() {
        for already_signed_out in [false, true] {
            let mut account = saved_account();
            if already_signed_out {
                account.tokens = None;
            } else {
                account.tokens.as_mut().unwrap().refresh_token = None;
            }
            let mut other = saved_account();
            other.client_id = "other-client".into();
            other.email = "other@example.test".into();
            let records = Records {
                host_id: "existing-host".into(),
                active: Some(account.client_id.clone()),
                accounts: vec![account, other],
            };
            let (_dir, store, client) = client_with(&records).await;
            client.sign_out("registered-client").await.unwrap();
            let saved = store.read().unwrap();
            assert_eq!(saved.host_id, "existing-host");
            assert_eq!(saved.active, None);
            assert_eq!(saved.accounts.len(), 1);
            assert_eq!(saved.accounts[0].client_id, "other-client");
            assert_eq!(saved.accounts[0].email, "other@example.test");
            assert!(saved.accounts[0].tokens.is_some());
            let summaries = store.accounts_async().await.unwrap();
            assert_eq!(summaries.len(), 1);
            assert_eq!(summaries[0].id, "other-client");
            let error = client
                .begin(Some("registered-client"), false)
                .await
                .unwrap_err();
            assert!(error.to_string().contains("no longer saved"));
            let new_login = client.begin(None, false).await.unwrap();
            let query: BTreeMap<_, _> = new_login.url.query_pairs().collect();
            assert_eq!(query["client_id"], "dynamic_agent_client");
            assert!(!query.contains_key("login_hint"));
        }
    }

    #[tokio::test]
    async fn legacy_signed_out_accounts_do_not_show_or_send_a_saved_name() {
        let mut account = saved_account();
        account.tokens = None;
        let (_dir, store, client) = client_with(&Records {
            accounts: vec![account],
            ..Records::default()
        })
        .await;
        let summaries = store.accounts_async().await.unwrap();
        assert!(summaries[0].label.starts_with("ChatGPT account"));
        assert!(!summaries[0].label.contains("person@example.test"));
        let pending = client
            .begin(Some("registered-client"), false)
            .await
            .unwrap();
        assert!(
            !pending
                .url
                .query_pairs()
                .any(|(key, _)| key == "login_hint")
        );
    }

    #[tokio::test]
    async fn code_rejection_reuses_registration_with_fresh_attempt_secrets() {
        let (_dir, store, client) = client_with(&Records::default()).await;
        let first = client.begin(None, false).await.unwrap();
        let error = token_failure(
            reqwest::StatusCode::BAD_REQUEST,
            "invalid_grant",
            TokenGrant::AuthorizationCode {
                client_id: "issued-client",
            },
        );
        let retry = client
            .begin_with_client(None, false, error.retry_client_id())
            .await
            .unwrap();
        let pairs = |pending: &PendingLogin| -> BTreeMap<String, String> {
            pending
                .url
                .query_pairs()
                .map(|(key, value)| (key.into_owned(), value.into_owned()))
                .collect()
        };
        let first_params = pairs(&first);
        let retry_params = pairs(&retry);
        assert_eq!(retry_params["client_id"], "issued-client");
        assert!(!retry_params.contains_key("agent_name_hint"));
        assert_eq!(
            retry_params["ext_agent_host_id"],
            first_params["ext_agent_host_id"]
        );
        for key in ["state", "nonce", "code_challenge"] {
            assert_ne!(retry_params[key], first_params[key]);
        }
        assert_ne!(first.verifier, retry.verifier);
        assert!(
            store.read().unwrap().accounts.is_empty(),
            "unverified registration is not activated or saved"
        );

        // A new attempt must still reject a callback naming another client.
        let address = retry.listener.local_addr().unwrap();
        let state = retry.state.clone();
        let completion = tokio::spawn(async move { client.complete(retry).await });
        let mut socket = tokio::net::TcpStream::connect(address).await.unwrap();
        socket.write_all(format!("GET /auth/callback?state={state}&code=one-use-code&client_id=other-client HTTP/1.1\r\n\r\n").as_bytes()).await.unwrap();
        let error = completion.await.unwrap().unwrap_err();
        assert!(error.to_string().contains("different client registration"));
        assert!(store.read().unwrap().accounts.is_empty());
    }

    #[tokio::test]
    async fn begin_keeps_registration_and_host_boundaries() {
        let (_dir, store, client) = client_with(&Records::default()).await;
        let first = client.begin(None, false).await.expect("begin new sign-in");
        let pairs: BTreeMap<_, _> = first
            .url
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        assert_eq!(
            pairs["agent_name_hint"], "Polaris Code",
            "a new registration names the agent"
        );
        assert_eq!(
            pairs["client_id"], "dynamic_agent_client",
            "a new registration uses dynamic registration"
        );
        assert_eq!(pairs["resource"], RESOURCE, "the resource is the API");
        assert_eq!(pairs["code_challenge_method"], "S256", "PKCE uses S256");
        assert!(
            pairs["redirect_uri"].starts_with("http://127.0.0.1:"),
            "the redirect is loopback"
        );
        let second = client
            .begin(None, false)
            .await
            .expect("begin another sign-in");
        assert_ne!(first.state, second.state, "each attempt has its own state");
        assert_ne!(first.nonce, second.nonce, "each attempt has its own nonce");
        let host = |login: &PendingLogin| {
            login
                .url
                .query_pairs()
                .find(|(k, _)| k == "ext_agent_host_id")
                .map(|(_, v)| v.into_owned())
        };
        assert_eq!(host(&first), host(&second), "the host id is stable");
        let mut records = store.read_async().await.expect("read records");
        let mut account = saved_account();
        account.tokens.as_mut().expect("seeded tokens").id_token =
            Some("private-identity-token".into());
        records.accounts.push(account);
        store.write(&records).expect("save account");
        let returning = client
            .begin(Some("registered-client"), false)
            .await
            .expect("begin returning sign-in");
        assert!(
            returning.authorization_url().contains("login_hint="),
            "a returning sign-in hints the email"
        );
        assert!(
            !returning.authorization_url().contains("id_token_hint"),
            "the identity token is never sent as a hint"
        );
        assert!(
            !format!("{returning:?} {client:?}").contains("private-identity-token"),
            "debug output omits tokens"
        );
        let address = first.listener.local_addr().expect("callback address");
        drop(first);
        let _released_port = std::net::TcpListener::bind(address)
            .expect("dropping the authorization attempt releases its callback port");
    }
}
