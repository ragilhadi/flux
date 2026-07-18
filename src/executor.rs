use crate::client::HttpClient;
use crate::config::{Config, Scenario};
use crate::metrics::{MetricsCollector, RequestResult};
use anyhow::Result;
use chrono::Utc;
use jsonpath_rust::JsonPath;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;
use tokio::time::{sleep, Duration};
use tracing::{debug, error, warn};

/// Executor for running load tests
pub struct Executor {
    config: Config,
    client: HttpClient,
    metrics: Arc<MetricsCollector>,
}

impl Executor {
    /// Create a new executor
    pub fn new(config: Config, metrics: Arc<MetricsCollector>) -> Result<Self> {
        let client = HttpClient::new(config.parse_timeout()?)?;
        Ok(Self {
            config,
            client,
            metrics,
        })
    }

    /// Run the load test
    pub async fn run(&self, duration_secs: u64) -> Result<()> {
        let start = Instant::now();
        let duration = Duration::from_secs(duration_secs);
        let ramp_up = self.config.parse_ramp_up()?;
        self.run_workers(start, duration, ramp_up).await
    }

    /// Start workers, optionally spreading their startup across a ramp-up period.
    async fn run_workers(
        &self,
        start: Instant,
        duration: Duration,
        ramp_up: Option<Duration>,
    ) -> Result<()> {
        let mut handles = vec![];
        let start_delay =
            worker_start_delay(self.config.concurrency, ramp_up, self.config.mode == "sync");

        for worker_id in 0..self.config.concurrency {
            let executor = self.clone_for_worker();
            let start_clone = start;
            let duration_clone = duration;

            let handle = tokio::spawn(async move {
                executor
                    .worker_loop(worker_id, start_clone, duration_clone)
                    .await;
            });

            handles.push(handle);
            if worker_id + 1 < self.config.concurrency && !start_delay.is_zero() {
                sleep(start_delay).await;
            }
        }

        // Wait for all workers to complete
        for handle in handles {
            let _ = handle.await;
        }

        Ok(())
    }

    /// Worker loop that executes requests
    async fn worker_loop(&self, worker_id: usize, start: Instant, duration: Duration) {
        debug!("Worker {} started", worker_id);

        while start.elapsed() < duration {
            if self.config.is_simple_mode() {
                self.execute_simple_request().await;
                if let Some(think_time) = self
                    .config
                    .parse_think_time()
                    .expect("Validated think_time became invalid")
                {
                    sleep(think_time).await;
                }
            } else {
                self.execute_scenarios().await;
            }

            // Small delay in sync mode
            if self.config.mode == "sync" {
                sleep(Duration::from_millis(10)).await;
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
                && should_retry_response(&result, &self.config.retry_on_status)
            {
                attempt += 1;
                debug!(
                    "Retrying request after attempt {} of {}",
                    attempt,
                    self.config.retry_count.saturating_add(1)
                );
                if !retry_delay.is_zero() {
                    sleep(retry_delay).await;
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

        self.metrics.record(request_result);
    }

    /// Execute all scenarios in sequence
    async fn execute_scenarios(&self) {
        let mut variables: HashMap<String, String> = HashMap::new();
        let mut executed: HashSet<String> = HashSet::new();

        for scenario in &self.config.scenarios {
            // Check dependencies
            if let Some(ref depends_on) = scenario.depends_on {
                if !Self::has_executed_scenario(depends_on, &executed) {
                    warn!(
                        "Skipping scenario '{}' - dependency '{}' not met",
                        scenario.name, depends_on
                    );
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

                if attempt < retry_count && should_retry_response(&result, retry_statuses) {
                    attempt += 1;
                    debug!(
                        "Retrying scenario '{}' after attempt {} of {}",
                        scenario.name,
                        attempt,
                        retry_count.saturating_add(1)
                    );
                    if !retry_delay.is_zero() {
                        sleep(retry_delay).await;
                    }
                    continue;
                }

                break (result, start_time, end_time, latency);
            };

            match result {
                Ok(response) => {
                    let status = response.status();
                    let status_code = status.as_u16();

                    let error = if !status.is_success() {
                        Some(format!("HTTP {}", status_code))
                    } else {
                        None
                    };

                    // Extract variables if needed (only on success)
                    if error.is_none() && !scenario.extract.is_empty() {
                        if let Ok(body) = response.text().await {
                            self.extract_variables(&body, scenario, &mut variables);
                        }
                    }

                    // Only mark as executed on a successful response
                    if error.is_none() {
                        executed.insert(scenario.name.clone());
                    }

                    let request_result = RequestResult {
                        scenario_name: Some(scenario.name.clone()),
                        latency_ms: latency,
                        status_code,
                        error,
                        request_start_timestamp: start_time,
                        request_end_timestamp: end_time,
                    };

                    self.metrics.record(request_result);
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

                    self.metrics.record(request_result);
                }
            }

            if let Some(think_time) = self
                .config
                .parse_think_time_for(scenario)
                .expect("Validated scenario think_time became invalid")
            {
                sleep(think_time).await;
            }
        }
    }

    /// Extract variables from response body using JSONPath
    fn extract_variables(
        &self,
        body: &str,
        scenario: &Scenario,
        variables: &mut HashMap<String, String>,
    ) {
        for (var_name, json_path) in &scenario.extract {
            match serde_json::from_str::<serde_json::Value>(body) {
                Ok(json) => match json.query(json_path) {
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
                },
                Err(e) => {
                    warn!("Failed to parse JSON response: {}", e);
                }
            }
        }
    }

    /// Check if a scenario has been executed
    fn has_executed_scenario(scenario_name: &str, executed: &HashSet<String>) -> bool {
        executed.contains(scenario_name)
    }

    /// Clone executor for worker
    fn clone_for_worker(&self) -> Self {
        Self {
            config: self.config.clone(),
            client: HttpClient::new(self.config.parse_timeout().expect("Invalid timeout"))
                .expect("Failed to create client"),
            metrics: Arc::clone(&self.metrics),
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::OutputConfig;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

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
            think_time: None,
            retry_count: 0,
            retry_delay: None,
            retry_on_status: vec![],
            mode: "async".to_string(),
            output: OutputConfig {
                json: "/app/results/output.json".to_string(),
                html: "/app/results/output.html".to_string(),
            },
        };

        let metrics = Arc::new(MetricsCollector::new());
        let executor = Executor::new(config, metrics);

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
            think_time: None,
            retry_count: 1,
            retry_delay: Some("0s".to_string()),
            retry_on_status: vec![503],
            mode: "async".to_string(),
            output: OutputConfig {
                json: "output.json".to_string(),
                html: "output.html".to_string(),
            },
        };
        let metrics = Arc::new(MetricsCollector::new());
        let executor = Executor::new(config, Arc::clone(&metrics)).unwrap();

        executor.execute_simple_request().await;
        server.await.unwrap();

        let results = metrics.get_results();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].status_code, 200);
        assert!(results[0].error.is_none());
    }
}
