use crate::cancel::Cancellation;
use crate::config::Config;
use crate::metrics::{MetricsCollector, RecentActivity, RecentResult};
use crate::redact::Redactor;
use anyhow::Result;
use serde::Serialize;
use serde_json::json;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tracing::{debug, warn};

/// The dashboard page, embedded so the binary serves it without any file or
/// network dependency.
const DASHBOARD_HTML: &str = include_str!("templates/dashboard.html");

/// Placeholder replaced with the configured poll interval.
const REFRESH_PLACEHOLDER: &str = "__REFRESH_MS__";

/// Largest request head accepted, in bytes. The dashboard only answers short
/// GETs, so anything larger is a client bug or an attempt to tie up memory.
const MAX_REQUEST_HEAD_BYTES: usize = 8 * 1024;

/// Lifecycle of the run, as reported by `/healthz`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunStatus {
    Running,
    Completed,
    Cancelled,
}

impl RunStatus {
    fn as_str(self) -> &'static str {
        match self {
            RunStatus::Running => "running",
            RunStatus::Completed => "completed",
            RunStatus::Cancelled => "cancelled",
        }
    }
}

/// Everything a request handler needs; shared by every connection.
#[derive(Debug)]
struct DashboardState {
    metrics: Arc<MetricsCollector>,
    redactor: Redactor,
    cancellation: Cancellation,
    finished: AtomicBool,
    refresh_ms: u64,
}

impl DashboardState {
    fn status(&self) -> RunStatus {
        if self.cancellation.is_cancelled() {
            RunStatus::Cancelled
        } else if self.finished.load(Ordering::Relaxed) {
            RunStatus::Completed
        } else {
            RunStatus::Running
        }
    }

    /// Recent activity with every free-form string redacted.
    ///
    /// Rows never carry headers, cookies or payloads; the error message is the
    /// only place a secret can surface, and it goes through the redactor here.
    fn redacted_activity(&self) -> RecentActivity {
        let mut activity = self.metrics.recent_activity();
        redact_rows(&self.redactor, &mut activity.results);
        redact_rows(&self.redactor, &mut activity.failures);
        activity
    }
}

fn redact_rows(redactor: &Redactor, rows: &mut [RecentResult]) {
    for row in rows {
        row.error = redactor.redact_optional(row.error.as_deref());
    }
}

/// Handle for the optional live dashboard HTTP server.
pub struct LiveDashboard {
    local_addr: SocketAddr,
    state: Arc<DashboardState>,
    shutdown: Option<oneshot::Sender<()>>,
    task: JoinHandle<Result<()>>,
}

impl LiveDashboard {
    /// Start the dashboard only when the configuration asks for it.
    ///
    /// Returning `None` is the whole point: with no `live_dashboard` section no
    /// socket is ever created, so an ordinary run exposes nothing.
    pub async fn maybe_start(
        config: &Config,
        metrics: Arc<MetricsCollector>,
        cancellation: Cancellation,
    ) -> Result<Option<Self>> {
        let Some(dashboard) = &config.live_dashboard else {
            return Ok(None);
        };

        let address = dashboard.parse_bind()?;
        if !address.ip().is_loopback() {
            warn!(
                "Live dashboard is bound to {address}, which is reachable beyond this host; \
                 it has no authentication, so restrict access at the network level"
            );
        }

        let server = Self::start(
            address,
            metrics,
            Redactor::from_config(config),
            cancellation,
            dashboard.refresh_ms,
        )
        .await?;

        Ok(Some(server))
    }

    /// Bind and serve. Port 0 lets the operating system choose, which is what
    /// the tests use.
    pub async fn start(
        address: SocketAddr,
        metrics: Arc<MetricsCollector>,
        redactor: Redactor,
        cancellation: Cancellation,
        refresh_ms: u64,
    ) -> Result<Self> {
        let listener = TcpListener::bind(address).await?;
        let local_addr = listener.local_addr()?;
        let (shutdown_tx, mut shutdown_rx) = oneshot::channel();

        let state = Arc::new(DashboardState {
            metrics,
            redactor,
            cancellation,
            finished: AtomicBool::new(false),
            refresh_ms,
        });
        let server_state = Arc::clone(&state);

        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = &mut shutdown_rx => break,
                    accepted = listener.accept() => {
                        let (stream, _) = accepted?;
                        let state = Arc::clone(&server_state);
                        tokio::spawn(async move {
                            if let Err(error) = serve_connection(stream, state).await {
                                debug!("Live dashboard connection failed: {error}");
                            }
                        });
                    }
                }
            }
            Ok(())
        });

        Ok(Self {
            local_addr,
            state,
            shutdown: Some(shutdown_tx),
            task,
        })
    }

    /// The bound address, including the assigned port when started on port 0.
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Browser-friendly URL for the dashboard.
    pub fn url(&self) -> String {
        let address = self.local_addr;
        if address.ip().is_unspecified() {
            format!("http://localhost:{}/", address.port())
        } else {
            format!("http://{address}/")
        }
    }

    /// Record that the load test has finished, so `/healthz` stops reporting a
    /// running test even before the socket closes.
    pub fn mark_finished(&self) {
        self.state.finished.store(true, Ordering::Relaxed);
    }

    /// Stop accepting connections and wait for the listener task to finish.
    pub async fn shutdown(mut self) -> Result<()> {
        self.mark_finished();
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        self.task.await??;
        Ok(())
    }
}

/// One HTTP response, always fully buffered: the payloads are small and bounded.
struct Response {
    status: &'static str,
    content_type: &'static str,
    body: String,
}

impl Response {
    fn html(body: String) -> Self {
        Self {
            status: "200 OK",
            content_type: "text/html; charset=utf-8",
            body,
        }
    }

    fn json(value: impl Serialize) -> Self {
        match serde_json::to_string(&value) {
            Ok(body) => Self {
                status: "200 OK",
                content_type: "application/json",
                body,
            },
            Err(error) => Self {
                status: "500 Internal Server Error",
                content_type: "application/json",
                body: json!({ "error": error.to_string() }).to_string(),
            },
        }
    }

    fn error(status: &'static str, message: &str) -> Self {
        Self {
            status,
            content_type: "application/json",
            body: json!({ "error": message }).to_string(),
        }
    }

    fn render(&self) -> String {
        format!(
            "HTTP/1.1 {status}\r\n\
             Content-Type: {content_type}\r\n\
             Content-Length: {length}\r\n\
             Cache-Control: no-store\r\n\
             X-Content-Type-Options: nosniff\r\n\
             Referrer-Policy: no-referrer\r\n\
             Content-Security-Policy: default-src 'none'; connect-src 'self'; \
             script-src 'unsafe-inline'; style-src 'unsafe-inline'\r\n\
             Connection: close\r\n\r\n\
             {body}",
            status = self.status,
            content_type = self.content_type,
            length = self.body.len(),
            body = self.body
        )
    }
}

async fn serve_connection(mut stream: TcpStream, state: Arc<DashboardState>) -> Result<()> {
    let response = match read_request_head(&mut stream).await? {
        Some(head) => route(&head, &state),
        None => Response::error(
            "431 Request Header Fields Too Large",
            "request head too large",
        ),
    };

    stream.write_all(response.render().as_bytes()).await?;
    stream.shutdown().await?;
    Ok(())
}

/// Read until the end of the request head, giving up past the size cap.
async fn read_request_head(stream: &mut TcpStream) -> Result<Option<String>> {
    let mut head = Vec::new();
    let mut chunk = [0_u8; 1024];

    loop {
        let bytes_read = stream.read(&mut chunk).await?;
        if bytes_read == 0 {
            break;
        }
        head.extend_from_slice(&chunk[..bytes_read]);
        if head.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
        if head.len() > MAX_REQUEST_HEAD_BYTES {
            return Ok(None);
        }
    }

    Ok(Some(String::from_utf8_lossy(&head).into_owned()))
}

fn route(head: &str, state: &DashboardState) -> Response {
    let Some(request_line) = head.lines().next() else {
        return Response::error("400 Bad Request", "empty request");
    };
    let mut parts = request_line.split_whitespace();
    let (Some(method), Some(target)) = (parts.next(), parts.next()) else {
        return Response::error("400 Bad Request", "malformed request line");
    };

    // The dashboard is read-only; nothing here should ever change the run.
    if method != "GET" {
        return Response::error("405 Method Not Allowed", "only GET is supported");
    }

    let path = target.split(['?', '#']).next().unwrap_or("/");
    match path {
        "/" | "/index.html" => Response::html(render_page(state.refresh_ms)),
        "/api/summary" => {
            let snapshot = state.metrics.snapshot();
            let mut value = serde_json::to_value(&snapshot).unwrap_or_else(|_| json!({}));
            if let Some(object) = value.as_object_mut() {
                object.insert("status".to_string(), json!(state.status().as_str()));
                object.insert("refresh_ms".to_string(), json!(state.refresh_ms));
            }
            Response::json(value)
        }
        "/api/recent-results" => Response::json(state.redacted_activity()),
        "/healthz" => {
            let snapshot = state.metrics.snapshot();
            Response::json(json!({
                "status": state.status().as_str(),
                "uptime_secs": snapshot.elapsed_secs,
                "total_requests": snapshot.total_requests,
            }))
        }
        _ => Response::error("404 Not Found", "unknown endpoint"),
    }
}

fn render_page(refresh_ms: u64) -> String {
    DASHBOARD_HTML.replace(REFRESH_PLACEHOLDER, &refresh_ms.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{LiveDashboardConfig, OutputConfig};
    use crate::metrics::RequestResult;
    use chrono::Utc;
    use std::collections::HashMap;

    fn config(live_dashboard: Option<LiveDashboardConfig>) -> Config {
        Config {
            target: Some("https://api.example.com".to_string()),
            method: Some("GET".to_string()),
            headers: HashMap::from([(
                "Authorization".to_string(),
                "Bearer configured-token-value".to_string(),
            )]),
            body: None,
            multipart: None,
            scenarios: vec![],
            concurrency: 1,
            duration: "1s".to_string(),
            timeout: "1s".to_string(),
            ramp_up: None,
            think_time: None,
            retry_count: 0,
            retry_delay: None,
            retry_on_status: vec![],
            assertions: None,
            prometheus_port: None,
            live_dashboard,
            mode: "async".to_string(),
            output: OutputConfig {
                json: "output.json".to_string(),
                html: "output.html".to_string(),
                csv: None,
                max_results: 0,
            },
        }
    }

    /// Bind on an ephemeral loopback port so tests never collide.
    fn ephemeral_dashboard() -> LiveDashboardConfig {
        LiveDashboardConfig {
            bind: "127.0.0.1:0".to_string(),
            refresh_ms: 250,
            redact: vec!["configured-extra-secret".to_string()],
        }
    }

    fn record(metrics: &MetricsCollector, status: u16, error: Option<&str>) {
        metrics.record(RequestResult {
            scenario_name: Some("login".to_string()),
            latency_ms: 42,
            status_code: status,
            error: error.map(ToString::to_string),
            request_start_timestamp: Utc::now(),
            request_end_timestamp: Utc::now(),
        });
    }

    async fn get(address: SocketAddr, path: &str) -> (String, String) {
        let mut stream = TcpStream::connect(address).await.unwrap();
        stream
            .write_all(format!("GET {path} HTTP/1.1\r\nHost: localhost\r\n\r\n").as_bytes())
            .await
            .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).await.unwrap();
        let (head, body) = response.split_once("\r\n\r\n").unwrap();
        (head.to_string(), body.to_string())
    }

    async fn start_dashboard(
        config: &Config,
        metrics: Arc<MetricsCollector>,
        cancellation: Cancellation,
    ) -> LiveDashboard {
        LiveDashboard::maybe_start(config, metrics, cancellation)
            .await
            .unwrap()
            .expect("dashboard configured")
    }

    #[tokio::test]
    async fn test_no_port_is_opened_without_configuration() {
        let started = LiveDashboard::maybe_start(
            &config(None),
            Arc::new(MetricsCollector::new()),
            Cancellation::new(),
        )
        .await
        .unwrap();

        assert!(started.is_none(), "an unconfigured run must not listen");
    }

    #[tokio::test]
    async fn test_dashboard_serves_page_and_live_metrics() {
        let metrics = Arc::new(MetricsCollector::new());
        record(&metrics, 200, None);
        record(&metrics, 500, Some("HTTP 500"));

        let dashboard = start_dashboard(
            &config(Some(ephemeral_dashboard())),
            Arc::clone(&metrics),
            Cancellation::new(),
        )
        .await;
        let address = dashboard.local_addr();

        let (head, body) = get(address, "/").await;
        assert!(head.starts_with("HTTP/1.1 200 OK"), "{head}");
        assert!(head.contains("text/html"), "{head}");
        assert!(body.contains("Flux live dashboard"), "{body}");
        // The poll interval is baked into the page, not left as a placeholder.
        assert!(body.contains("const REFRESH_MS = 250;"), "{body}");
        assert!(!body.contains(REFRESH_PLACEHOLDER));

        let (head, body) = get(address, "/api/summary").await;
        assert!(head.contains("application/json"), "{head}");
        let summary: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(summary["total_requests"], 2);
        assert_eq!(summary["failed_requests"], 1);
        assert_eq!(summary["status"], "running");
        assert_eq!(summary["refresh_ms"], 250);
        assert_eq!(summary["status_codes"]["200"], 1);
        assert_eq!(summary["status_codes"]["500"], 1);
        assert_eq!(summary["per_scenario"]["login"]["total_requests"], 2);
        assert!(!summary["timeline"].as_array().unwrap().is_empty());

        // New requests show up in the next poll: the dashboard is live.
        record(&metrics, 200, None);
        let (_, body) = get(address, "/api/summary").await;
        let summary: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(summary["total_requests"], 3);

        let (_, body) = get(address, "/api/recent-results").await;
        let recent: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(recent["results"].as_array().unwrap().len(), 3);
        assert_eq!(recent["failures"].as_array().unwrap().len(), 1);
        assert_eq!(recent["failures"][0]["error"], "HTTP 500");
        assert_eq!(recent["failures"][0]["scenario_name"], "login");

        dashboard.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn test_dashboard_redacts_secrets_from_failures() {
        let metrics = Arc::new(MetricsCollector::new());
        record(
            &metrics,
            0,
            Some(
                "error sending request for url \
                 (https://user:pass@api.example.com/v1?token=runtime-token-value): \
                 header Bearer configured-token-value, seen configured-extra-secret",
            ),
        );

        let dashboard = start_dashboard(
            &config(Some(ephemeral_dashboard())),
            Arc::clone(&metrics),
            Cancellation::new(),
        )
        .await;

        let (_, body) = get(dashboard.local_addr(), "/api/recent-results").await;

        // Configured secrets, runtime tokens and URL credentials are all gone,
        // while the rest of the message stays useful.
        assert!(!body.contains("configured-token-value"), "{body}");
        assert!(!body.contains("configured-extra-secret"), "{body}");
        assert!(!body.contains("runtime-token-value"), "{body}");
        assert!(!body.contains("user:pass"), "{body}");
        assert!(body.contains("api.example.com"), "{body}");

        dashboard.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn test_health_reports_lifecycle_and_unknown_paths_are_rejected() {
        let metrics = Arc::new(MetricsCollector::new());
        record(&metrics, 200, None);
        let cancellation = Cancellation::new();
        let dashboard = start_dashboard(
            &config(Some(ephemeral_dashboard())),
            Arc::clone(&metrics),
            cancellation.clone(),
        )
        .await;
        let address = dashboard.local_addr();

        let (head, body) = get(address, "/healthz").await;
        assert!(head.starts_with("HTTP/1.1 200 OK"), "{head}");
        let health: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(health["status"], "running");
        assert_eq!(health["total_requests"], 1);

        // Cancelling the run is visible before the socket closes.
        cancellation.cancel();
        let (_, body) = get(address, "/healthz").await;
        let health: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(health["status"], "cancelled");

        let (head, _) = get(address, "/does-not-exist").await;
        assert!(head.starts_with("HTTP/1.1 404 Not Found"), "{head}");

        let mut stream = TcpStream::connect(address).await.unwrap();
        stream
            .write_all(b"POST /api/summary HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).await.unwrap();
        assert!(
            response.starts_with("HTTP/1.1 405 Method Not Allowed"),
            "{response}"
        );

        dashboard.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn test_finished_run_is_reported_then_the_port_closes() {
        let metrics = Arc::new(MetricsCollector::new());
        let dashboard = start_dashboard(
            &config(Some(ephemeral_dashboard())),
            metrics,
            Cancellation::new(),
        )
        .await;
        let address = dashboard.local_addr();

        dashboard.mark_finished();
        let (_, body) = get(address, "/healthz").await;
        let health: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(health["status"], "completed");

        dashboard.shutdown().await.unwrap();

        // Shutdown releases the port instead of leaving a listener behind.
        assert!(TcpStream::connect(address).await.is_err());
    }

    #[tokio::test]
    async fn test_query_strings_and_trailing_paths_resolve_to_the_same_endpoint() {
        let dashboard = start_dashboard(
            &config(Some(ephemeral_dashboard())),
            Arc::new(MetricsCollector::new()),
            Cancellation::new(),
        )
        .await;

        let (head, body) = get(dashboard.local_addr(), "/api/summary?since=12345").await;
        assert!(head.starts_with("HTTP/1.1 200 OK"), "{head}");
        assert!(body.contains("\"total_requests\":0"), "{body}");

        dashboard.shutdown().await.unwrap();
    }
}
