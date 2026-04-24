//! Observability: distributed tracing (OpenTelemetry / OTLP) + Prometheus metrics.
//!
//! ## Distributed tracing
//!
//! Set `OTEL_EXPORTER_OTLP_ENDPOINT` to enable span export via OTLP/HTTP
//! (e.g. `http://otelcol:4318`).  `OTEL_SERVICE_NAME` overrides the service
//! name (default: `"ai-reviewer"`).  All existing `tracing::` macro calls are
//! automatically forwarded as OTel events/spans via the `tracing-opentelemetry`
//! bridge — no code changes needed in hot paths.
//!
//! ## Prometheus metrics
//!
//! Metrics are recorded throughout the run and exported at the end.  Two export
//! sinks are supported (both optional, independently configured):
//!
//! - `METRICS_FILE_PATH`          — write Prometheus text format to this path
//!   (use with the node_exporter textfile collector).
//! - `PROMETHEUS_PUSHGATEWAY_URL` — push to a Prometheus Pushgateway, e.g.
//!   `http://pushgateway:9091`.

use std::time::Duration;

use anyhow::{Context, Result};
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::{runtime, Resource};
use prometheus::{
    Encoder, HistogramOpts, HistogramVec, IntCounterVec, IntGauge, Opts, Registry, TextEncoder,
};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

use crate::models::{ReviewError, ReviewResult};

// ── Prometheus metrics ────────────────────────────────────────────────────────

/// All Prometheus metrics for a single review run.
pub struct Metrics {
    pub registry: Registry,
    /// End-to-end latency per reviewer slot (includes retries + backoff).
    pub review_duration_seconds: HistogramVec,
    /// Total LLM completion attempts, labelled by outcome.
    pub review_attempts_total: IntCounterVec,
    /// Model confidence distribution on successful parses.
    pub review_confidence: HistogramVec,
    /// Code findings raised, labelled by severity.
    pub findings_total: IntCounterVec,
    /// Lines in the input diff.
    pub diff_lines: IntGauge,
    /// 1 = gate passed, 0 = gate failed.
    pub gate_passed: IntGauge,
}

impl Metrics {
    pub fn new() -> Result<Self> {
        let registry = Registry::new();

        let review_duration_seconds = HistogramVec::new(
            HistogramOpts::new(
                "ai_reviewer_review_duration_seconds",
                "End-to-end duration per reviewer slot (includes retries and backoff)",
            )
            .buckets(vec![1.0, 5.0, 10.0, 30.0, 60.0, 120.0, 180.0]),
            &["reviewer", "focus"],
        )
        .context("create review_duration_seconds")?;
        registry
            .register(Box::new(review_duration_seconds.clone()))
            .context("register review_duration_seconds")?;

        let review_attempts_total = IntCounterVec::new(
            Opts::new(
                "ai_reviewer_attempts_total",
                "LLM completion attempts per reviewer slot",
            ),
            // outcome: success | timeout | max_retries
            &["reviewer", "focus", "outcome"],
        )
        .context("create review_attempts_total")?;
        registry
            .register(Box::new(review_attempts_total.clone()))
            .context("register review_attempts_total")?;

        let review_confidence = HistogramVec::new(
            HistogramOpts::new(
                "ai_reviewer_confidence",
                "Model confidence score on successful parse",
            )
            .buckets(vec![0.0, 0.5, 0.7, 0.8, 0.85, 0.9, 0.95, 1.0]),
            &["reviewer", "focus", "verdict"],
        )
        .context("create review_confidence")?;
        registry
            .register(Box::new(review_confidence.clone()))
            .context("register review_confidence")?;

        let findings_total = IntCounterVec::new(
            Opts::new(
                "ai_reviewer_findings_total",
                "Code findings raised across all reviewers",
            ),
            &["severity", "reviewer", "focus"],
        )
        .context("create findings_total")?;
        registry
            .register(Box::new(findings_total.clone()))
            .context("register findings_total")?;

        let diff_lines = IntGauge::new(
            "ai_reviewer_diff_lines",
            "Number of lines in the reviewed unified diff",
        )
        .context("create diff_lines")?;
        registry
            .register(Box::new(diff_lines.clone()))
            .context("register diff_lines")?;

        let gate_passed = IntGauge::new(
            "ai_reviewer_gate_passed",
            "1 if the consensus gate passed, 0 if it failed",
        )
        .context("create gate_passed")?;
        registry
            .register(Box::new(gate_passed.clone()))
            .context("register gate_passed")?;

        Ok(Self {
            registry,
            review_duration_seconds,
            review_attempts_total,
            review_confidence,
            findings_total,
            diff_lines,
            gate_passed,
        })
    }

    fn encode_text(&self) -> Result<Vec<u8>> {
        let mut buf = Vec::new();
        TextEncoder::new()
            .encode(&self.registry.gather(), &mut buf)
            .context("encode Prometheus text format")?;
        Ok(buf)
    }

    /// Export metrics to configured sinks (non-fatal on failure).
    ///
    /// Sinks (both optional, independently configured):
    /// - `METRICS_FILE_PATH`          — node_exporter textfile collector
    /// - `PROMETHEUS_PUSHGATEWAY_URL` — Prometheus Pushgateway
    pub async fn export(&self) {
        let payload = match self.encode_text() {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!("Metrics encode failed, skipping export: {e}");
                return;
            }
        };

        // ── Textfile sink ────────────────────────────────────────────────────
        if let Ok(path) = std::env::var("METRICS_FILE_PATH") {
            match std::fs::write(&path, &payload) {
                Ok(()) => tracing::info!(path, "Metrics written to file"),
                Err(e) => tracing::warn!(path, "Metrics file write failed: {e}"),
            }
        }

        // ── Pushgateway sink ─────────────────────────────────────────────────
        if let Ok(gw_url) = std::env::var("PROMETHEUS_PUSHGATEWAY_URL") {
            let push_url = format!("{gw_url}/metrics/job/ai-reviewer");
            match reqwest::Client::new()
                .post(&push_url)
                // Prometheus text format 0.0.4 content type
                .header(
                    "Content-Type",
                    "text/plain; version=0.0.4; charset=utf-8",
                )
                .body(payload)
                .send()
                .await
            {
                Ok(r) if r.status().is_success() => {
                    tracing::info!(%push_url, "Metrics pushed to Pushgateway");
                }
                Ok(r) => tracing::warn!(
                    status = %r.status(),
                    %push_url,
                    "Pushgateway returned non-success status"
                ),
                Err(e) => tracing::warn!(%push_url, "Pushgateway push failed: {e}"),
            }
        }
    }
}

// ── Metrics recording helper ──────────────────────────────────────────────────

/// Record per-reviewer metrics after a review call completes.
///
/// `focus` is one of `"security"`, `"correctness"`, `"performance"`, `"maintainability"`.
pub fn record_review(
    metrics: &Metrics,
    label: &str,
    focus: &str,
    duration: Duration,
    result: &Result<ReviewResult, ReviewError>,
) {
    metrics
        .review_duration_seconds
        .with_label_values(&[label, focus])
        .observe(duration.as_secs_f64());

    match result {
        Ok(r) => {
            metrics
                .review_attempts_total
                .with_label_values(&[label, focus, "success"])
                .inc();
            // Prometheus labels are conventionally lowercase.
            let verdict_lc = r.verdict.to_string().to_lowercase();
            metrics
                .review_confidence
                .with_label_values(&[label, focus, &verdict_lc])
                .observe(r.confidence);
            for f in &r.findings {
                metrics
                    .findings_total
                    .with_label_values(&[&f.severity.to_string(), label, focus])
                    .inc();
            }
        }
        Err(ReviewError::Completion(_)) => {
            metrics
                .review_attempts_total
                .with_label_values(&[label, focus, "timeout"])
                .inc();
        }
        Err(ReviewError::MaxRetriesExceeded { .. }) => {
            metrics
                .review_attempts_total
                .with_label_values(&[label, focus, "max_retries"])
                .inc();
        }
    }
}

// ── Distributed tracing (OTel) ────────────────────────────────────────────────

/// Drop guard that flushes and shuts down the global OTel tracer provider.
pub struct TelemetryGuard;

impl Drop for TelemetryGuard {
    fn drop(&mut self) {
        opentelemetry::global::shutdown_tracer_provider();
    }
}

/// Initialise the global `tracing` subscriber.
///
/// If `OTEL_EXPORTER_OTLP_ENDPOINT` is set, an additional
/// `tracing-opentelemetry` layer is composed in, forwarding all `tracing::`
/// spans/events to the OTLP/HTTP collector.  On failure the tool falls back to
/// `tracing`-only logging with a warning — observability is non-critical.
///
/// Returns a [`TelemetryGuard`] whose `Drop` flushes pending spans.
pub fn init_subscriber(verbose: bool) -> TelemetryGuard {
    let filter = if verbose {
        EnvFilter::new("ai_reviewer=debug,info")
    } else {
        EnvFilter::from_default_env()
    };

    let fmt = tracing_subscriber::fmt::layer().with_target(false);

    let endpoint = std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT").ok();

    if let Some(ep) = endpoint.as_deref() {
        let service = std::env::var("OTEL_SERVICE_NAME")
            .unwrap_or_else(|_| "ai-reviewer".into());

        match build_otlp_tracer(ep, &service) {
            Ok(tracer) => {
                // `tracing_opentelemetry` requires a concrete `SdkTracer`, not
                // the erased `BoxedTracer` returned by `global::tracer()`.
                // Obtaining the tracer directly from the provider before setting
                // the global gives us the correct concrete type.
                let otel = tracing_opentelemetry::layer().with_tracer(tracer);
                tracing_subscriber::registry()
                    .with(filter)
                    .with(fmt)
                    .with(otel)
                    .init();
                // Log after init so the message is captured by the subscriber.
                tracing::info!(
                    endpoint = ep,
                    service = %service,
                    "OTel OTLP tracing enabled"
                );
            }
            Err(e) => {
                tracing_subscriber::registry()
                    .with(filter)
                    .with(fmt)
                    .init();
                tracing::warn!(
                    endpoint = ep,
                    "OTel init failed ({e}), continuing with tracing-only logging"
                );
            }
        }
    } else {
        tracing_subscriber::registry()
            .with(filter)
            .with(fmt)
            .init();
    }

    TelemetryGuard
}

/// Build an OTLP span exporter, install it as the global tracer provider, and
/// return an `SdkTracer` for use with `tracing-opentelemetry`.
///
/// `SdkTracer` (not `BoxedTracer`) is required because
/// `tracing_opentelemetry::OpenTelemetryLayer` needs a type that implements
/// `PreSampledTracer`, which `BoxedTracer` intentionally does not expose.
fn build_otlp_tracer(
    endpoint: &str,
    service_name: &str,
) -> Result<opentelemetry_sdk::trace::Tracer> {
    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_http()
        .with_endpoint(endpoint)
        .build()
        .context("build OTLP span exporter")?;

    let resource = Resource::new(vec![
        opentelemetry::KeyValue::new("service.name", service_name.to_owned()),
        opentelemetry::KeyValue::new("service.version", env!("CARGO_PKG_VERSION")),
    ]);

    let provider = opentelemetry_sdk::trace::TracerProvider::builder()
        .with_batch_exporter(exporter, runtime::Tokio)
        .with_resource(resource)
        .build();

    // Install as global so any downstream code using `global::tracer()` works.
    opentelemetry::global::set_tracer_provider(provider.clone());

    // Return the SdkTracer directly (not via global) for the tracing-otel layer.
    Ok(provider.tracer("ai-reviewer"))
}
