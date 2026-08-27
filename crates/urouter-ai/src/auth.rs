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
    None,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum AuthPlanError {
    #[error("credential environment variable name must not be empty")]
    EmptyEnvironmentVariable,
    #[error("authentication header name must not be empty")]
    EmptyHeader,
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
            Self::None { allow_remote } => {
                if !is_loopback && !allow_remote {
                    return Err(AuthPlanError::RemoteWithoutAuthentication);
                }
                Ok(AuthPlan::None)
            }
        }
    }
}
