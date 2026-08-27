use std::collections::{BTreeMap, BTreeSet};

use thiserror::Error;
use url::Url;

use crate::{
    auth::AuthPlan,
    catalog::{ModelSpec, ProviderSpec},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndpointTemplate {
    template: String,
    variables: BTreeSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum EndpointTemplateError {
    #[error("unclosed endpoint variable")]
    UnclosedVariable,
    #[error("endpoint variable name must not be empty")]
    EmptyVariable,
    #[error("endpoint variable contains invalid characters: {0}")]
    InvalidVariable(String),
    #[error("missing endpoint variable: {0}")]
    MissingVariable(String),
    #[error("endpoint URL is invalid: {0}")]
    InvalidUrl(String),
    #[error("endpoint URL scheme must be http or https")]
    UnsupportedScheme,
}

impl EndpointTemplate {
    pub fn compile(template: impl Into<String>) -> Result<Self, EndpointTemplateError> {
        let template = template.into();
        let mut variables = BTreeSet::new();
        let mut remainder = template.as_str();
        while let Some(start) = remainder.find('{') {
            if remainder[..start].contains('}') {
                return Err(EndpointTemplateError::UnclosedVariable);
            }
            let after = &remainder[start + 1..];
            let end = after
                .find('}')
                .ok_or(EndpointTemplateError::UnclosedVariable)?;
            let variable = &after[..end];
            if variable.is_empty() {
                return Err(EndpointTemplateError::EmptyVariable);
            }
            if !variable
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
            {
                return Err(EndpointTemplateError::InvalidVariable(variable.to_owned()));
            }
            variables.insert(variable.to_owned());
            remainder = &after[end + 1..];
        }
        if remainder.contains('}') {
            return Err(EndpointTemplateError::UnclosedVariable);
        }
        Ok(Self {
            template,
            variables,
        })
    }

    #[must_use]
    pub fn variables(&self) -> &BTreeSet<String> {
        &self.variables
    }

    pub fn materialize(
        &self,
        values: &BTreeMap<String, String>,
    ) -> Result<Url, EndpointTemplateError> {
        let mut result = self.template.clone();
        for variable in &self.variables {
            let value = values
                .get(variable)
                .ok_or_else(|| EndpointTemplateError::MissingVariable(variable.clone()))?;
            result = result.replace(&format!("{{{variable}}}"), value);
        }
        let url = Url::parse(&result)
            .map_err(|error| EndpointTemplateError::InvalidUrl(error.to_string()))?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(EndpointTemplateError::UnsupportedScheme);
        }
        Ok(url)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndpointPlan {
    pub url: Url,
    pub auth: AuthPlan,
    pub public_headers: BTreeMap<String, String>,
}

impl EndpointPlan {
    pub fn for_provider(provider: &ProviderSpec) -> Result<Self, EndpointPlanBuildError> {
        Self::build(provider, None)
    }

    pub fn for_model(
        provider: &ProviderSpec,
        model: &ModelSpec,
    ) -> Result<Self, EndpointPlanBuildError> {
        Self::build(provider, Some(model))
    }

    fn build(
        provider: &ProviderSpec,
        model: Option<&ModelSpec>,
    ) -> Result<Self, EndpointPlanBuildError> {
        let base_url = model
            .and_then(|model| model.base_url.as_deref())
            .unwrap_or(&provider.base_url);
        let template = EndpointTemplate::compile(base_url)?;
        let url = template.materialize(&provider.env)?;
        let is_loopback = url.host_str().is_some_and(|host| {
            host.eq_ignore_ascii_case("localhost")
                || host
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
        });
        let auth = provider.auth.plan(is_loopback)?;
        let public_headers = model.map_or_else(
            || normalize_headers(&provider.headers),
            |model| {
                merge_public_headers(&provider.headers, &model.headers, &BTreeMap::new())
                    .expect("empty request headers cannot override protected headers")
            },
        );
        Ok(Self {
            url,
            auth,
            public_headers,
        })
    }
}

#[derive(Debug, Error)]
pub enum EndpointPlanBuildError {
    #[error(transparent)]
    Template(#[from] EndpointTemplateError),
    #[error(transparent)]
    Auth(#[from] crate::auth::AuthPlanError),
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum HeaderMergeError {
    #[error("request may not override protected header: {0}")]
    ProtectedHeader(String),
}

pub fn merge_public_headers(
    provider: &BTreeMap<String, String>,
    model: &BTreeMap<String, String>,
    request: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>, HeaderMergeError> {
    let mut merged = normalize_headers(provider);
    merged.extend(normalize_headers(model));
    for (name, value) in normalize_headers(request) {
        if matches!(name.as_str(), "authorization" | "host" | "content-length") {
            return Err(HeaderMergeError::ProtectedHeader(name));
        }
        merged.insert(name, value);
    }
    Ok(merged)
}

fn normalize_headers(input: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    input
        .iter()
        .map(|(name, value)| (name.to_ascii_lowercase(), value.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_declared_variables() {
        let template = EndpointTemplate::compile("https://{ACCOUNT}.example/v1").unwrap();
        let values = BTreeMap::from([("ACCOUNT".to_owned(), "acme".to_owned())]);
        assert_eq!(
            template.materialize(&values).unwrap().as_str(),
            "https://acme.example/v1"
        );
    }

    #[test]
    fn request_cannot_override_auth() {
        let request = BTreeMap::from([("Authorization".to_owned(), "secret".to_owned())]);
        assert!(merge_public_headers(&BTreeMap::new(), &BTreeMap::new(), &request).is_err());
    }

    #[test]
    fn rejects_unbalanced_template_and_non_http_url() {
        assert!(EndpointTemplate::compile("https://x}.invalid/{A}").is_err());
        let template = EndpointTemplate::compile("file:///tmp/model").unwrap();
        assert!(template.materialize(&BTreeMap::new()).is_err());
    }
}
