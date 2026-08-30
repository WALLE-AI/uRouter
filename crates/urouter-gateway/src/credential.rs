use std::{
    collections::BTreeMap,
    env,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::{
    fs,
    io::AsyncWriteExt,
    sync::{Mutex, RwLock},
    time::sleep,
};
use urouter_ai::auth::AuthPlan;

const REFRESH_SKEW_SECONDS: u64 = 30;
const LOCK_WAIT_ATTEMPTS: usize = 100;

#[derive(Clone)]
pub(crate) struct CredentialManager {
    client: reqwest::Client,
    locks: Arc<RwLock<BTreeMap<String, Arc<Mutex<()>>>>>,
}

impl CredentialManager {
    pub(crate) fn new(client: reqwest::Client) -> Self {
        Self {
            client,
            locks: Arc::default(),
        }
    }

    pub(crate) async fn resolve(
        &self,
        plan: &AuthPlan,
    ) -> Result<Option<(String, String)>, CredentialError> {
        match plan {
            AuthPlan::ApiKeyEnv {
                env: variable,
                header,
                prefix,
            }
            | AuthPlan::AmbientEnv {
                env: variable,
                header,
                prefix,
            } => {
                let secret = env::var(variable).map_err(|_| CredentialError::SourceUnavailable)?;
                Ok(Some((header.clone(), format!("{prefix}{secret}"))))
            }
            AuthPlan::OAuthBearerFile {
                path,
                header,
                prefix,
            } => {
                let secret = fs::read_to_string(path)
                    .await
                    .map_err(|_| CredentialError::SourceUnavailable)?;
                let secret = secret.trim();
                if secret.is_empty() {
                    return Err(CredentialError::EmptyCredential);
                }
                Ok(Some((header.clone(), format!("{prefix}{secret}"))))
            }
            AuthPlan::OAuthClientCredentials {
                token_url,
                client_id_env,
                client_secret_env,
                token_file,
                scope,
                audience,
                header,
                prefix,
            } => {
                let token = self
                    .oauth_token(
                        token_url,
                        client_id_env,
                        client_secret_env,
                        token_file,
                        scope.as_deref(),
                        audience.as_deref(),
                    )
                    .await?;
                Ok(Some((header.clone(), format!("{prefix}{token}"))))
            }
            AuthPlan::None => Ok(None),
        }
    }

    async fn oauth_token(
        &self,
        token_url: &str,
        client_id_env: &str,
        client_secret_env: &str,
        token_file: &str,
        scope: Option<&str>,
        audience: Option<&str>,
    ) -> Result<String, CredentialError> {
        let lock = {
            let mut locks = self.locks.write().await;
            Arc::clone(
                locks
                    .entry(token_file.to_owned())
                    .or_insert_with(|| Arc::new(Mutex::new(()))),
            )
        };
        let _guard = lock.lock().await;
        let token_path = Path::new(token_file);
        let lock_path = lock_path(token_path);
        if !fs::try_exists(&lock_path).await.unwrap_or(true)
            && let Some(token) = read_valid_token(token_path).await
        {
            return Ok(token.access_token);
        }
        let lock_file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock_path)
            .await;
        if lock_file.is_err() {
            for _ in 0..LOCK_WAIT_ATTEMPTS {
                sleep(Duration::from_millis(50)).await;
                if !fs::try_exists(&lock_path).await.unwrap_or(true)
                    && let Some(token) = read_valid_token(token_path).await
                {
                    return Ok(token.access_token);
                }
            }
            return Err(CredentialError::RefreshLockTimeout);
        }
        let result = self
            .refresh_token(
                token_url,
                client_id_env,
                client_secret_env,
                token_path,
                scope,
                audience,
            )
            .await;
        let _ = fs::remove_file(&lock_path).await;
        result
    }

    async fn refresh_token(
        &self,
        token_url: &str,
        client_id_env: &str,
        client_secret_env: &str,
        token_path: &Path,
        scope: Option<&str>,
        audience: Option<&str>,
    ) -> Result<String, CredentialError> {
        let client_id = env::var(client_id_env).map_err(|_| CredentialError::SourceUnavailable)?;
        let client_secret =
            env::var(client_secret_env).map_err(|_| CredentialError::SourceUnavailable)?;
        let mut form = vec![
            ("grant_type", "client_credentials".to_owned()),
            ("client_id", client_id),
            ("client_secret", client_secret),
        ];
        if let Some(scope) = scope {
            form.push(("scope", scope.to_owned()));
        }
        if let Some(audience) = audience {
            form.push(("audience", audience.to_owned()));
        }
        let response = self
            .client
            .post(token_url)
            .form(&form)
            .send()
            .await
            .map_err(|_| CredentialError::RefreshFailed)?;
        if !response.status().is_success() {
            return Err(CredentialError::RefreshFailed);
        }
        let response: OAuthResponse = response
            .json()
            .await
            .map_err(|_| CredentialError::InvalidResponse)?;
        if response.access_token.trim().is_empty() || response.expires_in == 0 {
            return Err(CredentialError::InvalidResponse);
        }
        let snapshot = TokenSnapshot {
            access_token: response.access_token,
            expires_at_unix_s: unix_seconds().saturating_add(response.expires_in),
        };
        if let Some(parent) = token_path.parent() {
            fs::create_dir_all(parent).await?;
        }
        let temporary = PathBuf::from(format!("{}.refresh.tmp", token_path.display()));
        let bytes = serde_json::to_vec(&snapshot).map_err(|_| CredentialError::InvalidResponse)?;
        let mut file = fs::File::create(&temporary).await?;
        file.write_all(&bytes).await?;
        file.sync_all().await?;
        drop(file);
        if fs::try_exists(token_path).await.unwrap_or(false) {
            fs::remove_file(token_path).await?;
        }
        fs::rename(&temporary, token_path).await?;
        Ok(snapshot.access_token)
    }
}

#[derive(Debug, Deserialize)]
struct OAuthResponse {
    access_token: String,
    expires_in: u64,
}

#[derive(Debug, Serialize, Deserialize)]
struct TokenSnapshot {
    access_token: String,
    expires_at_unix_s: u64,
}

async fn read_valid_token(path: &Path) -> Option<TokenSnapshot> {
    let bytes = fs::read(path).await.ok()?;
    let token = serde_json::from_slice::<TokenSnapshot>(&bytes).ok()?;
    (token.expires_at_unix_s > unix_seconds().saturating_add(REFRESH_SKEW_SECONDS)
        && !token.access_token.trim().is_empty())
    .then_some(token)
}

fn lock_path(path: &Path) -> PathBuf {
    PathBuf::from(format!("{}.lock", path.display()))
}

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[derive(Debug, Error)]
pub(crate) enum CredentialError {
    #[error("credential source is unavailable")]
    SourceUnavailable,
    #[error("credential source is empty")]
    EmptyCredential,
    #[error("OAuth refresh failed")]
    RefreshFailed,
    #[error("OAuth response is invalid")]
    InvalidResponse,
    #[error("OAuth refresh lock timed out")]
    RefreshLockTimeout,
    #[error("credential persistence failed")]
    Io(#[from] std::io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    use axum::{Json, Router, extract::State, http::StatusCode, routing::post};
    use serde_json::json;

    static TEST_ID: AtomicU64 = AtomicU64::new(0);

    fn root() -> PathBuf {
        std::env::temp_dir().join(format!(
            "urouter-oauth-{}-{}",
            std::process::id(),
            TEST_ID.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn plan(url: String, token_file: &Path) -> AuthPlan {
        AuthPlan::OAuthClientCredentials {
            token_url: url,
            client_id_env: "PATH".to_owned(),
            client_secret_env: "PATH".to_owned(),
            token_file: token_file.display().to_string(),
            scope: Some("models.read".to_owned()),
            audience: None,
            header: "authorization".to_owned(),
            prefix: "Bearer ".to_owned(),
        }
    }

    async fn server(app: Router) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{address}/token"), handle)
    }

    #[tokio::test]
    async fn expired_token_refresh_is_single_flight_across_managers() {
        async fn token(State(calls): State<Arc<AtomicU64>>) -> Json<serde_json::Value> {
            calls.fetch_add(1, Ordering::Relaxed);
            Json(json!({"access_token": "fresh-token", "expires_in": 3600}))
        }
        let calls = Arc::new(AtomicU64::new(0));
        let (url, server) = server(
            Router::new()
                .route("/token", post(token))
                .with_state(Arc::clone(&calls)),
        )
        .await;
        let root = root();
        fs::create_dir_all(&root).await.unwrap();
        let token_file = root.join("token.json");
        fs::write(
            &token_file,
            serde_json::to_vec(&TokenSnapshot {
                access_token: "expired".to_owned(),
                expires_at_unix_s: 1,
            })
            .unwrap(),
        )
        .await
        .unwrap();
        let plan = plan(url, &token_file);
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let mut tasks = Vec::new();
        for _ in 0..8 {
            let manager = CredentialManager::new(client.clone());
            let plan = plan.clone();
            tasks.push(tokio::spawn(async move {
                manager.resolve(&plan).await.unwrap()
            }));
        }
        for task in tasks {
            assert_eq!(
                task.await.unwrap(),
                Some(("authorization".to_owned(), "Bearer fresh-token".to_owned()))
            );
        }
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        server.abort();
        fs::remove_dir_all(root).await.unwrap();
    }

    #[tokio::test]
    async fn refresh_failure_is_redacted_and_does_not_publish_a_token() {
        async fn fail() -> (StatusCode, &'static str) {
            (StatusCode::UNAUTHORIZED, "client_secret=must-not-leak")
        }
        let (url, server) = server(Router::new().route("/token", post(fail))).await;
        let root = root();
        fs::create_dir_all(&root).await.unwrap();
        let token_file = root.join("token.json");
        let manager =
            CredentialManager::new(reqwest::Client::builder().no_proxy().build().unwrap());
        let error = manager.resolve(&plan(url, &token_file)).await.unwrap_err();
        assert_eq!(error.to_string(), "OAuth refresh failed");
        assert!(!fs::try_exists(&token_file).await.unwrap());
        server.abort();
        fs::remove_dir_all(root).await.unwrap();
    }
}
