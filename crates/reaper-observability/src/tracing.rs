//! Log and trace setup (contracts/config.md, `--log-format` and `OTel` env).
//!
//! Logs go to stdout as JSON or text, filtered by `RUST_LOG` (default `info`).
//! When an OTLP endpoint is configured, spans are also exported over OTLP/HTTP.

use std::str::FromStr;

use opentelemetry::trace::TracerProvider as _;
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::trace::SdkTracerProvider;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer, Registry};

const SERVICE_NAME: &str = "k8s-reaper";

/// Log line format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LogFormat {
    #[default]
    Json,
    Text,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown log format {0:?}, expected `json` or `text`")]
pub struct UnknownLogFormat(String);

impl FromStr for LogFormat {
    type Err = UnknownLogFormat;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "json" => Ok(Self::Json),
            "text" => Ok(Self::Text),
            _ => Err(UnknownLogFormat(s.to_owned())),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum InitError {
    #[error("building the OTLP span exporter: {0}")]
    Exporter(#[from] opentelemetry_otlp::ExporterBuildError),
    #[error("installing the global subscriber: {0}")]
    Subscriber(#[from] tracing_subscriber::util::TryInitError),
}

/// Keeps the OTLP pipeline alive; dropping it flushes and shuts it down.
#[derive(Debug)]
pub struct TracingGuard {
    provider: Option<SdkTracerProvider>,
}

impl Drop for TracingGuard {
    fn drop(&mut self) {
        if let Some(provider) = self.provider.take() {
            // Nothing useful can be done with a failed flush at exit.
            let _ = provider.shutdown();
        }
    }
}

/// Installs the global subscriber. Call once, early in `main`.
///
/// # Errors
///
/// The OTLP exporter cannot be built, or a global subscriber is already set.
pub fn init(format: LogFormat) -> Result<TracingGuard, InitError> {
    let env = |key: &str| std::env::var(key).ok();

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let fmt = match format {
        LogFormat::Json => tracing_subscriber::fmt::layer().json().boxed(),
        LogFormat::Text => tracing_subscriber::fmt::layer().boxed(),
    };

    let provider = if otlp_enabled(env) {
        let exporter = opentelemetry_otlp::SpanExporter::builder()
            .with_http()
            .build()?;
        let mut resource = Resource::builder();
        if needs_default_service_name(env) {
            resource = resource.with_service_name(SERVICE_NAME);
        }
        Some(
            SdkTracerProvider::builder()
                .with_resource(resource.build())
                .with_batch_exporter(exporter)
                .build(),
        )
    } else {
        None
    };
    let otel = provider
        .as_ref()
        .map(|p| tracing_opentelemetry::layer().with_tracer(p.tracer(SERVICE_NAME)));

    Registry::default()
        .with(filter)
        .with(fmt)
        .with(otel)
        .try_init()?;
    Ok(TracingGuard { provider })
}

fn non_empty(value: Option<String>) -> Option<String> {
    value.filter(|v| !v.trim().is_empty())
}

/// OTLP is on when either the general or the traces endpoint is set.
fn otlp_enabled(env: impl Fn(&str) -> Option<String>) -> bool {
    [
        "OTEL_EXPORTER_OTLP_ENDPOINT",
        "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT",
    ]
    .iter()
    .any(|key| non_empty(env(key)).is_some())
}

/// The SDK names the service `unknown_service` unless the env names it.
fn needs_default_service_name(env: impl Fn(&str) -> Option<String>) -> bool {
    let from_attributes = non_empty(env("OTEL_RESOURCE_ATTRIBUTES")).is_some_and(|attrs| {
        attrs
            .split(',')
            .any(|kv| kv.trim().starts_with("service.name="))
    });
    non_empty(env("OTEL_SERVICE_NAME")).is_none() && !from_attributes
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        move |key| map.get(key).cloned()
    }

    #[test]
    fn log_format_parses_json_and_text() {
        assert_eq!("json".parse(), Ok(LogFormat::Json));
        assert_eq!("TEXT".parse(), Ok(LogFormat::Text));
        assert_eq!(
            "yaml".parse::<LogFormat>(),
            Err(UnknownLogFormat("yaml".to_owned()))
        );
        assert_eq!(LogFormat::default(), LogFormat::Json);
    }

    #[test]
    fn otlp_is_off_without_an_endpoint() {
        assert!(!otlp_enabled(env(&[])));
        assert!(!otlp_enabled(env(&[("OTEL_EXPORTER_OTLP_ENDPOINT", "  ")])));
    }

    #[test]
    fn otlp_is_on_with_either_endpoint() {
        assert!(otlp_enabled(env(&[(
            "OTEL_EXPORTER_OTLP_ENDPOINT",
            "http://collector:4318"
        )])));
        assert!(otlp_enabled(env(&[(
            "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT",
            "http://collector:4318/v1/traces"
        )])));
    }

    #[test]
    fn service_name_defaults_only_when_the_env_does_not_name_it() {
        assert!(needs_default_service_name(env(&[])));
        assert!(needs_default_service_name(env(&[(
            "OTEL_RESOURCE_ATTRIBUTES",
            "team=sre"
        )])));
        assert!(!needs_default_service_name(env(&[(
            "OTEL_SERVICE_NAME",
            "reaper-prod"
        )])));
        assert!(!needs_default_service_name(env(&[(
            "OTEL_RESOURCE_ATTRIBUTES",
            "team=sre, service.name=reaper-prod"
        )])));
    }
}
