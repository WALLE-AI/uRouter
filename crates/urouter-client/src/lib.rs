use std::time::Duration;

use reqwest::{Response, StatusCode, Url};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

#[derive(Debug, Clone)]
pub struct UrouterClient {
    http: reqwest::Client,
    gateways: Vec<Url>,
    maximum_attempts: usize,
}

impl UrouterClient {
    pub fn new(
        gateways: Vec<Url>,
        timeout: Duration,
        maximum_attempts: usize,
    ) -> Result<Self, ClientError> {
        if gateways.is_empty() {
            return Err(ClientError::NoGateways);
        }
        if maximum_attempts == 0 {
            return Err(ClientError::InvalidAttemptLimit);
        }
        if gateways
            .iter()
            .any(|url| !matches!(url.scheme(), "http" | "https"))
        {
            return Err(ClientError::InvalidGatewayScheme);
        }
        Ok(Self {
            // Gateways are addressed directly, exactly like the Gateway's own
            // upstream client. An ambient `http_proxy` must never silently
            // re-route a Gateway call or turn a loopback deployment into a
            // proxied request.
            http: reqwest::Client::builder()
                .no_proxy()
                .timeout(timeout)
                .build()?,
            gateways,
            maximum_attempts,
        })
    }

    pub async fn post_json(
        &self,
        path: &str,
        body: &Value,
        replay: ReplaySafety,
    ) -> Result<ClientResponse, ClientError> {
        let mut attempts = Vec::new();
        let limit = if replay == ReplaySafety::Safe {
            self.maximum_attempts.min(self.gateways.len())
        } else {
            1
        };
        for gateway in self.gateways.iter().take(limit) {
            let endpoint = endpoint(gateway, path)?;
            match self.http.post(endpoint.clone()).json(body).send().await {
                Ok(response) if response.status().is_success() => {
                    let status = response.status();
                    let value = response.json().await?;
                    attempts.push(ClientAttempt {
                        gateway: gateway.to_string(),
                        status: Some(status.as_u16()),
                        retryable: false,
                    });
                    return Ok(ClientResponse {
                        gateway: gateway.to_string(),
                        status: status.as_u16(),
                        body: value,
                        attempts,
                    });
                }
                Ok(response) => {
                    let status = response.status();
                    let retryable = transient_status(status);
                    attempts.push(ClientAttempt {
                        gateway: gateway.to_string(),
                        status: Some(status.as_u16()),
                        retryable,
                    });
                    if replay == ReplaySafety::Unsafe || !retryable {
                        return Err(ClientError::HttpStatus { status, attempts });
                    }
                }
                Err(error) => {
                    attempts.push(ClientAttempt {
                        gateway: gateway.to_string(),
                        status: None,
                        retryable: true,
                    });
                    if replay == ReplaySafety::Unsafe {
                        return Err(ClientError::Transport { error, attempts });
                    }
                }
            }
        }
        Err(ClientError::Exhausted { attempts })
    }

    pub async fn open_stream(
        &self,
        path: &str,
        body: &Value,
        replay_before_start: ReplaySafety,
    ) -> Result<GatewayStream, ClientError> {
        let mut attempts = Vec::new();
        let limit = if replay_before_start == ReplaySafety::Safe {
            self.maximum_attempts.min(self.gateways.len())
        } else {
            1
        };
        for gateway in self.gateways.iter().take(limit) {
            let endpoint = endpoint(gateway, path)?;
            match self.http.post(endpoint).json(body).send().await {
                Ok(response) if response.status().is_success() => {
                    attempts.push(ClientAttempt {
                        gateway: gateway.to_string(),
                        status: Some(response.status().as_u16()),
                        retryable: false,
                    });
                    return Ok(GatewayStream {
                        gateway: gateway.to_string(),
                        response,
                        attempts,
                    });
                }
                Ok(response) => {
                    let status = response.status();
                    let retryable = transient_status(status);
                    attempts.push(ClientAttempt {
                        gateway: gateway.to_string(),
                        status: Some(status.as_u16()),
                        retryable,
                    });
                    if replay_before_start == ReplaySafety::Unsafe || !retryable {
                        return Err(ClientError::HttpStatus { status, attempts });
                    }
                }
                Err(error) => {
                    attempts.push(ClientAttempt {
                        gateway: gateway.to_string(),
                        status: None,
                        retryable: true,
                    });
                    if replay_before_start == ReplaySafety::Unsafe {
                        return Err(ClientError::Transport { error, attempts });
                    }
                }
            }
        }
        Err(ClientError::Exhausted { attempts })
    }
}

fn endpoint(base: &Url, path: &str) -> Result<Url, ClientError> {
    base.join(path.trim_start_matches('/'))
        .map_err(ClientError::Url)
}

fn transient_status(status: StatusCode) -> bool {
    status.as_u16() == 429 || status.is_server_error()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplaySafety {
    Safe,
    Unsafe,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientAttempt {
    pub gateway: String,
    pub status: Option<u16>,
    pub retryable: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientResponse {
    pub gateway: String,
    pub status: u16,
    pub body: Value,
    pub attempts: Vec<ClientAttempt>,
}

pub struct GatewayStream {
    pub gateway: String,
    pub response: Response,
    pub attempts: Vec<ClientAttempt>,
}

impl GatewayStream {
    /// Returns the upstream response exactly once. Any body error is surfaced to
    /// the caller and is never replayed by `urouter-client`.
    #[must_use]
    pub fn into_response(self) -> Response {
        self.response
    }
}

#[derive(Debug, Error)]
pub enum ClientError {
    #[error("at least one Gateway URL is required")]
    NoGateways,
    #[error("maximum_attempts must be greater than zero")]
    InvalidAttemptLimit,
    #[error("Gateway URL scheme must be HTTP or HTTPS")]
    InvalidGatewayScheme,
    #[error("Gateway returned HTTP {status}")]
    HttpStatus {
        status: StatusCode,
        attempts: Vec<ClientAttempt>,
    },
    #[error("Gateway request failed before a response started")]
    Transport {
        #[source]
        error: reqwest::Error,
        attempts: Vec<ClientAttempt>,
    },
    #[error("all eligible Gateway attempts were exhausted")]
    Exhausted { attempts: Vec<ClientAttempt> },
    #[error(transparent)]
    Http(#[from] reqwest::Error),
    #[error(transparent)]
    Url(#[from] url::ParseError),
}

#[cfg(test)]
mod tests {
    use axum::{Json, Router, http::StatusCode, routing::post};
    use serde_json::json;
    use tokio::net::TcpListener;

    use super::*;

    async fn server(status: StatusCode) -> (Url, tokio::task::JoinHandle<()>) {
        let app = Router::new().route(
            "/v1/chat/completions",
            post(move || async move { (status, Json(json!({"status": status.as_u16()}))) }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (
            Url::parse(&format!("http://{address}/v1/")).unwrap(),
            handle,
        )
    }

    #[tokio::test]
    async fn replay_safe_request_fails_over_to_next_gateway() {
        let (first, first_handle) = server(StatusCode::SERVICE_UNAVAILABLE).await;
        let (second, second_handle) = server(StatusCode::OK).await;
        let client = UrouterClient::new(vec![first, second], Duration::from_secs(2), 2).unwrap();
        let response = client
            .post_json("chat/completions", &json!({}), ReplaySafety::Safe)
            .await
            .unwrap();
        assert_eq!(response.attempts.len(), 2);
        assert_eq!(response.status, 200);
        first_handle.abort();
        second_handle.abort();
    }

    #[tokio::test]
    async fn unsafe_request_and_started_stream_are_never_replayed() {
        let (first, first_handle) = server(StatusCode::SERVICE_UNAVAILABLE).await;
        let (second, second_handle) = server(StatusCode::OK).await;
        let client = UrouterClient::new(vec![first, second], Duration::from_secs(2), 2).unwrap();
        let error = client
            .post_json("chat/completions", &json!({}), ReplaySafety::Unsafe)
            .await
            .unwrap_err();
        assert!(matches!(error, ClientError::HttpStatus { attempts, .. } if attempts.len() == 1));

        let (stream_url, stream_handle) = server(StatusCode::OK).await;
        let stream_client =
            UrouterClient::new(vec![stream_url], Duration::from_secs(2), 1).unwrap();
        let stream = stream_client
            .open_stream("chat/completions", &json!({}), ReplaySafety::Safe)
            .await
            .unwrap();
        assert_eq!(stream.attempts.len(), 1);
        assert_eq!(stream.into_response().status(), StatusCode::OK);
        first_handle.abort();
        second_handle.abort();
        stream_handle.abort();
    }
}
