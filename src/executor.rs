use crate::client::HttpClient;
use crate::config::{Config, Scenario};
use crate::metrics::{MetricsCollector, RequestResult};
use anyhow::Result;
use chrono::Utc;
use jsonpath_rust::JsonPathFinder;
use std::collections::HashMap;
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
        let client = HttpClient::new()?;
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

        if self.config.mode == "async" {
            self.run_async(start, duration).await?;
        } else {
            self.run_sync(start, duration).await?;
        }

        Ok(())
    }

    /// Run in async mode
    async fn run_async(&self, start: Instant, duration: Duration) -> Result<()> {
        let mut handles = vec![];

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
        }

        // Wait for all workers to complete
        for handle in handles {
            let _ = handle.await;
        }

        Ok(())
    }

    /// Run in sync mode
    async fn run_sync(&self, start: Instant, duration: Duration) -> Result<()> {
        for worker_id in 0..self.config.concurrency {
            let executor = self.clone_for_worker();
            let start_clone = start;
            let duration_clone = duration;

            tokio::spawn(async move {
                executor
                    .worker_loop(worker_id, start_clone, duration_clone)
                    .await;
            });

            // Small delay between workers in sync mode
            sleep(Duration::from_millis(10)).await;
        }

        // Wait for duration
        sleep(duration).await;

        Ok(())
    }

    /// Worker loop that executes requests
    async fn worker_loop(&self, worker_id: usize, start: Instant, duration: Duration) {
        debug!("Worker {} started", worker_id);

        while start.elapsed() < duration {
            if self.config.is_simple_mode() {
                self.execute_simple_request().await;
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
        let start_time = Utc::now();
        let request_start = Instant::now();

        let result = self
            .client
            .execute_simple(
                self.config.target.as_ref().unwrap(),
                self.config.method.as_ref().unwrap_or(&"GET".to_string()),
                &self.config.headers,
                self.config.body.as_deref(),
                self.config.multipart.as_ref(),
            )
            .await;

        let latency = request_start.elapsed().as_millis() as u64;
        let end_time = Utc::now();

        let request_result = match result {
            Ok(response) => RequestResult {
                scenario_name: None,
                latency_ms: latency,
                status_code: response.status().as_u16(),
                error: None,
                request_start_timestamp: start_time,
                request_end_timestamp: end_time,
            },
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

        for scenario in &self.config.scenarios {
            // Check dependencies
            if let Some(ref depends_on) = scenario.depends_on {
                if !self.has_executed_scenario(depends_on, &variables) {
                    warn!(
                        "Skipping scenario '{}' - dependency '{}' not met",
                        scenario.name, depends_on
                    );
                    continue;
                }
            }

            let start_time = Utc::now();
            let request_start = Instant::now();

            let result = self
                .client
                .execute_scenario(self.config.target.as_deref(), scenario, &variables)
                .await;

            let latency = request_start.elapsed().as_millis() as u64;
            let end_time = Utc::now();

            match result {
                Ok(response) => {
                    let status = response.status().as_u16();

                    // Extract variables if needed
                    if !scenario.extract.is_empty() {
                        if let Ok(body) = response.text().await {
                            self.extract_variables(&body, scenario, &mut variables);
                        }
                    }

                    let request_result = RequestResult {
                        scenario_name: Some(scenario.name.clone()),
                        latency_ms: latency,
                        status_code: status,
                        error: None,
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
                Ok(_json) => {
                    match JsonPathFinder::from_str(body, json_path) {
                        Ok(finder) => {
                            let result = finder.find();
                            if let serde_json::Value::Array(arr) = result {
                                if let Some(value) = arr.first() {
                                    let extracted = match value {
                                        serde_json::Value::String(s) => s.clone(),
                                        serde_json::Value::Number(n) => n.to_string(),
                                        serde_json::Value::Bool(b) => b.to_string(),
                                        _ => value.to_string(),
                                    };

                                    debug!("Extracted variable '{}' = '{}'", var_name, extracted);
                                    variables.insert(var_name.clone(), extracted);
                                }
                            } else if result != serde_json::Value::Null {
                                // Single value result
                                let extracted = match result {
                                    serde_json::Value::String(s) => s,
                                    serde_json::Value::Number(n) => n.to_string(),
                                    serde_json::Value::Bool(b) => b.to_string(),
                                    _ => result.to_string(),
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
                Err(e) => {
                    warn!("Failed to parse JSON response: {}", e);
                }
            }
        }
    }

    /// Check if a scenario has been executed (simple check via variables)
    fn has_executed_scenario(
        &self,
        _scenario_name: &str,
        variables: &HashMap<String, String>,
    ) -> bool {
        // Simple heuristic: if we have variables, assume dependencies are met
        // In a more sophisticated implementation, we'd track executed scenarios
        !variables.is_empty()
    }

    /// Clone executor for worker
    fn clone_for_worker(&self) -> Self {
        Self {
            config: self.config.clone(),
            client: HttpClient::new().expect("Failed to create client"),
            metrics: Arc::clone(&self.metrics),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::OutputConfig;

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
}
