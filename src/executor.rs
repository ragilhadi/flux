use crate::cancel::Cancellation;
use crate::client::HttpClient;
use crate::config::{Config, ResponseAssertions, Scenario};
use crate::metrics::{MetricsCollector, RequestResult};
use crate::redact::Redactor;
use anyhow::Result;
use chrono::Utc;
use jsonpath_rust::JsonPath;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Instant;
use tokio::sync::Semaphore;
use tokio::time::Duration;
use tracing::{debug, error, warn};

/// How often an idle staged-profile worker rechecks whether it has become
/// active, in response to a stage transition or cancellation.
const STAGE_IDLE_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Executor for running load tests
pub struct Executor {
    config: Config,
    client: HttpClient,
    metrics: Arc<MetricsCollector>,
    cancellation: Cancellation,
    redactor: Redactor,
}

impl Executor {
    /// Create a new executor
    pub fn new(
        config: Config,
        metrics: Arc<MetricsCollector>,
        cancellation: Cancellation,
    ) -> Result<Self> {
        let client = HttpClient::new(config.parse_timeout()?)?;
        let redactor = Redactor::from_config(&config);
        Ok(Self {
            config,
            client,
            metrics,
            cancellation,
            redactor,
        })
    }

    /// Run the load test.
    ///
    /// `duration_secs` drives the plain fixed-concurrency path. When
    /// `load_profile` is configured, the profile's own stage or arrival-rate
    /// durations are used instead and `duration_secs` is ignored.
    pub async fn run(&self, duration_secs: u64) -> Result<()> {
        if let Some(stages) = self.config.parsed_stages()? {
            return self.run_staged(stages).await;
        }
        if let Some((target_rps, duration, max_concurrency)) = self.config.parsed_arrival_rate()? {
            return self
                .run_arrival_rate(target_rps, duration, max_concurrency)
                .await;
        }

        let duration = Duration::from_secs(duration_secs);
        let ramp_up = self.config.parse_ramp_up()?;
        self.run_workers(Instant::now(), duration, ramp_up).await
    }

    /// Run a `stages` load profile: concurrency ramps through a sequence of
    /// levels, each held for its configured duration.
    ///
    /// Every worker that could be needed at any stage is spawned up front; a
    /// worker whose id is not yet (or no longer) within the active stage's
    /// `target_concurrency` idles instead of sending requests. This makes
    /// stage transitions predictable — no worker startup latency mid-run —
    /// without needing to cancel and respawn tasks as the target changes.
    async fn run_staged(&self, stages: Vec<(Duration, usize)>) -> Result<()> {
        self.metrics.set_load_profile("stages", None);

        if stages.is_empty() {
            return Ok(());
        }

        let peak_concurrency = stages.iter().map(|(_, target)| *target).max().unwrap_or(0);
        let total_duration: Duration = stages.iter().map(|(duration, _)| *duration).sum();
        let active_target = Arc::new(AtomicUsize::new(stages[0].1));
        // The deadline cannot be fixed until every worker has actually been
        // spawned — computing it up front and handing it to workers as they
        // spawn would charge spawn time (client construction used to make
        // this seconds, not spawning itself) against the configured stage
        // durations. Workers wait on this cell instead of a plain `Instant`.
        let deadline_cell: Arc<OnceLock<Instant>> = Arc::new(OnceLock::new());

        // Stage 0 begins before any worker is spawned, so no request can be
        // recorded before `current_stage` is set — otherwise the first
        // requests would land in the measured window but in no stage at all.
        self.metrics.begin_stage(
            format!("Stage 1 ({} workers)", stages[0].1),
            Some(stages[0].1),
            None,
            stages[0].0.as_secs_f64(),
        );

        let mut handles = Vec::with_capacity(peak_concurrency);
        for worker_id in 0..peak_concurrency {
            if self.cancellation.is_cancelled() {
                debug!("Cancellation requested; not starting worker {}", worker_id);
                break;
            }

            let executor = self.clone_for_worker();
            let target = Arc::clone(&active_target);
            let deadline_cell = Arc::clone(&deadline_cell);
            handles.push(tokio::spawn(async move {
                executor
                    .staged_worker_loop(worker_id, target, deadline_cell)
                    .await;
            }));
        }

        // Every worker that stage 0 could need is now spawned, so the
        // measured load window — and the deadline every worker waits on —
        // start here, mirroring the fixed-concurrency profile's ramp-up
        // boundary.
        self.metrics.mark_load_phase_started();
        let _ = deadline_cell.set(Instant::now() + total_duration);

        for (index, (duration, target_concurrency)) in stages.iter().enumerate() {
            if self.cancellation.is_cancelled() {
                debug!("Cancellation requested; stopping stage schedule");
                break;
            }
            if index > 0 {
                active_target.store(*target_concurrency, Ordering::SeqCst);
                self.metrics.begin_stage(
                    format!("Stage {} ({} workers)", index + 1, target_concurrency),
                    Some(*target_concurrency),
                    None,
                    duration.as_secs_f64(),
                );
            }
            if !self.cancellation.sleep(*duration).await {
                debug!("Cancellation requested during stage {}", index + 1);
                break;
            }
        }
        active_target.store(0, Ordering::SeqCst);

        for handle in handles {
            let _ = handle.await;
        }

        Ok(())
    }

    /// A staged-profile worker: active only while its id is within the
    /// currently active stage's target concurrency, idling otherwise.
    async fn staged_worker_loop(
        &self,
        worker_id: usize,
        active_target: Arc<AtomicUsize>,
        deadline_cell: Arc<OnceLock<Instant>>,
    ) {
        debug!("Staged worker {} started", worker_id);

        // Wait for the run to actually start: the deadline is not set until
        // every worker this stage could need has been spawned.
        while deadline_cell.get().is_none() {
            if self.cancellation.is_cancelled() {
                debug!(
                    "Staged worker {} cancelled before the run started",
                    worker_id
                );
                return;
            }
            if !self.cancellation.sleep(STAGE_IDLE_POLL_INTERVAL).await {
                return;
            }
        }
        let deadline = *deadline_cell.get().expect("checked above");

        while Instant::now() < deadline && !self.cancellation.is_cancelled() {
            if worker_id >= active_target.load(Ordering::SeqCst) {
                if !self.cancellation.sleep(STAGE_IDLE_POLL_INTERVAL).await {
                    break;
                }
                continue;
            }

            if self.config.is_simple_mode() {
                self.execute_simple_request().await;
                if let Some(think_time) = self
                    .config
                    .parse_think_time()
                    .expect("Validated think_time became invalid")
                {
                    if !self.cancellation.sleep(think_time).await {
                        break;
                    }
                }
            } else {
                self.execute_scenarios().await;
            }

            if self.config.mode == "sync"
                && !self.cancellation.sleep(Duration::from_millis(10)).await
            {
                break;
            }
        }

        debug!("Staged worker {} finished", worker_id);
    }

    /// Run an `arrival_rate` load profile: request starts are paced at
    /// `target_rps` instead of being driven by a fixed worker loop.
    ///
    /// In-flight requests are bounded by `max_concurrency` via a semaphore: a
    /// pacing tick that finds no free permit is counted as saturation and
    /// skipped rather than queued, so a target the backend cannot sustain
    /// shows up in the report instead of creating unbounded pending work.
    async fn run_arrival_rate(
        &self,
        target_rps: f64,
        duration: Duration,
        max_concurrency: usize,
    ) -> Result<()> {
        self.metrics
            .set_load_profile("arrival_rate", Some(target_rps));
        self.metrics.begin_stage(
            format!("Arrival rate ({target_rps:.2} req/s)"),
            None,
            Some(target_rps),
            duration.as_secs_f64(),
        );
        self.metrics.mark_load_phase_started();

        let semaphore = Arc::new(Semaphore::new(max_concurrency));
        // A period of exactly zero would make `interval` panic, so a huge
        // target rate is clamped to a still-very-fast one instead of failing.
        let period = Duration::from_secs_f64((1.0 / target_rps).max(0.000_001));
        let mut ticker = tokio::time::interval(period);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let deadline = Instant::now() + duration;

        loop {
            if Instant::now() >= deadline || self.cancellation.is_cancelled() {
                break;
            }

            tokio::select! {
                biased;
                _ = self.cancellation.cancelled() => break,
                _ = ticker.tick() => {}
            }

            if Instant::now() >= deadline || self.cancellation.is_cancelled() {
                break;
            }

            self.metrics.record_scheduled_tick();
            match Arc::clone(&semaphore).try_acquire_owned() {
                Ok(permit) => {
                    let executor = self.clone_for_worker();
                    tokio::spawn(async move {
                        let _permit = permit;
                        if executor.config.is_simple_mode() {
                            executor.execute_simple_request().await;
                        } else {
                            executor.execute_scenarios().await;
                        }
                    });
                }
                Err(_) => {
                    self.metrics.record_saturation();
                }
            }
        }

        // Wait for every in-flight request to finish: once every permit is
        // back, no spawned task can still be running. Config validation
        // rejects a max_concurrency that would not fit in a u32, so this
        // cast cannot truncate.
        let _ = semaphore.acquire_many(max_concurrency as u32).await;

        Ok(())
    }

    /// Start workers, optionally spreading their startup across a ramp-up
    /// period.
    ///
    /// Ramp-up is warm-up time that precedes the measured window: the
    /// configured `duration` starts once the last worker has been started, so
    /// every worker gets a full-length active period rather than losing its
    /// share of the run to the time it waited to start.
    async fn run_workers(
        &self,
        start: Instant,
        duration: Duration,
        ramp_up: Option<Duration>,
    ) -> Result<()> {
        let mut handles = vec![];
        let start_delay =
            worker_start_delay(self.config.concurrency, ramp_up, self.config.mode == "sync");
        let deadline = start + startup_span(self.config.concurrency, start_delay) + duration;

        for worker_id in 0..self.config.concurrency {
            // Stop spawning additional workers once cancellation is requested.
            if self.cancellation.is_cancelled() {
                debug!("Cancellation requested; not starting worker {}", worker_id);
                break;
            }

            let executor = self.clone_for_worker();

            let handle = tokio::spawn(async move {
                executor.worker_loop(worker_id, deadline).await;
            });

            handles.push(handle);
            if worker_id + 1 < self.config.concurrency
                && !start_delay.is_zero()
                && !self.cancellation.sleep(start_delay).await
            {
                debug!("Cancellation requested during ramp-up");
                break;
            }
        }

        // Every worker is running: the measured load window starts here.
        self.metrics.mark_load_phase_started();

        // Wait for all workers to complete
        for handle in handles {
            let _ = handle.await;
        }

        Ok(())
    }

    /// Worker loop that executes requests until the shared test deadline.
    async fn worker_loop(&self, worker_id: usize, deadline: Instant) {
        debug!("Worker {} started", worker_id);

        while Instant::now() < deadline && !self.cancellation.is_cancelled() {
            if self.config.is_simple_mode() {
                self.execute_simple_request().await;
                if let Some(think_time) = self
                    .config
                    .parse_think_time()
                    .expect("Validated think_time became invalid")
                {
                    if !self.cancellation.sleep(think_time).await {
                        break;
                    }
                }
            } else {
                self.execute_scenarios().await;
            }

            // Small delay in sync mode
            if self.config.mode == "sync"
                && !self.cancellation.sleep(Duration::from_millis(10)).await
            {
                break;
            }
        }

        debug!("Worker {} finished", worker_id);
    }

    /// Execute a simple request
    async fn execute_simple_request(&self) {
        let retry_delay = self
            .config
            .parse_retry_delay()
            .expect("Validated retry_delay became invalid");
        let mut attempt = 0;

        let (result, start_time, end_time, latency) = loop {
            let start_time = Utc::now();
            let request_start = Instant::now();
            let result = self
                .client
                .execute_simple(
                    self.config.target.as_ref().unwrap(),
                    self.config.method.as_deref().unwrap_or("GET"),
                    &self.config.headers,
                    self.config.body.as_deref(),
                    self.config.multipart.as_ref(),
                )
                .await;
            let latency = request_start.elapsed().as_millis() as u64;
            let end_time = Utc::now();

            if attempt < self.config.retry_count
                && !self.cancellation.is_cancelled()
                && should_retry_response(&result, &self.config.retry_on_status)
            {
                attempt += 1;
                debug!(
                    "Retrying request after attempt {} of {}",
                    attempt,
                    self.config.retry_count.saturating_add(1)
                );
                if !self.cancellation.sleep(retry_delay).await {
                    break (result, start_time, end_time, latency);
                }
                continue;
            }

            break (result, start_time, end_time, latency);
        };

        let request_result = match result {
            Ok(response) => {
                let status = response.status();
                let error = if !status.is_success() {
                    Some(format!("HTTP {}", status.as_u16()))
                } else {
                    None
                };
                RequestResult {
                    scenario_name: None,
                    latency_ms: latency,
                    status_code: status.as_u16(),
                    error,
                    request_start_timestamp: start_time,
                    request_end_timestamp: end_time,
                }
            }
            Err(e) => {
                error!("Request failed: {}", e);
                RequestResult {
                    scenario_name: None,
                    latency_ms: latency,
                    status_code: 0,
                    error: Some(e.to_string()),
                    request_start_timestamp: start_time,
                    request_end_timestamp: end_time,
                }
            }
        };

        self.record_result(request_result);
    }

    /// Execute all scenarios in sequence
    async fn execute_scenarios(&self) {
        let mut variables: HashMap<String, String> = HashMap::new();
        let mut executed: HashSet<String> = HashSet::new();

        for scenario in &self.config.scenarios {
            if self.cancellation.is_cancelled() {
                debug!("Cancellation requested; stopping scenario sequence");
                break;
            }

            // Check dependencies
            if let Some(ref depends_on) = scenario.depends_on {
                // Configuration validation guarantees the dependency exists and
                // runs earlier, so reaching this point means it failed at
                // runtime. Count the skip so reports show the lost work.
                if !Self::has_executed_scenario(depends_on, &executed) {
                    warn!(
                        "Skipping scenario '{}' - dependency '{}' did not succeed",
                        scenario.name, depends_on
                    );
                    self.metrics.record_skipped_scenario(&scenario.name);
                    continue;
                }
            }

            let retry_count = self.config.retry_count_for(scenario);
            let retry_delay = self
                .config
                .retry_delay_for(scenario)
                .expect("Validated retry_delay became invalid");
            let retry_statuses = self.config.retry_statuses_for(scenario);
            let mut attempt = 0;

            let (result, start_time, end_time, latency) = loop {
                let start_time = Utc::now();
                let request_start = Instant::now();
                let result = self
                    .client
                    .execute_scenario(self.config.target.as_deref(), scenario, &variables)
                    .await;
                let latency = request_start.elapsed().as_millis() as u64;
                let end_time = Utc::now();

                if attempt < retry_count
                    && !self.cancellation.is_cancelled()
                    && should_retry_response(&result, retry_statuses)
                {
                    attempt += 1;
                    debug!(
                        "Retrying scenario '{}' after attempt {} of {}",
                        scenario.name,
                        attempt,
                        retry_count.saturating_add(1)
                    );
                    if !self.cancellation.sleep(retry_delay).await {
                        break (result, start_time, end_time, latency);
                    }
                    continue;
                }

                break (result, start_time, end_time, latency);
            };

            match result {
                Ok(response) => {
                    let status = response.status();
                    let status_code = status.as_u16();
                    let mut assertion_error = response_status_error(
                        status_code,
                        status.is_success(),
                        scenario.assertions.as_ref(),
                    );
                    let needs_body = !scenario.extract.is_empty()
                        || scenario
                            .assertions
                            .as_ref()
                            .and_then(|assertions| assertions.body_contains.as_ref())
                            .is_some();
                    let body = if needs_body {
                        match response.text().await {
                            Ok(body) => Some(body),
                            Err(error) => {
                                if assertion_error.is_none() {
                                    assertion_error = Some(format!(
                                        "Failed to read response body for assertion: {error}"
                                    ));
                                }
                                None
                            }
                        }
                    } else {
                        None
                    };

                    if assertion_error.is_none() {
                        assertion_error =
                            response_body_error(body.as_deref(), scenario.assertions.as_ref());
                    }

                    // Extract variables only after all response assertions pass.
                    if assertion_error.is_none() && !scenario.extract.is_empty() {
                        if let Some(body) = body.as_deref() {
                            self.extract_variables(body, scenario, &mut variables);
                        }
                    }

                    // Only mark as executed on a successful response
                    if assertion_error.is_none() {
                        executed.insert(scenario.name.clone());
                    }

                    let request_result = RequestResult {
                        scenario_name: Some(scenario.name.clone()),
                        latency_ms: latency,
                        status_code,
                        error: assertion_error,
                        request_start_timestamp: start_time,
                        request_end_timestamp: end_time,
                    };

                    self.record_result(request_result);
                }
                Err(e) => {
                    error!("Scenario '{}' failed: {}", scenario.name, e);

                    let request_result = RequestResult {
                        scenario_name: Some(scenario.name.clone()),
                        latency_ms: latency,
                        status_code: 0,
                        error: Some(e.to_string()),
                        request_start_timestamp: start_time,
                        request_end_timestamp: end_time,
                    };

                    self.record_result(request_result);
                }
            }

            if let Some(think_time) = self
                .config
                .parse_think_time_for(scenario)
                .expect("Validated scenario think_time became invalid")
            {
                if !self.cancellation.sleep(think_time).await {
                    break;
                }
            }
        }
    }

    /// Extract variables from response body using JSONPath
    ///
    /// The body is parsed once and reused for every `extract` entry, rather
    /// than once per variable: a scenario extracting several variables from
    /// a large body used to pay for a full re-parse of it per variable, on
    /// the hot path.
    fn extract_variables(
        &self,
        body: &str,
        scenario: &Scenario,
        variables: &mut HashMap<String, String>,
    ) {
        let json: serde_json::Value = match serde_json::from_str(body) {
            Ok(json) => json,
            Err(e) => {
                warn!("Failed to parse JSON response: {}", e);
                return;
            }
        };

        for (var_name, json_path) in &scenario.extract {
            match json.query(json_path) {
                Ok(results) => {
                    if let Some(value) = results.first() {
                        let extracted = match value {
                            serde_json::Value::String(s) => s.clone(),
                            serde_json::Value::Number(n) => n.to_string(),
                            serde_json::Value::Bool(b) => b.to_string(),
                            _ => value.to_string(),
                        };
                        debug!("Extracted variable '{}' = '{}'", var_name, extracted);
                        variables.insert(var_name.clone(), extracted);
                    }
                }
                Err(e) => {
                    warn!("JSONPath error for '{}': {}", json_path, e);
                }
            }
        }
    }

    /// Check if a scenario has been executed
    fn has_executed_scenario(scenario_name: &str, executed: &HashSet<String>) -> bool {
        executed.contains(scenario_name)
    }

    /// Clone executor for worker.
    ///
    /// Shares `self.client` rather than building a new one: `HttpClient` is
    /// `Arc`-backed, so every worker reuses the same connection pool instead
    /// of opening its own. Building a fresh client per worker (or, worse, per
    /// request in `run_arrival_rate`) was measured at several milliseconds
    /// each — enough to silently steal from a staged profile's stage
    /// durations and to cap arrival-rate pacing well below its target.
    fn clone_for_worker(&self) -> Self {
        Self {
            config: self.config.clone(),
            client: self.client.clone(),
            metrics: Arc::clone(&self.metrics),
            cancellation: self.cancellation.clone(),
            redactor: self.redactor.clone(),
        }
    }

    /// Record a result, redacting its error string first.
    ///
    /// A request error can quote the full request URL — including any
    /// extracted token in a query string, which the config-derived literals
    /// alone would not catch — so every result is redacted the same way
    /// before it reaches the collector, rather than only when a report or
    /// the dashboard later reads it back.
    fn record_result(&self, mut result: RequestResult) {
        result.error = self.redactor.redact_optional(result.error.as_deref());
        self.metrics.record(result);
    }
}

/// Total time spent starting workers: the delay applies between starts, so the
/// first worker starts immediately and the last one starts after `n - 1` gaps.
fn startup_span(concurrency: usize, start_delay: Duration) -> Duration {
    let gaps = u32::try_from(concurrency.saturating_sub(1)).unwrap_or(u32::MAX);
    start_delay.saturating_mul(gaps)
}

fn worker_start_delay(concurrency: usize, ramp_up: Option<Duration>, sync_mode: bool) -> Duration {
    match ramp_up {
        Some(ramp_up) => Duration::from_secs_f64(ramp_up.as_secs_f64() / concurrency as f64),
        None if sync_mode => Duration::from_millis(10),
        None => Duration::ZERO,
    }
}

fn should_retry_response(
    result: &anyhow::Result<reqwest::Response>,
    retry_statuses: &[u16],
) -> bool {
    match result {
        Err(_) => true,
        Ok(response) => retry_statuses.contains(&response.status().as_u16()),
    }
}

fn response_status_error(
    actual: u16,
    is_success: bool,
    assertions: Option<&ResponseAssertions>,
) -> Option<String> {
    if let Some(expected) = assertions.and_then(|assertions| assertions.status_code) {
        if actual != expected {
            return Some(format!(
                "Status assertion failed: expected {expected}, received {actual}"
            ));
        }
        return None;
    }

    (!is_success).then(|| format!("HTTP {actual}"))
}

fn response_body_error(
    body: Option<&str>,
    assertions: Option<&ResponseAssertions>,
) -> Option<String> {
    let expected = assertions.and_then(|assertions| assertions.body_contains.as_deref())?;
    match body {
        Some(body) if body.contains(expected) => None,
        Some(_) => Some(format!(
            "Body assertion failed: response does not contain '{expected}'"
        )),
        None => Some("Body assertion failed: response body is unavailable".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{LoadProfile, OutputConfig, Stage};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::time::sleep;

    #[test]
    fn test_executor_creation() {
        let config = Config {
            target: Some("http://example.com".to_string()),
            method: Some("GET".to_string()),
            headers: HashMap::new(),
            body: None,
            multipart: None,
            scenarios: vec![],
            concurrency: 10,
            duration: "30s".to_string(),
            timeout: "30s".to_string(),
            ramp_up: None,
            load_profile: None,
            think_time: None,
            retry_count: 0,
            retry_delay: None,
            retry_on_status: vec![],
            assertions: None,
            prometheus_port: None,
            prometheus_bind: "127.0.0.1".to_string(),
            live_dashboard: None,
            mode: "async".to_string(),
            output: OutputConfig {
                json: "/app/results/output.json".to_string(),
                html: "/app/results/output.html".to_string(),
                csv: None,
                max_results: 0,
            },
        };

        let metrics = Arc::new(MetricsCollector::new());
        let executor = Executor::new(config, metrics, Cancellation::new());

        assert!(executor.is_ok());
    }

    #[test]
    fn test_has_executed_scenario() {
        let mut executed = HashSet::new();

        // Not yet executed
        assert!(!Executor::has_executed_scenario("login", &executed));

        // After inserting
        executed.insert("login".to_string());
        assert!(Executor::has_executed_scenario("login", &executed));

        // Different name still not executed
        assert!(!Executor::has_executed_scenario("get-profile", &executed));
    }

    #[test]
    fn test_ramp_up_start_delay() {
        assert_eq!(
            worker_start_delay(10, Some(Duration::from_secs(10)), false),
            Duration::from_secs(1)
        );
        assert_eq!(worker_start_delay(10, None, false), Duration::ZERO);
        assert_eq!(
            worker_start_delay(10, None, true),
            Duration::from_millis(10)
        );
    }

    #[test]
    fn test_startup_span_covers_every_worker_start() {
        // 10 workers one second apart: the last one starts after nine seconds.
        assert_eq!(
            startup_span(
                10,
                worker_start_delay(10, Some(Duration::from_secs(10)), false)
            ),
            Duration::from_secs(9)
        );
        // A single worker starts immediately, so there is no ramp-up to absorb.
        assert_eq!(
            startup_span(
                1,
                worker_start_delay(1, Some(Duration::from_secs(10)), false)
            ),
            Duration::ZERO
        );
        assert_eq!(startup_span(10, Duration::ZERO), Duration::ZERO);
    }

    /// Every worker must get its full active window regardless of when it
    /// started, so the deadline is pushed out by the whole ramp-up span.
    #[test]
    fn test_deadline_gives_each_worker_the_full_duration() {
        let duration = Duration::from_secs(10);
        let start = Instant::now();

        for (concurrency, ramp_up) in [
            (10, Some(Duration::from_secs(10))),
            (10, None),
            (1, Some(Duration::from_secs(10))),
            (1, None),
        ] {
            let start_delay = worker_start_delay(concurrency, ramp_up, false);
            let deadline = start + startup_span(concurrency, start_delay) + duration;

            for worker_id in 0..concurrency {
                let worker_start = start + start_delay * worker_id as u32;
                assert!(
                    deadline.duration_since(worker_start) >= duration,
                    "worker {worker_id} of {concurrency} lost part of its window"
                );
            }
        }
    }

    #[tokio::test]
    async fn test_ramp_up_equal_to_duration_still_runs_every_worker() {
        let (address, server) = spawn_status_server(200).await;
        let mut config = cancellation_test_config(format!("http://{address}"));
        config.concurrency = 4;
        // Ramp-up as long as the duration used to leave the last workers with
        // no time to send a single request.
        config.ramp_up = Some("2s".to_string());
        config.duration = "2s".to_string();
        let metrics = Arc::new(MetricsCollector::new());
        let executor = Executor::new(config, Arc::clone(&metrics), Cancellation::new()).unwrap();

        let started = Instant::now();
        executor.run(2).await.unwrap();
        server.abort();

        // Ramp-up (1.5s of gaps) is additive, so the run takes longer than the
        // configured duration but each worker gets its own two seconds.
        assert!(started.elapsed() >= Duration::from_secs(3));
        let summary = metrics.generate_summary();
        assert!(summary.total_requests > 0);
        assert!(summary.ramp_up_secs > 0.0);
        assert!(summary.measured_duration_secs >= 1.5);
        assert!(summary.measured_requests > 0);
    }

    #[tokio::test]
    async fn test_no_ramp_up_leaves_timing_unchanged() {
        let (address, server) = spawn_status_server(200).await;
        let mut config = cancellation_test_config(format!("http://{address}"));
        config.concurrency = 2;
        config.duration = "1s".to_string();
        let metrics = Arc::new(MetricsCollector::new());
        let executor = Executor::new(config, Arc::clone(&metrics), Cancellation::new()).unwrap();

        let started = Instant::now();
        executor.run(1).await.unwrap();
        server.abort();

        assert!(started.elapsed() < Duration::from_secs(3));
        let summary = metrics.generate_summary();
        assert!(summary.ramp_up_secs < 0.5);
        assert!(summary.measured_requests > 0);
        assert!(summary.measured_requests <= summary.total_requests);
    }

    #[tokio::test]
    async fn test_retry_records_only_successful_final_attempt() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            for status in [503, 200] {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = [0_u8; 1024];
                let _ = socket.read(&mut request).await.unwrap();
                let reason = if status == 200 {
                    "OK"
                } else {
                    "Service Unavailable"
                };
                let response = format!(
                    "HTTP/1.1 {status} {reason}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                );
                socket.write_all(response.as_bytes()).await.unwrap();
            }
        });

        let config = Config {
            target: Some(format!("http://{address}")),
            method: Some("GET".to_string()),
            headers: HashMap::new(),
            body: None,
            multipart: None,
            scenarios: vec![],
            concurrency: 1,
            duration: "1s".to_string(),
            timeout: "5s".to_string(),
            ramp_up: None,
            load_profile: None,
            think_time: None,
            retry_count: 1,
            retry_delay: Some("0s".to_string()),
            retry_on_status: vec![503],
            assertions: None,
            prometheus_port: None,
            prometheus_bind: "127.0.0.1".to_string(),
            live_dashboard: None,
            mode: "async".to_string(),
            output: OutputConfig {
                json: "output.json".to_string(),
                html: "output.html".to_string(),
                csv: None,
                max_results: 0,
            },
        };
        let metrics = Arc::new(MetricsCollector::new());
        let executor = Executor::new(config, Arc::clone(&metrics), Cancellation::new()).unwrap();

        executor.execute_simple_request().await;
        server.await.unwrap();

        let results = metrics.get_results();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].status_code, 200);
        assert!(results[0].error.is_none());
    }

    #[test]
    fn test_response_assertions() {
        let assertions = ResponseAssertions {
            status_code: Some(201),
            body_contains: Some("created".to_string()),
        };

        assert!(response_status_error(201, true, Some(&assertions)).is_none());
        assert!(response_status_error(200, true, Some(&assertions))
            .unwrap()
            .contains("expected 201"));
        assert!(response_body_error(Some("resource created"), Some(&assertions)).is_none());
        assert!(response_body_error(Some("missing"), Some(&assertions))
            .unwrap()
            .contains("does not contain"));
    }

    #[test]
    fn test_expected_non_success_status_can_pass() {
        let assertions = ResponseAssertions {
            status_code: Some(404),
            body_contains: None,
        };
        assert!(response_status_error(404, false, Some(&assertions)).is_none());
    }

    /// Accept connections until dropped, always answering with `status`.
    async fn spawn_status_server(
        status: u16,
    ) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    let mut request = [0_u8; 1024];
                    let _ = socket.read(&mut request).await;
                    let response = format!(
                        "HTTP/1.1 {status} OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                });
            }
        });
        (address, handle)
    }

    fn cancellation_test_config(target: String) -> Config {
        Config {
            target: Some(target),
            method: Some("GET".to_string()),
            headers: HashMap::new(),
            body: None,
            multipart: None,
            scenarios: vec![],
            concurrency: 1,
            duration: "300s".to_string(),
            timeout: "5s".to_string(),
            ramp_up: None,
            load_profile: None,
            think_time: None,
            retry_count: 0,
            retry_delay: None,
            retry_on_status: vec![],
            assertions: None,
            prometheus_port: None,
            prometheus_bind: "127.0.0.1".to_string(),
            live_dashboard: None,
            mode: "async".to_string(),
            output: OutputConfig {
                json: "output.json".to_string(),
                html: "output.html".to_string(),
                csv: None,
                max_results: 0,
            },
        }
    }

    #[tokio::test]
    async fn test_recorded_errors_are_redacted_before_reaching_the_collector() {
        // A connection failure's error text quotes the request URL. A secret
        // header value embedded in that URL used to reach the JSON/CSV
        // reports verbatim, because redaction was only ever applied when the
        // dashboard read results back — never when they were recorded.
        const SECRET: &str = "MySecretHeaderValueXYZ";
        let mut config = cancellation_test_config("http://127.0.0.1:1".to_string());
        config.target = None;
        config.scenarios = vec![Scenario {
            name: "call".to_string(),
            method: "GET".to_string(),
            url: format!("http://127.0.0.1:1/path?tag={SECRET}"),
            headers: HashMap::from([("Authorization".to_string(), SECRET.to_string())]),
            body: None,
            multipart: None,
            extract: HashMap::new(),
            depends_on: None,
            think_time: None,
            retry_count: None,
            retry_delay: None,
            retry_on_status: None,
            assertions: None,
        }];
        let metrics = Arc::new(MetricsCollector::new());
        let executor = Executor::new(config, Arc::clone(&metrics), Cancellation::new()).unwrap();

        executor.execute_scenarios().await;

        let results = metrics.get_results();
        assert_eq!(results.len(), 1);
        let error = results[0].error.as_deref().expect("connection should fail");
        assert!(
            !error.contains(SECRET),
            "secret leaked into report: {error}"
        );
    }

    #[tokio::test]
    async fn test_runtime_dependency_failure_is_counted_as_skipped() {
        let (address, server) = spawn_status_server(500).await;
        let mut config = cancellation_test_config(format!("http://{address}"));
        config.scenarios = vec![
            Scenario {
                name: "login".to_string(),
                method: "GET".to_string(),
                url: "/login".to_string(),
                headers: HashMap::new(),
                body: None,
                multipart: None,
                extract: HashMap::new(),
                depends_on: None,
                think_time: None,
                retry_count: None,
                retry_delay: None,
                retry_on_status: None,
                assertions: None,
            },
            Scenario {
                name: "profile".to_string(),
                method: "GET".to_string(),
                url: "/profile".to_string(),
                headers: HashMap::new(),
                body: None,
                multipart: None,
                extract: HashMap::new(),
                depends_on: Some("login".to_string()),
                think_time: None,
                retry_count: None,
                retry_delay: None,
                retry_on_status: None,
                assertions: None,
            },
        ];
        let metrics = Arc::new(MetricsCollector::new());
        let executor = Executor::new(config, Arc::clone(&metrics), Cancellation::new()).unwrap();

        executor.execute_scenarios().await;
        server.abort();

        let summary = metrics.generate_summary();
        // The dependency failed at runtime, so the dependent step is skipped
        // and the skip is visible in the report instead of vanishing.
        assert_eq!(summary.skipped_scenarios.get("profile"), Some(&1));
        assert!(!summary.per_scenario.contains_key("profile"));
        assert_eq!(summary.failed_requests, 1);
    }

    #[tokio::test]
    async fn test_cancellation_stops_active_run() {
        let (address, server) = spawn_status_server(200).await;
        let config = cancellation_test_config(format!("http://{address}"));
        let metrics = Arc::new(MetricsCollector::new());
        let cancellation = Cancellation::new();
        let executor = Executor::new(config, Arc::clone(&metrics), cancellation.clone()).unwrap();

        let canceller = cancellation.clone();
        tokio::spawn(async move {
            sleep(Duration::from_millis(100)).await;
            canceller.cancel();
        });

        let started = Instant::now();
        executor.run(300).await.unwrap();
        server.abort();

        // Without cancellation this would run for the configured 300 seconds.
        assert!(started.elapsed() < Duration::from_secs(30));
        assert!(!metrics.get_results().is_empty());
    }

    #[tokio::test]
    async fn test_cancellation_during_ramp_up_stops_spawning_workers() {
        let (address, server) = spawn_status_server(200).await;
        let mut config = cancellation_test_config(format!("http://{address}"));
        config.concurrency = 20;
        config.ramp_up = Some("200s".to_string());
        let metrics = Arc::new(MetricsCollector::new());
        let cancellation = Cancellation::new();
        let executor = Executor::new(config, Arc::clone(&metrics), cancellation.clone()).unwrap();

        let canceller = cancellation.clone();
        tokio::spawn(async move {
            sleep(Duration::from_millis(100)).await;
            canceller.cancel();
        });

        let started = Instant::now();
        executor.run(300).await.unwrap();
        server.abort();

        // Each worker start is 10s apart, so only cancellation can end this quickly.
        assert!(started.elapsed() < Duration::from_secs(30));
    }

    #[tokio::test]
    async fn test_cancellation_interrupts_retry_delay() {
        let (address, server) = spawn_status_server(503).await;
        let mut config = cancellation_test_config(format!("http://{address}"));
        config.retry_count = 3;
        config.retry_delay = Some("120s".to_string());
        config.retry_on_status = vec![503];
        let metrics = Arc::new(MetricsCollector::new());
        let cancellation = Cancellation::new();
        let executor = Executor::new(config, Arc::clone(&metrics), cancellation.clone()).unwrap();

        let canceller = cancellation.clone();
        tokio::spawn(async move {
            sleep(Duration::from_millis(100)).await;
            canceller.cancel();
        });

        let started = Instant::now();
        executor.execute_simple_request().await;
        server.abort();

        assert!(started.elapsed() < Duration::from_secs(30));
        let results = metrics.get_results();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].status_code, 503);
    }

    /// Accept connections until dropped, sleeping `delay` before answering
    /// with `status` — used to make a target look slow or overloaded.
    async fn spawn_delayed_status_server(
        status: u16,
        delay: Duration,
    ) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    let mut request = [0_u8; 1024];
                    let _ = socket.read(&mut request).await;
                    sleep(delay).await;
                    let response = format!(
                        "HTTP/1.1 {status} OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                });
            }
        });
        (address, handle)
    }

    #[tokio::test]
    async fn test_staged_profile_ramps_through_each_stage_on_schedule() {
        let (address, server) = spawn_status_server(200).await;
        let mut config = cancellation_test_config(format!("http://{address}"));
        config.load_profile = Some(LoadProfile::Stages {
            stages: vec![
                Stage {
                    duration: "1s".to_string(),
                    target_concurrency: 2,
                },
                Stage {
                    duration: "1s".to_string(),
                    target_concurrency: 8,
                },
            ],
        });
        let metrics = Arc::new(MetricsCollector::new());
        let executor = Executor::new(config, Arc::clone(&metrics), Cancellation::new()).unwrap();

        let started = Instant::now();
        executor.run(0).await.unwrap();
        server.abort();

        // Two 1s stages: comfortably over the per-stage duration so both
        // stages actually ran, with slack for a loaded test environment.
        assert!(started.elapsed() >= Duration::from_millis(1_800));
        assert!(started.elapsed() < Duration::from_secs(30));

        let summary = metrics.generate_summary();
        assert_eq!(summary.stages.len(), 2);
        assert_eq!(summary.stages[0].label, "Stage 1 (2 workers)");
        assert_eq!(summary.stages[0].target_concurrency, Some(2));
        assert_eq!(summary.stages[1].label, "Stage 2 (8 workers)");
        assert_eq!(summary.stages[1].target_concurrency, Some(8));
        // Both stages actually reached their configured concurrency and
        // issued requests on schedule.
        assert!(summary.stages[0].metrics.total_requests > 0);
        assert!(summary.stages[1].metrics.total_requests > 0);
        assert!(summary.total_requests > 0);
        // Every request lands in exactly one stage — none should be lost in
        // the gap between worker startup and the first stage beginning.
        let stage_total: usize = summary
            .stages
            .iter()
            .map(|s| s.metrics.total_requests)
            .sum();
        assert_eq!(stage_total, summary.total_requests);
    }

    #[tokio::test]
    async fn test_staged_profile_with_high_peak_concurrency_does_not_lose_a_stage() {
        // A high peak concurrency used to make worker startup itself (client
        // construction per worker, previously) eat into the deadline
        // computed before spawning began, sometimes leaving a later stage no
        // time to run at all. With workers spawned before the deadline is
        // fixed, every stage should still get its planned share of the run.
        let (address, server) = spawn_status_server(200).await;
        let mut config = cancellation_test_config(format!("http://{address}"));
        config.load_profile = Some(LoadProfile::Stages {
            stages: vec![
                Stage {
                    duration: "400ms".to_string(),
                    target_concurrency: 50,
                },
                Stage {
                    duration: "400ms".to_string(),
                    target_concurrency: 300,
                },
                Stage {
                    duration: "400ms".to_string(),
                    target_concurrency: 100,
                },
            ],
        });
        let metrics = Arc::new(MetricsCollector::new());
        let executor = Executor::new(config, Arc::clone(&metrics), Cancellation::new()).unwrap();

        let started = Instant::now();
        executor.run(0).await.unwrap();
        server.abort();

        // Three 400ms stages: comfortably bounded even with scheduler slack.
        // A regression that charged spawn time to the deadline used to blow
        // this well past a few seconds.
        assert!(started.elapsed() < Duration::from_secs(5));

        let summary = metrics.generate_summary();
        assert_eq!(summary.stages.len(), 3);
        for stage in &summary.stages {
            assert!(
                stage.metrics.total_requests > 0,
                "stage '{}' recorded no requests",
                stage.label
            );
            // Each stage's observed duration should be in the right ballpark
            // of its planned one, not near-zero (starved) or the sum of every
            // stage (a stalled schedule).
            assert!(
                stage.observed_duration_secs > 0.1 && stage.observed_duration_secs < 2.0,
                "stage '{}' observed duration {} looks wrong",
                stage.label,
                stage.observed_duration_secs
            );
        }
    }

    #[tokio::test]
    async fn test_staged_profile_cancellation_stops_quickly() {
        let (address, server) = spawn_status_server(200).await;
        let mut config = cancellation_test_config(format!("http://{address}"));
        config.load_profile = Some(LoadProfile::Stages {
            stages: vec![
                Stage {
                    duration: "200s".to_string(),
                    target_concurrency: 3,
                },
                Stage {
                    duration: "200s".to_string(),
                    target_concurrency: 20,
                },
            ],
        });
        let metrics = Arc::new(MetricsCollector::new());
        let cancellation = Cancellation::new();
        let executor = Executor::new(config, Arc::clone(&metrics), cancellation.clone()).unwrap();

        let canceller = cancellation.clone();
        tokio::spawn(async move {
            sleep(Duration::from_millis(100)).await;
            canceller.cancel();
        });

        let started = Instant::now();
        executor.run(0).await.unwrap();
        server.abort();

        assert!(started.elapsed() < Duration::from_secs(30));
        assert!(!metrics.get_results().is_empty());
    }

    #[tokio::test]
    async fn test_arrival_rate_paces_near_the_target_rate() {
        let (address, server) = spawn_status_server(200).await;
        let mut config = cancellation_test_config(format!("http://{address}"));
        config.load_profile = Some(LoadProfile::ArrivalRate {
            target_rps: 20.0,
            duration: "1s".to_string(),
            max_concurrency: 200,
        });
        let metrics = Arc::new(MetricsCollector::new());
        let executor = Executor::new(config, Arc::clone(&metrics), Cancellation::new()).unwrap();

        executor.run(0).await.unwrap();
        server.abort();

        let summary = metrics.generate_summary();
        let profile = summary.load_profile.expect("arrival_rate profile summary");
        assert_eq!(profile.kind, "arrival_rate");
        assert_eq!(profile.target_rps, Some(20.0));
        // Pacing fired at least a handful of times (proving it is not stuck)
        // and stayed within a generous multiple of the nominal target*duration
        // tick count (proving it is not spinning unboundedly), tolerant of a
        // slow or contended test environment in either direction.
        assert!(profile.scheduled_ticks > 0, "{}", profile.scheduled_ticks);
        assert!(
            profile.scheduled_ticks <= 200,
            "{}",
            profile.scheduled_ticks
        );
        assert!(summary.total_requests > 0);
        assert!(summary.total_requests <= profile.scheduled_ticks);
        assert_eq!(summary.stages.len(), 1);
        assert_eq!(summary.stages[0].target_rps, Some(20.0));
    }

    #[tokio::test]
    async fn test_arrival_rate_pacing_is_not_bottlenecked_by_per_tick_setup() {
        // Building an HTTP client per pacing tick used to cost several
        // milliseconds each — comparable to or larger than the tick period
        // itself at a moderate target rate — and silently capped the
        // achieved rate well below what was configured. With a shared
        // client, pacing should keep up with a rate the local loopback
        // server can easily sustain.
        let (address, server) = spawn_status_server(200).await;
        let mut config = cancellation_test_config(format!("http://{address}"));
        config.load_profile = Some(LoadProfile::ArrivalRate {
            target_rps: 150.0,
            duration: "1s".to_string(),
            max_concurrency: 200,
        });
        let metrics = Arc::new(MetricsCollector::new());
        let executor = Executor::new(config, Arc::clone(&metrics), Cancellation::new()).unwrap();

        executor.run(0).await.unwrap();
        server.abort();

        let summary = metrics.generate_summary();
        let profile = summary.load_profile.expect("arrival_rate profile summary");
        // Nominal tick count for 150 req/s over 1s is 150; tolerant of a
        // loaded CI environment, but nowhere near the ~40% a per-tick client
        // build used to cap this at.
        assert!(
            profile.scheduled_ticks >= 100,
            "only {} of ~150 ticks fired; pacing is bottlenecked",
            profile.scheduled_ticks
        );
    }

    #[tokio::test]
    async fn test_arrival_rate_tracks_saturation_against_a_slow_target() {
        // Each request takes 200ms to answer but only one may be in flight,
        // so at 50 req/s most pacing ticks cannot start a new request.
        let (address, server) = spawn_delayed_status_server(200, Duration::from_millis(200)).await;
        let mut config = cancellation_test_config(format!("http://{address}"));
        config.load_profile = Some(LoadProfile::ArrivalRate {
            target_rps: 50.0,
            duration: "500ms".to_string(),
            max_concurrency: 1,
        });
        let metrics = Arc::new(MetricsCollector::new());
        let executor = Executor::new(config, Arc::clone(&metrics), Cancellation::new()).unwrap();

        executor.run(0).await.unwrap();
        server.abort();

        let summary = metrics.generate_summary();
        let profile = summary.load_profile.expect("arrival_rate profile summary");
        assert!(profile.saturated_ticks > 0);
        assert!(profile.scheduled_ticks >= profile.saturated_ticks);
    }

    #[tokio::test]
    async fn test_arrival_rate_cancellation_stops_quickly() {
        let (address, server) = spawn_status_server(200).await;
        let mut config = cancellation_test_config(format!("http://{address}"));
        config.load_profile = Some(LoadProfile::ArrivalRate {
            target_rps: 10.0,
            duration: "300s".to_string(),
            max_concurrency: 50,
        });
        let metrics = Arc::new(MetricsCollector::new());
        let cancellation = Cancellation::new();
        let executor = Executor::new(config, Arc::clone(&metrics), cancellation.clone()).unwrap();

        let canceller = cancellation.clone();
        tokio::spawn(async move {
            sleep(Duration::from_millis(100)).await;
            canceller.cancel();
        });

        let started = Instant::now();
        executor.run(0).await.unwrap();
        server.abort();

        assert!(started.elapsed() < Duration::from_secs(30));
    }
}
