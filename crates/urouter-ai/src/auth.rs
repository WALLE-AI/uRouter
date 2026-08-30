use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum AuthSpec {
    ApiKeyEnv {
        env: String,
        #[serde(default = "default_authorization_header")]
        header: String,
        #[serde(default = "default_bearer_prefix")]
        prefix: String,
    },
    OAuthBearerFile {
        path: String,
        #[serde(default = "default_authorization_header")]
        header: String,
        #[serde(default = "default_bearer_prefix")]
        prefix: String,
    },
    OAuthClientCredentials {
        token_url: String,
        client_id_env: String,
        client_secret_env: String,
        token_file: String,
        #[serde(default)]
        scope: Option<String>,
        #[serde(default)]
        audience: Option<String>,
        #[serde(default = "default_authorization_header")]
        header: String,
        #[serde(default = "default_bearer_prefix")]
        prefix: String,
    },
    AmbientEnv {
        env: String,
        #[serde(default = "default_authorization_header")]
        header: String,
        #[serde(default = "default_bearer_prefix")]
        prefix: String,
    },
    None {
        #[serde(default)]
        allow_remote: bool,
    },
}

fn default_authorization_header() -> String {
    "authorization".to_owned()
}

fn default_bearer_prefix() -> String {
    "Bearer ".to_owned()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthPlan {
    ApiKeyEnv {
        env: String,
        header: String,
        prefix: String,
    },
    OAuthBearerFile {
        path: String,
        header: String,
        prefix: String,
    },
    OAuthClientCredentials {
        token_url: String,
        client_id_env: String,
        client_secret_env: String,
        token_file: String,
        scope: Option<String>,
        audience: Option<String>,
        header: String,
        prefix: String,
    },
    AmbientEnv {
        env: String,
        header: String,
        prefix: String,
    },
    None,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum AuthPlanError {
    #[error("credential environment variable name must not be empty")]
    EmptyEnvironmentVariable,
    #[error("authentication header name must not be empty")]
    EmptyHeader,
    #[error("credential file path must not be empty")]
    EmptyCredentialFile,
    #[error("OAuth token URL must not be empty")]
    EmptyTokenUrl,
    #[error("remote endpoint without authentication requires allow_remote=true")]
    RemoteWithoutAuthentication,
}

impl AuthSpec {
    pub fn plan(&self, is_loopback: bool) -> Result<AuthPlan, AuthPlanError> {
        match self {
            Self::ApiKeyEnv {
                env,
                header,
                prefix,
            } => {
                if env.is_empty() {
                    return Err(AuthPlanError::EmptyEnvironmentVariable);
                }
                if header.is_empty() {
                    return Err(AuthPlanError::EmptyHeader);
                }
                Ok(AuthPlan::ApiKeyEnv {
                    env: env.clone(),
                    header: header.to_ascii_lowercase(),
                    prefix: prefix.clone(),
                })
            }
            Self::OAuthBearerFile {
                path,
                header,
                prefix,
            } => {
                if path.is_empty() {
                    return Err(AuthPlanError::EmptyCredentialFile);
                }
                if header.is_empty() {
                    return Err(AuthPlanError::EmptyHeader);
                }
                Ok(AuthPlan::OAuthBearerFile {
                    path: path.clone(),
                    header: header.to_ascii_lowercase(),
                    prefix: prefix.clone(),
                })
            }
            Self::OAuthClientCredentials {
                token_url,
                client_id_env,
                client_secret_env,
                token_file,
                scope,
                audience,
                header,
                prefix,
            } => {
                if token_url.is_empty() {
                    return Err(AuthPlanError::EmptyTokenUrl);
                }
                if client_id_env.is_empty() || client_secret_env.is_empty() {
                    return Err(AuthPlanError::EmptyEnvironmentVariable);
                }
                if token_file.is_empty() {
                    return Err(AuthPlanError::EmptyCredentialFile);
                }
                if header.is_empty() {
                    return Err(AuthPlanError::EmptyHeader);
                }
                Ok(AuthPlan::OAuthClientCredentials {
                    token_url: token_url.clone(),
                    client_id_env: client_id_env.clone(),
                    client_secret_env: client_secret_env.clone(),
                    token_file: token_file.clone(),
                    scope: scope.clone(),
                    audience: audience.clone(),
                    header: header.to_ascii_lowercase(),
                    prefix: prefix.clone(),
                })
            }
            Self::AmbientEnv {
                env,
                header,
                prefix,
            } => {
                if env.is_empty() {
                    return Err(AuthPlanError::EmptyEnvironmentVariable);
                }
                if header.is_empty() {
                    return Err(AuthPlanError::EmptyHeader);
                }
                Ok(AuthPlan::AmbientEnv {
                    env: env.clone(),
                    header: header.to_ascii_lowercase(),
                    prefix: prefix.clone(),
                })
            }
            Self::None { allow_remote } => {
                if !is_loopback && !allow_remote {
                    return Err(AuthPlanError::RemoteWithoutAuthentication);
                }
                Ok(AuthPlan::None)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oauth_file_plan_preserves_rotation_source_without_reading_it() {
        let plan = AuthSpec::OAuthBearerFile {
            path: "run/secrets/provider-token".to_owned(),
            header: "Authorization".to_owned(),
            prefix: "Bearer ".to_owned(),
        }
        .plan(false)
        .unwrap();
        assert_eq!(
            plan,
            AuthPlan::OAuthBearerFile {
                path: "run/secrets/provider-token".to_owned(),
                header: "authorization".to_owned(),
                prefix: "Bearer ".to_owned(),
            }
        );
    }

    #[test]
    fn ambient_plan_is_provider_neutral_and_validated() {
        assert_eq!(
            AuthSpec::AmbientEnv {
                env: "CLOUD_IDENTITY_TOKEN".to_owned(),
                header: "X-Cloud-Token".to_owned(),
                prefix: String::new(),
            }
            .plan(false)
            .unwrap(),
            AuthPlan::AmbientEnv {
                env: "CLOUD_IDENTITY_TOKEN".to_owned(),
                header: "x-cloud-token".to_owned(),
                prefix: String::new(),
            }
        );
        assert!(matches!(
            AuthSpec::OAuthBearerFile {
                path: String::new(),
                header: "authorization".to_owned(),
                prefix: String::new(),
            }
            .plan(false),
            Err(AuthPlanError::EmptyCredentialFile)
        ));
    }

    #[test]
    fn oauth_client_credentials_plan_is_provider_neutral_and_validated() {
        let plan = AuthSpec::OAuthClientCredentials {
            token_url: "https://identity.example.test/oauth/token".to_owned(),
            client_id_env: "PROVIDER_CLIENT_ID".to_owned(),
            client_secret_env: "PROVIDER_CLIENT_SECRET".to_owned(),
            token_file: "run/provider-token.json".to_owned(),
            scope: Some("inference".to_owned()),
            audience: Some("models".to_owned()),
            header: "Authorization".to_owned(),
            prefix: "Bearer ".to_owned(),
        }
        .plan(false)
        .unwrap();
        assert!(matches!(
            plan,
            AuthPlan::OAuthClientCredentials {
                header,
                scope: Some(scope),
                ..
            } if header == "authorization" && scope == "inference"
        ));
        assert!(matches!(
            AuthSpec::OAuthClientCredentials {
                token_url: String::new(),
                client_id_env: "ID".to_owned(),
                client_secret_env: "SECRET".to_owned(),
                token_file: "token.json".to_owned(),
                scope: None,
                audience: None,
                header: "authorization".to_owned(),
                prefix: "Bearer ".to_owned(),
            }
            .plan(false),
            Err(AuthPlanError::EmptyTokenUrl)
        ));
    }
}
