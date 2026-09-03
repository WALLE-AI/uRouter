use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpstreamErrorKind {
    Transport,
    Timeout,
    RateLimited,
    ServerError,
    ProviderUnavailable,
    Unauthorized,
    NotFound,
    BadRequest,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FallbackCause {
    ContextWindow,
    ContentPolicy,
    Quota,
    Capacity,
    Transport,
    Timeout,
    RateLimited,
    ServerError,
    ProviderUnavailable,
    Unauthorized,
    NotFound,
    BadRequest,
}

impl From<UpstreamErrorKind> for FallbackCause {
    fn from(value: UpstreamErrorKind) -> Self {
        match value {
            UpstreamErrorKind::Transport => Self::Transport,
            UpstreamErrorKind::Timeout => Self::Timeout,
            UpstreamErrorKind::RateLimited => Self::RateLimited,
            UpstreamErrorKind::ServerError => Self::ServerError,
            UpstreamErrorKind::ProviderUnavailable => Self::ProviderUnavailable,
            UpstreamErrorKind::Unauthorized => Self::Unauthorized,
            UpstreamErrorKind::NotFound => Self::NotFound,
            UpstreamErrorKind::BadRequest => Self::BadRequest,
        }
    }
}

impl UpstreamErrorKind {
    #[must_use]
    pub const fn retryable(self) -> bool {
        matches!(
            self,
            Self::Transport
                | Self::Timeout
                | Self::RateLimited
                | Self::ServerError
                | Self::ProviderUnavailable
        )
    }
}
