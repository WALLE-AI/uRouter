//! Structured logging setup.
//!
//! The Gateway already emits `OpenMetrics` histograms and persists `DecisionRecord`s,
//! but neither is usable for diagnosing a single failing request. Logs carry the
//! same identifiers the records do — `request_id`, `decision_id`, `tenant_key`,
//! `task_id` — so an operator can pivot from a metric to a log line to a record.
//!
//! Field discipline mirrors the metrics rule: identifiers are span fields, never
//! metric labels, so cardinality stays in the log backend where it belongs.

use std::fmt;
use std::str::FromStr;

use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::time::UtcTime;

/// Wire format for log records.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogFormat {
    /// Newline-delimited JSON, one object per event. The production default.
    Json,
    /// Human-readable single-line text for local runs.
    Text,
}

impl LogFormat {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Json => "json",
            Self::Text => "text",
        }
    }
}

impl fmt::Display for LogFormat {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for LogFormat {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "json" => Ok(Self::Json),
            "text" => Ok(Self::Text),
            other => Err(format!(
                "log format must be `json` or `text`, got `{other}`"
            )),
        }
    }
}

/// Checks that `level` is a usable filter directive.
///
/// # Errors
///
/// Returns an error when the directive cannot be parsed.
pub fn validate_level(level: &str) -> Result<(), String> {
    // `EnvFilter` reads any bare unrecognised word as a *target* name with the
    // default level, so `--log-level warining` would parse cleanly and then emit
    // nothing. A single bare word is therefore required to be a real level.
    let bare_word = !level.contains('=') && !level.contains(',');
    if bare_word && !BARE_LEVELS.contains(&level) {
        return Err(format!(
            "invalid log level: `{level}` is not one of {}",
            BARE_LEVELS.join(", ")
        ));
    }
    EnvFilter::try_new(level)
        .map(|_| ())
        .map_err(|error| format!("invalid log level: {error}"))
}

const BARE_LEVELS: [&str; 6] = ["off", "error", "warn", "info", "debug", "trace"];

/// Installs the process-wide subscriber.
///
/// `RUST_LOG` wins when it is set, so an operator can raise a single module to
/// `debug` without restarting with different flags. `--log-level` is the
/// fallback directive.
///
/// # Errors
///
/// Returns an error when `level` is not a valid filter directive. Returns `Ok`
/// when a subscriber is already installed, which keeps tests that initialise
/// logging more than once from aborting the process.
pub fn install(format: LogFormat, level: &str) -> Result<(), String> {
    // Validated before `RUST_LOG` is consulted so that a bad `--log-level` fails
    // at startup even on a host that happens to set `RUST_LOG`.
    validate_level(level)?;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(level));
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_timer(UtcTime::rfc_3339())
        .with_target(true);
    let installed = match format {
        LogFormat::Json => builder
            .json()
            .flatten_event(true)
            .with_current_span(true)
            .with_span_list(false)
            .try_init(),
        LogFormat::Text => builder.try_init(),
    };
    // An already-installed subscriber is not an error: the binary's own tests
    // run in one process and would otherwise race on the global default.
    let _ = installed;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_both_supported_formats() {
        assert_eq!(LogFormat::from_str("json").unwrap(), LogFormat::Json);
        assert_eq!(LogFormat::from_str("text").unwrap(), LogFormat::Text);
    }

    #[test]
    fn rejects_an_unknown_format_with_a_usable_message() {
        let error = LogFormat::from_str("logfmt").unwrap_err();
        assert!(error.contains("json"), "{error}");
        assert!(error.contains("logfmt"), "{error}");
    }

    #[test]
    fn round_trips_through_display() {
        for format in [LogFormat::Json, LogFormat::Text] {
            assert_eq!(LogFormat::from_str(&format.to_string()).unwrap(), format);
        }
    }

    #[test]
    fn an_invalid_level_is_rejected_regardless_of_the_environment() {
        for level in ["==", "app=verbose", "%%%"] {
            let error = validate_level(level).unwrap_err();
            assert!(error.contains("invalid log level"), "{level}: {error}");
        }
        let error = install(LogFormat::Json, "app=verbose").unwrap_err();
        assert!(error.contains("invalid log level"), "{error}");
    }

    /// A bare typo parses as a target directive in `EnvFilter` and would
    /// silently suppress every log line, so it must be rejected by name.
    #[test]
    fn a_bare_level_typo_is_rejected_rather_than_read_as_a_target() {
        for level in ["warining", "verbose", "INFO"] {
            let error = validate_level(level).unwrap_err();
            assert!(error.contains("is not one of"), "{level}: {error}");
        }
    }

    #[test]
    fn ordinary_levels_and_directives_are_accepted() {
        for level in [
            "off",
            "error",
            "warn",
            "info",
            "debug",
            "trace",
            "urouter_gateway=debug,info",
            "urouter_gateway::logging=trace",
        ] {
            validate_level(level).unwrap_or_else(|error| panic!("{level}: {error}"));
        }
    }

    #[test]
    fn installing_twice_is_not_an_error() {
        install(LogFormat::Text, "info").unwrap();
        install(LogFormat::Json, "warn").unwrap();
    }
}
