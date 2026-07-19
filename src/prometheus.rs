use crate::metrics::MetricsCollector;
use anyhow::Result;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

/// Handle for the optional live Prometheus HTTP endpoint.
pub struct PrometheusServer {
    local_addr: SocketAddr,
    shutdown: Option<oneshot::Sender<()>>,
    task: JoinHandle<Result<()>>,
}

impl PrometheusServer {
    /// Start the endpoint on all interfaces. Port 0 is supported for tests.
    pub async fn start(port: u16, metrics: Arc<MetricsCollector>) -> Result<Self> {
        let listener = TcpListener::bind(("0.0.0.0", port)).await?;
        let local_addr = listener.local_addr()?;
        let (shutdown_tx, mut shutdown_rx) = oneshot::channel();

        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = &mut shutdown_rx => break,
                    accepted = listener.accept() => {
                        let (stream, _) = accepted?;
                        let metrics = Arc::clone(&metrics);
                        tokio::spawn(async move {
                            if let Err(error) = serve_connection(stream, metrics).await {
                                tracing::debug!("Prometheus connection failed: {error}");
                            }
                        });
                    }
                }
            }
            Ok(())
        });

        Ok(Self {
            local_addr,
            shutdown: Some(shutdown_tx),
            task,
        })
    }

    /// Return the bound address, including the assigned port when started with port 0.
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Stop accepting connections and wait for the listener task to finish.
    pub async fn shutdown(mut self) -> Result<()> {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        self.task.await??;
        Ok(())
    }
}

async fn serve_connection(mut stream: TcpStream, metrics: Arc<MetricsCollector>) -> Result<()> {
    let mut request = [0_u8; 4096];
    let bytes_read = stream.read(&mut request).await?;
    let request = String::from_utf8_lossy(&request[..bytes_read]);
    let is_metrics_request = request
        .lines()
        .next()
        .is_some_and(|line| line.starts_with("GET /metrics "));

    let (status, content_type, body) = if is_metrics_request {
        (
            "200 OK",
            "text/plain; version=0.0.4; charset=utf-8",
            metrics.render_prometheus(),
        )
    } else {
        (
            "404 Not Found",
            "text/plain; charset=utf-8",
            "Not Found\n".to_string(),
        )
    };
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await?;
    stream.shutdown().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::RequestResult;
    use chrono::Utc;

    #[tokio::test]
    async fn test_metrics_endpoint_and_clean_shutdown() {
        let metrics = Arc::new(MetricsCollector::new());
        metrics.record(RequestResult {
            scenario_name: None,
            latency_ms: 25,
            status_code: 200,
            error: None,
            request_start_timestamp: Utc::now(),
            request_end_timestamp: Utc::now(),
        });
        let server = PrometheusServer::start(0, metrics).await.unwrap();
        let address = SocketAddr::from(([127, 0, 0, 1], server.local_addr().port()));

        let mut stream = TcpStream::connect(address).await.unwrap();
        stream
            .write_all(b"GET /metrics HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).await.unwrap();

        assert!(response.starts_with("HTTP/1.1 200 OK"));
        assert!(response.contains("flux_requests_total{status=\"success\"} 1"));
        assert!(response.contains("flux_request_duration_ms{quantile=\"0.95\"} 25"));

        server.shutdown().await.unwrap();
        assert!(TcpStream::connect(address).await.is_err());
    }
}
