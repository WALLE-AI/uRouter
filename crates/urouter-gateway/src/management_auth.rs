use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use axum::http::{HeaderMap, header};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use thiserror::Error;
use tokio::{
    io::AsyncWriteExt,
    sync::{RwLock, mpsc, oneshot},
    time::sleep,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ManagementRole {
    Reader,
    Operator,
    Admin,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct KeyringDocument {
    version: u16,
    keys: Vec<ManagementKey>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManagementKey {
    id: String,
    token_sha256: String,
    role: ManagementRole,
    tenant_keys: Vec<String>,
    #[serde(default)]
    not_before_unix_s: Option<u64>,
    #[serde(default)]
    expires_at_unix_s: Option<u64>,
}

#[derive(Debug, Clone)]
struct Keyring {
    keys: Vec<ManagementKey>,
}

#[derive(Debug, Clone)]
pub(crate) struct ManagementPrincipal {
    pub(crate) key_id: String,
    pub(crate) role: ManagementRole,
}

#[derive(Debug, Error)]
pub(crate) enum ManagementAuthError {
    #[error("management authorization header is required")]
    MissingCredential,
    #[error("management authorization credential is invalid or inactive")]
    InvalidCredential,
    #[error("management credential cannot access this tenant")]
    TenantForbidden,
    #[error("management role is insufficient for this action")]
    RoleForbidden,
    #[error("management audit event could not be persisted")]
    AuditUnavailable,
    #[error("management keyring is invalid: {0}")]
    InvalidKeyring(String),
    #[error("management keyring could not be read: {0}")]
    KeyringIo(#[from] std::io::Error),
}

#[derive(Debug, Clone, Serialize)]
struct AuditEvent {
    timestamp_unix_ms: u128,
    key_id: String,
    role: Option<ManagementRole>,
    tenant_key: String,
    action: String,
    target_key: Option<String>,
    allowed: bool,
    reason: String,
}

struct AuditCommand {
    event: AuditEvent,
    completed: oneshot::Sender<bool>,
}

#[derive(Clone)]
struct AuditSink {
    sender: mpsc::Sender<AuditCommand>,
}

#[derive(Clone)]
pub(crate) struct ManagementAuth {
    keyring: Option<Arc<RwLock<Keyring>>>,
    audit: Option<AuditSink>,
}

impl ManagementAuth {
    #[cfg(test)]
    pub(crate) const fn disabled() -> Self {
        Self {
            keyring: None,
            audit: None,
        }
    }

    pub(crate) async fn open(
        keyring_path: Option<PathBuf>,
        audit_path: Option<PathBuf>,
        audit_queue_capacity: usize,
        reload_interval: Duration,
    ) -> Result<Self, ManagementAuthError> {
        if audit_queue_capacity == 0 || reload_interval.is_zero() {
            return Err(ManagementAuthError::InvalidKeyring(
                "audit queue capacity and reload interval must be greater than zero".to_owned(),
            ));
        }
        if keyring_path.is_some() && audit_path.is_none() {
            return Err(ManagementAuthError::InvalidKeyring(
                "--management-audit is required with --management-keyring".to_owned(),
            ));
        }
        let audit = match audit_path {
            Some(path) => Some(AuditSink::open(path, audit_queue_capacity).await?),
            None => None,
        };
        let keyring = match keyring_path {
            Some(path) => {
                let (loaded, fingerprint) = load_keyring(&path).await?;
                let keyring = Arc::new(RwLock::new(loaded));
                spawn_keyring_reload(
                    Arc::clone(&keyring),
                    path,
                    reload_interval,
                    fingerprint,
                    audit.clone(),
                );
                Some(keyring)
            }
            None => None,
        };
        Ok(Self { keyring, audit })
    }

    pub(crate) const fn enabled(&self) -> bool {
        self.keyring.is_some()
    }

    pub(crate) async fn authorize(
        &self,
        headers: &HeaderMap,
        tenant_key: &str,
        required: ManagementRole,
        action: &str,
        target_key: Option<String>,
    ) -> Result<ManagementPrincipal, ManagementAuthError> {
        let Some(keyring) = &self.keyring else {
            let principal = ManagementPrincipal {
                key_id: "local-compatibility".to_owned(),
                role: ManagementRole::Admin,
            };
            self.audit(
                &principal.key_id,
                Some(principal.role),
                tenant_key,
                action,
                target_key,
                true,
                "auth_disabled",
            )
            .await?;
            return Ok(principal);
        };
        let (principal, error) = match bearer_token(headers) {
            Some(token) => {
                let keyring = keyring.read().await;
                match authenticate_credential(&keyring, token) {
                    Ok(key) => {
                        let principal = ManagementPrincipal {
                            key_id: key.id.clone(),
                            role: key.role,
                        };
                        let error = authorize_key(key, tenant_key, required).err();
                        (Some(principal), error)
                    }
                    Err(error) => (None, Some(error)),
                }
            }
            None => (None, Some(ManagementAuthError::MissingCredential)),
        };
        let (key_id, role) = principal
            .as_ref()
            .map_or(("unresolved", None), |principal| {
                (principal.key_id.as_str(), Some(principal.role))
            });
        let (allowed, reason) = match &error {
            Some(error) => (false, error_reason(error)),
            None => (true, "authorized"),
        };
        self.audit(
            key_id, role, tenant_key, action, target_key, allowed, reason,
        )
        .await?;
        match error {
            Some(error) => Err(error),
            None => Ok(principal.expect("authorized management key has a principal")),
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn audit(
        &self,
        key_id: &str,
        role: Option<ManagementRole>,
        tenant_key: &str,
        action: &str,
        target_key: Option<String>,
        allowed: bool,
        reason: &str,
    ) -> Result<(), ManagementAuthError> {
        let Some(audit) = &self.audit else {
            return Ok(());
        };
        audit
            .write(AuditEvent {
                timestamp_unix_ms: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis(),
                key_id: key_id.to_owned(),
                role,
                tenant_key: tenant_key.to_owned(),
                action: action.to_owned(),
                target_key,
                allowed,
                reason: reason.to_owned(),
            })
            .await
    }
}

#[cfg(test)]
#[allow(clippy::items_after_test_module, clippy::needless_pass_by_value)]
mod tests {
    use super::*;
    use serde_json::json;

    fn digest(value: &str) -> String {
        format!("sha256:{:x}", Sha256::digest(value.as_bytes()))
    }

    fn key(id: &str, token: &str, role: &str, tenants: Vec<String>) -> serde_json::Value {
        json!({
            "id": id,
            "token_sha256": digest(token),
            "role": role,
            "tenant_keys": tenants
        })
    }

    fn keyring(keys: Vec<serde_json::Value>) -> String {
        serde_json::to_string(&json!({"version": 1, "keys": keys})).unwrap()
    }

    fn headers(token: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            format!("Bearer {token}").parse().unwrap(),
        );
        headers
    }

    fn temporary_paths(name: &str) -> (PathBuf, PathBuf) {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let base = std::env::temp_dir().join(format!(
            "urouter-management-auth-{name}-{}-{suffix}",
            std::process::id()
        ));
        (
            base.with_extension("json"),
            base.with_extension("audit.jsonl"),
        )
    }

    #[test]
    fn role_tenant_and_activation_are_enforced() {
        let tenant = digest("tenant-a");
        let mut expired = key("expired", "expired-token", "admin", vec!["*".to_owned()]);
        expired["expires_at_unix_s"] = json!(1);
        let parsed = parse_keyring(&keyring(vec![
            key("reader", "reader-token", "reader", vec![tenant.clone()]),
            expired,
        ]))
        .unwrap();

        assert!(authenticate(&parsed, "reader-token", &tenant, ManagementRole::Reader).is_ok());
        assert!(matches!(
            authenticate(&parsed, "reader-token", &tenant, ManagementRole::Operator),
            Err(ManagementAuthError::RoleForbidden)
        ));
        assert!(matches!(
            authenticate(
                &parsed,
                "reader-token",
                &digest("tenant-b"),
                ManagementRole::Reader
            ),
            Err(ManagementAuthError::TenantForbidden)
        ));
        assert!(matches!(
            authenticate(&parsed, "expired-token", &tenant, ManagementRole::Reader),
            Err(ManagementAuthError::InvalidCredential)
        ));
    }

    #[test]
    fn overlapping_rotation_keys_are_both_accepted() {
        let parsed = parse_keyring(&keyring(vec![
            key("old", "old-token", "operator", vec!["*".to_owned()]),
            key("new", "new-token", "operator", vec!["*".to_owned()]),
        ]))
        .unwrap();
        let tenant = digest("tenant-a");

        assert!(authenticate(&parsed, "old-token", &tenant, ManagementRole::Operator).is_ok());
        assert!(authenticate(&parsed, "new-token", &tenant, ManagementRole::Operator).is_ok());
    }

    #[tokio::test]
    async fn authorization_audit_is_synchronous_and_redacted() {
        let (keyring_path, audit_path) = temporary_paths("audit");
        tokio::fs::write(
            &keyring_path,
            keyring(vec![key(
                "reader",
                "reader-secret",
                "reader",
                vec!["*".to_owned()],
            )]),
        )
        .await
        .unwrap();
        let auth = ManagementAuth::open(
            Some(keyring_path.clone()),
            Some(audit_path.clone()),
            8,
            Duration::from_secs(60),
        )
        .await
        .unwrap();
        let tenant = digest("tenant-a");

        auth.authorize(
            &headers("reader-secret"),
            &tenant,
            ManagementRole::Reader,
            "decision.read",
            Some(digest("target")),
        )
        .await
        .unwrap();
        assert!(matches!(
            auth.authorize(
                &HeaderMap::new(),
                &tenant,
                ManagementRole::Reader,
                "decision.read",
                None,
            )
            .await,
            Err(ManagementAuthError::MissingCredential)
        ));
        assert!(matches!(
            auth.authorize(
                &headers("reader-secret"),
                &tenant,
                ManagementRole::Operator,
                "decision.delete",
                None,
            )
            .await,
            Err(ManagementAuthError::RoleForbidden)
        ));

        let audit = tokio::fs::read_to_string(&audit_path).await.unwrap();
        assert_eq!(audit.lines().count(), 3);
        assert!(audit.contains("\"allowed\":true"));
        assert!(audit.contains("\"allowed\":false"));
        assert!(audit.contains("\"key_id\":\"reader\""));
        assert!(audit.contains("\"reason\":\"role_forbidden\""));
        assert!(!audit.contains("reader-secret"));
        let _ = tokio::fs::remove_file(keyring_path).await;
        let _ = tokio::fs::remove_file(audit_path).await;
    }

    #[tokio::test]
    async fn hot_reload_retains_valid_keyring_and_accepts_rotated_key() {
        let (keyring_path, audit_path) = temporary_paths("reload");
        tokio::fs::write(
            &keyring_path,
            keyring(vec![key("old", "old-token", "admin", vec!["*".to_owned()])]),
        )
        .await
        .unwrap();
        let auth = ManagementAuth::open(
            Some(keyring_path.clone()),
            Some(audit_path.clone()),
            8,
            Duration::from_millis(10),
        )
        .await
        .unwrap();
        let tenant = digest("tenant-a");

        tokio::fs::write(&keyring_path, "not-json").await.unwrap();
        sleep(Duration::from_millis(30)).await;
        assert!(
            auth.authorize(
                &headers("old-token"),
                &tenant,
                ManagementRole::Admin,
                "metrics.read",
                None,
            )
            .await
            .is_ok()
        );

        tokio::fs::write(
            &keyring_path,
            keyring(vec![key("new", "new-token", "admin", vec!["*".to_owned()])]),
        )
        .await
        .unwrap();
        sleep(Duration::from_millis(30)).await;
        assert!(
            auth.authorize(
                &headers("new-token"),
                &tenant,
                ManagementRole::Admin,
                "metrics.read",
                None,
            )
            .await
            .is_ok()
        );
        assert!(matches!(
            auth.authorize(
                &headers("old-token"),
                &tenant,
                ManagementRole::Admin,
                "metrics.read",
                None,
            )
            .await,
            Err(ManagementAuthError::InvalidCredential)
        ));
        let audit = tokio::fs::read_to_string(&audit_path).await.unwrap();
        assert!(audit.contains("invalid_keyring_retained_previous"));
        assert!(audit.contains("\"reason\":\"reloaded\""));
        let _ = tokio::fs::remove_file(keyring_path).await;
        let _ = tokio::fs::remove_file(audit_path).await;
    }
}

impl AuditSink {
    async fn open(path: PathBuf, queue_capacity: usize) -> Result<Self, ManagementAuthError> {
        tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await?
            .sync_data()
            .await?;
        let (sender, receiver) = mpsc::channel(queue_capacity);
        tokio::spawn(audit_writer(path, receiver));
        Ok(Self { sender })
    }

    async fn write(&self, event: AuditEvent) -> Result<(), ManagementAuthError> {
        let (completed, receiver) = oneshot::channel();
        self.sender
            .send(AuditCommand { event, completed })
            .await
            .map_err(|_| ManagementAuthError::AuditUnavailable)?;
        if receiver.await.unwrap_or(false) {
            Ok(())
        } else {
            Err(ManagementAuthError::AuditUnavailable)
        }
    }
}

async fn audit_writer(path: PathBuf, mut receiver: mpsc::Receiver<AuditCommand>) {
    while let Some(command) = receiver.recv().await {
        let success = async {
            let mut line = serde_json::to_vec(&command.event).map_err(std::io::Error::other)?;
            line.push(b'\n');
            let mut file = tokio::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .await?;
            file.write_all(&line).await?;
            file.sync_data().await
        }
        .await
        .is_ok();
        let _ = command.completed.send(success);
    }
}

async fn load_keyring(path: &Path) -> Result<(Keyring, String), ManagementAuthError> {
    let contents = tokio::fs::read_to_string(path).await?;
    let fingerprint = format!("sha256:{:x}", Sha256::digest(contents.as_bytes()));
    parse_keyring(&contents).map(|keyring| (keyring, fingerprint))
}

fn parse_keyring(contents: &str) -> Result<Keyring, ManagementAuthError> {
    let document: KeyringDocument = serde_json::from_str(contents)
        .map_err(|error| ManagementAuthError::InvalidKeyring(error.to_string()))?;
    validate_keyring(document)
}

fn validate_keyring(document: KeyringDocument) -> Result<Keyring, ManagementAuthError> {
    if document.version != 1 || document.keys.is_empty() {
        return Err(ManagementAuthError::InvalidKeyring(
            "version must be 1 and keys must not be empty".to_owned(),
        ));
    }
    let mut ids = BTreeSet::new();
    let mut digests = BTreeSet::new();
    for key in &document.keys {
        if key.id.trim().is_empty()
            || key.id.len() > 128
            || !key
                .id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
            || !ids.insert(key.id.clone())
        {
            return Err(ManagementAuthError::InvalidKeyring(
                "key ids must be unique 1..=128 byte ASCII identifiers".to_owned(),
            ));
        }
        if !valid_digest(&key.token_sha256) || !digests.insert(key.token_sha256.clone()) {
            return Err(ManagementAuthError::InvalidKeyring(
                "token_sha256 values must be unique sha256 digests".to_owned(),
            ));
        }
        if key.tenant_keys.is_empty()
            || key
                .tenant_keys
                .iter()
                .any(|tenant| tenant != "*" && !valid_digest(tenant))
        {
            return Err(ManagementAuthError::InvalidKeyring(
                "tenant_keys must contain * or sha256 digests".to_owned(),
            ));
        }
        if key
            .not_before_unix_s
            .zip(key.expires_at_unix_s)
            .is_some_and(|(start, end)| start >= end)
        {
            return Err(ManagementAuthError::InvalidKeyring(
                "key activation interval is empty".to_owned(),
            ));
        }
    }
    Ok(Keyring {
        keys: document.keys,
    })
}

#[cfg(test)]
fn authenticate(
    keyring: &Keyring,
    token: &str,
    tenant_key: &str,
    required: ManagementRole,
) -> Result<ManagementPrincipal, ManagementAuthError> {
    let key = authenticate_credential(keyring, token)?;
    authorize_key(key, tenant_key, required)?;
    Ok(ManagementPrincipal {
        key_id: key.id.clone(),
        role: key.role,
    })
}

fn authenticate_credential<'a>(
    keyring: &'a Keyring,
    token: &str,
) -> Result<&'a ManagementKey, ManagementAuthError> {
    let digest = format!("sha256:{:x}", Sha256::digest(token.as_bytes()));
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    keyring
        .keys
        .iter()
        .find(|key| key.token_sha256.as_bytes().ct_eq(digest.as_bytes()).into())
        .filter(|key| key.not_before_unix_s.is_none_or(|start| start <= now))
        .filter(|key| key.expires_at_unix_s.is_none_or(|end| now < end))
        .ok_or(ManagementAuthError::InvalidCredential)
}

fn authorize_key(
    key: &ManagementKey,
    tenant_key: &str,
    required: ManagementRole,
) -> Result<(), ManagementAuthError> {
    if !key
        .tenant_keys
        .iter()
        .any(|tenant| tenant == "*" || tenant == tenant_key)
    {
        return Err(ManagementAuthError::TenantForbidden);
    }
    if key.role < required {
        return Err(ManagementAuthError::RoleForbidden);
    }
    Ok(())
}

fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.split_once(' ')?;
    (scheme.eq_ignore_ascii_case("bearer") && !token.is_empty() && token.len() <= 512)
        .then_some(token)
}

fn valid_digest(value: &str) -> bool {
    value.len() == 71
        && value.starts_with("sha256:")
        && value[7..]
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn error_reason(error: &ManagementAuthError) -> &'static str {
    match error {
        ManagementAuthError::MissingCredential => "missing_credential",
        ManagementAuthError::InvalidCredential => "invalid_credential",
        ManagementAuthError::TenantForbidden => "tenant_forbidden",
        ManagementAuthError::RoleForbidden => "role_forbidden",
        ManagementAuthError::AuditUnavailable => "audit_unavailable",
        ManagementAuthError::InvalidKeyring(_) => "invalid_keyring",
        ManagementAuthError::KeyringIo(_) => "keyring_io",
    }
}

fn spawn_keyring_reload(
    keyring: Arc<RwLock<Keyring>>,
    path: PathBuf,
    reload_interval: Duration,
    mut fingerprint: String,
    audit: Option<AuditSink>,
) {
    tokio::spawn(async move {
        loop {
            sleep(reload_interval).await;
            let Ok(contents) = tokio::fs::read_to_string(&path).await else {
                continue;
            };
            let observed = format!("sha256:{:x}", Sha256::digest(contents.as_bytes()));
            if observed == fingerprint {
                continue;
            }
            fingerprint = observed.clone();
            let (allowed, reason) = match parse_keyring(&contents) {
                Ok(reloaded) => {
                    *keyring.write().await = reloaded;
                    (true, "reloaded")
                }
                Err(_) => (false, "invalid_keyring_retained_previous"),
            };
            if let Some(audit) = &audit {
                let result = audit
                    .write(AuditEvent {
                        timestamp_unix_ms: SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_millis(),
                        key_id: "system".to_owned(),
                        role: None,
                        tenant_key: "system".to_owned(),
                        action: "management_keyring.reload".to_owned(),
                        target_key: Some(observed),
                        allowed,
                        reason: reason.to_owned(),
                    })
                    .await;
                if result.is_err() {
                    eprintln!("management keyring reload audit could not be persisted");
                }
            }
        }
    });
}
