//! The OpenTelemetry pipeline: OpenTelemetry at the edges,
//! provider swappable — everything emits OTLP; the provider is just a sink.
//!
//! Bootstrapped FIRST in `main`, before any other subsystem, so that even
//! startup failures are traceable. Two layers on one global subscriber:
//!
//! * the fmt layer — always on (JSON by default, `SECTR_LOG_FORMAT=text`
//!   for humans); stdout logging is unchanged in every mode
//! * the OTel layer — **env-gated**: `SECTR_OTLP_ENDPOINT` set → spans
//!   flow as OTLP/HTTP-protobuf to the provider; unset → the layer simply
//!   doesn't exist and dev/localhost keeps its exact behavior
//!   (no client, no extra threads, no provider)
//!
//! The same gate ALSO stands up the LOGS half — every
//! `tracing::info!`-style event (via the `OpenTelemetryTracingBridge`)
//! becomes an OTel log record exported to `/v1/logs`, which Grafana Cloud
//! ingests into Loki: plain tracing lines are searchable there
//! (`{service_name="sectr-server"} |= "turn complete"`).
//!
//! The same gate also stands up the METRICS half — an OTLP metrics
//! provider (`/v1/metrics`, same base URL + headers, cumulative temporality
//! — what Grafana Cloud expects) installed as the global meter provider.
//! Unset endpoint → `opentelemetry::global::meter` stays a no-op, so metric
//! call sites are always safe. The metrics reader is periodic (60s
//! collect/export on the SDK's own thread) — the hot path only touches
//! lock-free counters/histograms.
//!
//! Env vars:
//!
//! * `SECTR_OTLP_ENDPOINT` — OTLP/HTTP collector BASE URL; `/v1/traces`
//!   and `/v1/metrics` are appended per OTel-spec base-URL semantics
//!   (Grafana Cloud's `…/otlp` becomes `…/otlp/v1/traces` + `…/v1/metrics`);
//!   unset/empty = no-op
//! * `SECTR_OTLP_HEADERS` — request headers as `key1=value1,key2=value2`
//!   (OTel-spec format; the value runs to end-of-pair, so base64 padding
//!   in `Authorization=Basic abc==` survives); used for the provider auth
//!   token (traces AND metrics)
//! * `SECTR_OTLP_SAMPLE_RATIO` — head-sampling ratio in [0.0, 1.0]
//!   (`TraceIdRatioBased`; default 1.0 — alpha volume is tiny, this is the
//!   free-tier lever)
//!
//! Resource attributes: `service.name` = the caller's binary name
//! (`sectr-server`, `sectr-worker` — passed to [`init`]),
//! `service.version = CARGO_PKG_VERSION` — answers "what build is prod
//! running" straight off a trace.
//!
//! Wired in: sectr-server (`init("sectr-server")`) and the worker sidecar
//! binary (`init("sectr-worker")`). DELIBERATELY NOT wired in the CLI: the
//! `sectr dev` engine runs worker-library code in the CLI process with no
//! subscriber — those events drop, which is correct for now (clean CLI
//! output; worker events under dev are noise, not diagnostics). May be
//! wired later (decided 2026-09-22).
//!
//! Never journal content or provider secrets in span attributes — that
//! discipline lives with the callers.

use std::collections::HashMap;
use std::time::Duration;

use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::{Protocol, WithExportConfig, WithHttpConfig};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::metrics::{PeriodicReader, SdkMeterProvider, Temporality};
use opentelemetry_sdk::trace::{Sampler, SdkTracerProvider};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// Env-gated OTLP exporter config, parsed from `SECTR_OTLP_*` env vars.
///
/// `None` (endpoint unset) = the no-op dev path: no OTel layer, no provider.
#[derive(Debug, Clone, PartialEq)]
pub struct OtlpConfig {
  /// OTLP/HTTP collector base URL; the exporter appends `/v1/traces`.
  pub endpoint: String,
  /// Request headers (provider auth), parsed from
  /// `SECTR_OTLP_HEADERS=key1=value1,key2=value2`.
  pub headers: HashMap<String, String>,
  /// Head-sampling ratio in [0.0, 1.0]; `SECTR_OTLP_SAMPLE_RATIO`.
  pub sample_ratio: f64,
}

impl OtlpConfig {
  /// `Some(config)` when `SECTR_OTLP_ENDPOINT` is set and non-empty.
  /// Malformed values are startup errors (fail fast, per the
  /// `ServerConfig::from_env` check_* discipline): a misconfigured
  /// exporter must not look like working telemetry.
  pub fn from_env() -> anyhow::Result<Option<Self>> {
    let endpoint = match std::env::var("SECTR_OTLP_ENDPOINT") {
      Ok(v) if !v.trim().is_empty() => v,
      _ => return Ok(None),
    };
    let headers = match std::env::var("SECTR_OTLP_HEADERS") {
      Ok(raw) if !raw.trim().is_empty() => parse_headers(&raw)?,
      _ => HashMap::new(),
    };
    let sample_ratio = match std::env::var("SECTR_OTLP_SAMPLE_RATIO") {
      Ok(v) if !v.trim().is_empty() => parse_sample_ratio(&v)?,
      _ => 1.0,
    };
    Ok(Some(OtlpConfig {
      endpoint,
      headers,
      sample_ratio,
    }))
  }
}

/// Parse `SECTR_OTLP_HEADERS` in OTel-spec format: comma-separated
/// `key=value` pairs, split on the FIRST `=` so values keep their `=`
/// (base64 auth tokens end in padding). Values are used verbatim — this is
/// our own env var, not the otlp crate's percent-decoded
/// `OTEL_EXPORTER_OTLP_HEADERS`.
fn parse_headers(raw: &str) -> anyhow::Result<HashMap<String, String>> {
  let mut headers = HashMap::new();
  for pair in raw.split(',') {
    let pair = pair.trim();
    if pair.is_empty() {
      continue;
    }
    let (key, value) = pair.split_once('=').ok_or_else(|| {
      anyhow::anyhow!(
        "SECTR_OTLP_HEADERS: entry {pair:?} is not `key=value` \
                 (format: key1=value1,key2=value2)"
      )
    })?;
    let key = key.trim();
    if key.is_empty() {
      anyhow::bail!("SECTR_OTLP_HEADERS: entry {pair:?} has an empty header name");
    }
    if headers.insert(key.to_owned(), value.to_owned()).is_some() {
      anyhow::bail!("SECTR_OTLP_HEADERS: duplicate header {key:?}");
    }
  }
  Ok(headers)
}

/// Parse `SECTR_OTLP_SAMPLE_RATIO` — must be a finite f64 in [0.0, 1.0]
/// (0.0 = never sample, 1.0 = everything).
fn parse_sample_ratio(raw: &str) -> anyhow::Result<f64> {
  let ratio: f64 = raw
    .trim()
    .parse()
    .map_err(|_| anyhow::anyhow!("SECTR_OTLP_SAMPLE_RATIO={raw:?} is not a number"))?;
  if !(0.0..=1.0).contains(&ratio) || ratio.is_nan() {
    anyhow::bail!("SECTR_OTLP_SAMPLE_RATIO={ratio} is not in [0.0, 1.0]");
  }
  Ok(ratio)
}

/// Handle that flushes the tracer + meter providers on drop — the batch
/// exporter's last in-flight spans and the periodic reader's final metric
/// collection must not be lost when the process shuts down.
#[must_use = "dropping the guard early stops the OTel pipeline flushing at exit"]
pub struct TelemetryGuard {
  provider: Option<SdkTracerProvider>,
  meter_provider: Option<SdkMeterProvider>,
  log_provider: Option<opentelemetry_sdk::logs::SdkLoggerProvider>,
}

impl Drop for TelemetryGuard {
  fn drop(&mut self) {
    if let Some(provider) = self.provider.take()
      && let Err(err) = provider.shutdown()
    {
      tracing::warn!(error = %err, "tracer provider shutdown failed");
    }
    if let Some(provider) = self.meter_provider.take()
      && let Err(err) = provider.shutdown()
    {
      tracing::warn!(error = %err, "meter provider shutdown failed");
    }
    if let Some(provider) = self.log_provider.take()
      && let Err(err) = provider.shutdown()
    {
      tracing::warn!(error = %err, "logger provider shutdown failed");
    }
  }
}

/// Build the global subscriber: fmt layer always, OTel layer when
/// `SECTR_OTLP_ENDPOINT` is set. Must run before anything else logs.
///
/// `service_name` stamps the OTel resource (`service.name`) and the
/// tracer name — each binary identifies itself ("sectr-server",
/// "sectr-worker"). The guard must be held for the process lifetime.
pub fn init(service_name: &str) -> anyhow::Result<TelemetryGuard> {
  // Log format: JSON by default (the prod artifact — machine-parseable,
  // jq-able, one format on localhost and the VPS); `SECTR_LOG_FORMAT=text`
  // for human-readable colored output when running locally. The JSON and
  // text fmt layers are different concrete types — box to unify.
  let filter = tracing_subscriber::EnvFilter::try_from_default_env()
    .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
  type FmtLayer = Box<dyn tracing_subscriber::Layer<tracing_subscriber::Registry> + Send + Sync>;
  let fmt_layer: FmtLayer = match std::env::var("SECTR_LOG_FORMAT").as_deref() {
    Ok("text") | Ok("pretty") => {
      Box::new(tracing_subscriber::fmt::layer().with_filter(filter.clone()))
    }
    _ => Box::new(
      tracing_subscriber::fmt::layer()
        .json()
        .with_filter(filter.clone()),
    ),
  };

  match OtlpConfig::from_env()? {
    None => {
      tracing_subscriber::registry().with(fmt_layer).init();
      Ok(TelemetryGuard {
        provider: None,
        meter_provider: None,
        log_provider: None,
      })
    }
    Some(config) => {
      let provider = build_provider(&config, service_name)?;
      let tracer = provider.tracer(service_name.to_string());
      // Logs ride the same gate (the third signal): the
      // tracing bridge converts every `tracing::info!`-style event into
      // an OTel log record exported to /v1/logs (Loki in Grafana Cloud).
      // Installed BELOW the fmt layer's filter behavior — the EnvFilter
      // lives on the fmt layer; the bridge copies events at its own
      // level, so it needs its own filter to honor SECTR_LOG / the
      // default `info` (do not re-log debug spam).
      let logger_provider = build_logger_provider(&config, service_name)?;
      let log_bridge =
        opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge::new(&logger_provider)
          .with_filter(filter.clone());
      // Metrics ride the same gate: one env var, one
      // Grafana Cloud OTLP base URL, both signals. Installed as the
      // GLOBAL meter provider — metric call sites go through
      // `opentelemetry::global::meter` and stay no-ops when this
      // arm never ran.
      let meter_provider = build_meter_provider(&config, service_name)?;
      opentelemetry::global::set_meter_provider(meter_provider.clone());
      tracing_subscriber::registry()
        .with(fmt_layer)
        .with(tracing_opentelemetry::layer().with_tracer(tracer))
        .with(log_bridge)
        .try_init()?;
      tracing::info!(
          endpoint = %config.endpoint,
          sample_ratio = config.sample_ratio,
          "OTLP enabled (spans → /v1/traces, metrics → /v1/metrics, logs → /v1/logs on {})",
          config.endpoint
      );
      Ok(TelemetryGuard {
        provider: Some(provider),
        meter_provider: Some(meter_provider),
        log_provider: Some(logger_provider),
      })
    }
  }
}

/// Resolve the traces endpoint from the `SECTR_OTLP_ENDPOINT` base URL.
/// Spec semantics of `OTEL_EXPORTER_OTLP_ENDPOINT` (base URL + signal
/// path): the signal path is appended unless the URL already ends with it —
/// a programmatic endpoint is otherwise used verbatim by the exporter
/// (Grafana Cloud's `…/otlp` expects `…/otlp/v1/traces`).
fn resolve_signal_endpoint(endpoint: &str, signal_path: &str) -> String {
  if endpoint.trim_end_matches('/').ends_with(signal_path) {
    return endpoint.to_string();
  }
  if endpoint.ends_with('/') {
    format!("{endpoint}{signal_path}")
  } else {
    format!("{endpoint}/{signal_path}")
  }
}

fn resolve_traces_endpoint(endpoint: &str) -> String {
  resolve_signal_endpoint(endpoint, "v1/traces")
}

fn resolve_metrics_endpoint(endpoint: &str) -> String {
  resolve_signal_endpoint(endpoint, "v1/metrics")
}

fn resolve_logs_endpoint(endpoint: &str) -> String {
  resolve_signal_endpoint(endpoint, "v1/logs")
}

/// OTLP/HTTP exporter (protobuf — simpler than gRPC through proxies) on the
/// SDK's default `BatchSpanProcessor`: a dedicated background thread that
/// blocks on export batches, so the exporter must be the BLOCKING reqwest
/// client (workspace feature choice — see root Cargo.toml). Batch tuning
/// stays at SDK defaults; the sampler is the free-tier lever.
fn build_provider(config: &OtlpConfig, service_name: &str) -> anyhow::Result<SdkTracerProvider> {
  let exporter = opentelemetry_otlp::SpanExporter::builder()
    .with_http()
    .with_endpoint(resolve_traces_endpoint(&config.endpoint))
    .with_timeout(Duration::from_secs(10))
    .with_protocol(Protocol::HttpBinary)
    .with_headers(config.headers.clone())
    .build()?;
  Ok(
    SdkTracerProvider::builder()
      .with_resource(
        Resource::builder()
          .with_attribute(opentelemetry::KeyValue::new(
            "service.name",
            service_name.to_string(),
          ))
          .with_attribute(opentelemetry::KeyValue::new(
            "service.version",
            env!("CARGO_PKG_VERSION"),
          ))
          .build(),
      )
      .with_sampler(Sampler::TraceIdRatioBased(config.sample_ratio))
      .with_batch_exporter(exporter)
      .build(),
  )
}

/// OTLP/HTTP metrics exporter (protobuf, cumulative temporality — Grafana
/// Cloud's default expectation) on a `PeriodicReader`: collect + export on
/// the reader's own 60s cadence, so the hot path only touches the SDK's
/// lock-free instrument state. Same base URL + headers as traces.
fn build_meter_provider(
  config: &OtlpConfig,
  service_name: &str,
) -> anyhow::Result<SdkMeterProvider> {
  let exporter = opentelemetry_otlp::MetricExporter::builder()
    .with_http()
    .with_endpoint(resolve_metrics_endpoint(&config.endpoint))
    .with_timeout(Duration::from_secs(10))
    .with_protocol(Protocol::HttpBinary)
    .with_headers(config.headers.clone())
    .with_temporality(Temporality::Cumulative)
    .build()?;
  let reader = PeriodicReader::builder(exporter)
    .with_interval(Duration::from_secs(60))
    .build();
  Ok(
    SdkMeterProvider::builder()
      .with_resource(
        Resource::builder()
          .with_attribute(opentelemetry::KeyValue::new(
            "service.name",
            service_name.to_string(),
          ))
          .with_attribute(opentelemetry::KeyValue::new(
            "service.version",
            env!("CARGO_PKG_VERSION"),
          ))
          .build(),
      )
      .with_reader(reader)
      .build(),
  )
}

/// OTLP/HTTP logs exporter (protobuf) on a `BatchLogProcessor` — the third
/// signal on the same env gate. `tracing::info!`-style events (via the
/// `OpenTelemetryTracingBridge` installed in [`init`]) become OTel log
/// records at `/v1/logs`; Grafana Cloud ingests them into Loki, so plain
/// `tracing` lines are searchable there (`{service_name=...} |= "..."`).
/// Same base URL + headers as traces/metrics; blocking reqwest, same
/// reason as the span exporter.
fn build_logger_provider(
  config: &OtlpConfig,
  service_name: &str,
) -> anyhow::Result<opentelemetry_sdk::logs::SdkLoggerProvider> {
  use opentelemetry_sdk::logs::SdkLoggerProvider;
  let exporter = opentelemetry_otlp::LogExporter::builder()
    .with_http()
    .with_endpoint(resolve_logs_endpoint(&config.endpoint))
    .with_timeout(Duration::from_secs(10))
    .with_protocol(Protocol::HttpBinary)
    .with_headers(config.headers.clone())
    .build()?;
  Ok(
    SdkLoggerProvider::builder()
      .with_resource(
        Resource::builder()
          .with_attribute(opentelemetry::KeyValue::new(
            "service.name",
            service_name.to_string(),
          ))
          .with_attribute(opentelemetry::KeyValue::new(
            "service.version",
            env!("CARGO_PKG_VERSION"),
          ))
          .build(),
      )
      .with_batch_exporter(exporter)
      .build(),
  )
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn parse_headers_single_pair() {
    let h = parse_headers("Authorization=Basic dXNlcjpwYXNz").unwrap();
    assert_eq!(h.get("Authorization").unwrap(), "Basic dXNlcjpwYXNz");
  }

  #[test]
  fn parse_headers_value_keeps_equals_base64_padding() {
    let h = parse_headers("Authorization=Basic abc==,x-flag=1").unwrap();
    assert_eq!(h.get("Authorization").unwrap(), "Basic abc==");
    assert_eq!(h.get("x-flag").unwrap(), "1");
  }

  #[test]
  fn parse_headers_trims_whitespace_and_skips_empties() {
    let h = parse_headers(" a = b , , c=d").unwrap();
    assert_eq!(h.get("a").unwrap(), " b");
    assert_eq!(h.get("c").unwrap(), "d");
    assert_eq!(h.len(), 2);
  }

  #[test]
  fn parse_headers_rejects_missing_equals() {
    assert!(parse_headers("novaluehere").is_err());
  }

  #[test]
  fn parse_headers_rejects_empty_name() {
    assert!(parse_headers("=value").is_err());
  }

  #[test]
  fn parse_headers_rejects_duplicate_key() {
    assert!(parse_headers("a=1,a=2").is_err());
  }

  #[test]
  fn parse_sample_ratio_default_and_bounds() {
    assert_eq!(parse_sample_ratio("1.0").unwrap(), 1.0);
    assert_eq!(parse_sample_ratio("0.25").unwrap(), 0.25);
    assert_eq!(parse_sample_ratio("0").unwrap(), 0.0);
  }

  #[test]
  fn parse_sample_ratio_rejects_garbage_and_out_of_range() {
    assert!(parse_sample_ratio("banana").is_err());
    assert!(parse_sample_ratio("1.5").is_err());
    assert!(parse_sample_ratio("-0.1").is_err());
    assert!(parse_sample_ratio("nan").is_err());
  }

  #[test]
  fn config_from_env_unset_is_noop() {
    // No SECTR_OTLP_* in the test process env — the no-op path.
    let cfg = OtlpConfig::from_env().unwrap();
    assert!(cfg.is_none());
  }

  #[test]
  fn traces_endpoint_appends_signal_path() {
    assert_eq!(
      resolve_traces_endpoint("http://localhost:4318"),
      "http://localhost:4318/v1/traces"
    );
    // Grafana Cloud-style base with a path.
    assert_eq!(
      resolve_traces_endpoint("https://otlp-gateway.example.com/otlp"),
      "https://otlp-gateway.example.com/otlp/v1/traces"
    );
    // Already a full traces URL — left alone (no double append).
    assert_eq!(
      resolve_traces_endpoint("https://collector.example.com/custom/v1/traces"),
      "https://collector.example.com/custom/v1/traces"
    );
  }

  #[test]
  fn metrics_endpoint_appends_signal_path() {
    assert_eq!(
      resolve_metrics_endpoint("http://localhost:4318"),
      "http://localhost:4318/v1/metrics"
    );
    assert_eq!(
      resolve_metrics_endpoint("https://otlp-gateway.example.com/otlp"),
      "https://otlp-gateway.example.com/otlp/v1/metrics"
    );
    assert_eq!(
      resolve_metrics_endpoint("https://collector.example.com/v1/metrics"),
      "https://collector.example.com/v1/metrics"
    );
  }

  /// End-to-end: a closed span must arrive as an OTLP/HTTP-protobuf
  /// POST at the configured endpoint. Proves the exporter + batch
  /// processor + sampler pipeline actually ships spans.
  #[test]
  fn e2e_span_reaches_otlp_endpoint() {
    use std::io::Read;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
      let (mut sock, _) = listener.accept().unwrap();
      let mut buf = [0u8; 4096];
      let mut data = Vec::new();
      while !data.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = sock.read(&mut buf).unwrap();
        assert!(n > 0, "connection closed before request headers");
        data.extend_from_slice(&buf[..n]);
      }
      String::from_utf8_lossy(&data).to_string()
    });

    let config = OtlpConfig {
      endpoint: format!("http://127.0.0.1:{port}"),
      headers: HashMap::new(),
      sample_ratio: 1.0,
    };
    let provider = build_provider(&config, "sectr-server").unwrap();
    let tracer = provider.tracer("e2e-test");
    // The provider alone sees no spans — they flow through a
    // tracing_opentelemetry layer; install one as the thread-local
    // dispatcher for the span's lifetime.
    let subscriber =
      tracing_subscriber::registry().with(tracing_opentelemetry::layer().with_tracer(tracer));
    tracing::subscriber::with_default(subscriber, || {
      let span = tracing::info_span!(target: "e2e_test", "smoke_span");
      let _guard = span.enter();
      tracing::info!("hello otlp");
    }); // span closes → queued for export
    provider.shutdown().unwrap(); // flush

    let request = server.join().unwrap();
    assert!(request.starts_with("POST /v1/traces "), "got: {request}");
    assert!(
      request
        .to_ascii_lowercase()
        .contains("content-type: application/x-protobuf"),
      "expected protobuf content-type, got: {request}"
    );
  }

  /// End-to-end, metrics half: a recorded counter must reach the
  /// endpoint as an OTLP/HTTP-protobuf POST at `/v1/metrics` when the
  /// meter provider shuts down (the periodic reader's final flush). The
  /// metrics reader uses its own background thread — shutdown blocks
  /// until the last collection is exported.
  #[test]
  fn e2e_metric_reaches_otlp_endpoint() {
    use opentelemetry::metrics::MeterProvider as _;
    use std::io::Read;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
      let (mut sock, _) = listener.accept().unwrap();
      let mut buf = [0u8; 4096];
      let mut data = Vec::new();
      // Read headers, then drain the protobuf body via Content-Length
      // — a mid-body socket drop would surface to the exporter as a
      // write error and fail the provider shutdown.
      let content_length = loop {
        let n = sock.read(&mut buf).unwrap();
        assert!(n > 0, "connection closed before request headers");
        data.extend_from_slice(&buf[..n]);
        let text = String::from_utf8_lossy(&data).to_string();
        if let Some(len) = text
          .to_ascii_lowercase()
          .split("\r\n")
          .find_map(|l| l.strip_prefix("content-length:"))
        {
          let len: usize = len.trim().parse().unwrap();
          let head = text.split("\r\n\r\n").next().unwrap().len() + 4;
          if data.len() >= head + len {
            break len;
          }
        }
        if text.contains("\r\n\r\n") && !text.contains("content-length:") {
          break 0; // unlikely, but don't hang on a headerless body
        }
      };
      let _ = content_length;
      // The exporter waits for a response — an empty 200 so the
      // export succeeds and the provider shutdown is clean.
      use std::io::Write;
      sock
        .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n")
        .unwrap();
      String::from_utf8_lossy(&data).to_string()
    });

    let config = OtlpConfig {
      endpoint: format!("http://127.0.0.1:{port}"),
      headers: HashMap::new(),
      sample_ratio: 1.0,
    };
    let provider = build_meter_provider(&config, "sectr-server").unwrap();
    let meter = provider.meter("e2e-test");
    let counter = meter
      .u64_counter("sectr.e2e.smoke")
      .with_description("pipeline smoke")
      .build();
    counter.add(1, &[]);
    provider.shutdown().unwrap(); // final collection + export

    let request = server.join().unwrap();
    assert!(request.starts_with("POST /v1/metrics "), "got: {request}");
    assert!(
      request
        .to_ascii_lowercase()
        .contains("content-type: application/x-protobuf"),
      "expected protobuf content-type, got: {request}"
    );
  }

  /// End-to-end, logs half: a `tracing::info!` event through the
  /// `OpenTelemetryTracingBridge` must reach the endpoint as an
  /// OTLP/HTTP-protobuf POST at `/v1/logs` when the logger provider shuts
  /// down (batch flush). This is what makes plain `tracing` lines
  /// searchable in Grafana Cloud's Loki.
  #[test]
  fn e2e_log_reaches_otlp_endpoint() {
    use std::io::Read;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
      let (mut sock, _) = listener.accept().unwrap();
      let mut buf = [0u8; 8192];
      let mut data = Vec::new();
      while !data.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = sock.read(&mut buf).unwrap();
        assert!(n > 0, "connection closed before request headers");
        data.extend_from_slice(&buf[..n]);
      }
      String::from_utf8_lossy(&data).to_string()
    });

    let config = OtlpConfig {
      endpoint: format!("http://127.0.0.1:{port}"),
      headers: HashMap::new(),
      sample_ratio: 1.0,
    };
    let provider = build_logger_provider(&config, "sectr-server").unwrap();
    let bridge = opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge::new(&provider);
    let subscriber = tracing_subscriber::registry().with(bridge);
    tracing::subscriber::with_default(subscriber, || {
      tracing::info!(session = "e2e", "turn complete");
    });
    provider.shutdown().unwrap(); // flush the batch processor

    let request = server.join().unwrap();
    assert!(request.starts_with("POST /v1/logs "), "got: {request}");
    assert!(
      request
        .to_ascii_lowercase()
        .contains("content-type: application/x-protobuf"),
      "expected protobuf content-type, got: {request}"
    );
  }
}
