use crate::config::{MultipartPart, Scenario};
use anyhow::Result;
use reqwest::{Client, Method, Response};
use std::collections::HashMap;
use std::path::Path;
use std::str::FromStr;
use std::time::Duration;

/// HTTP client wrapper for making requests
pub struct HttpClient {
    client: Client,
}

impl HttpClient {
    /// Create a new HTTP client
    pub fn new(timeout: Duration) -> Result<Self> {
        let client = Client::builder()
            .timeout(timeout)
            .pool_max_idle_per_host(100)
            .build()?;

        Ok(Self { client })
    }

    /// Execute a simple request
    pub async fn execute_simple(
        &self,
        url: &str,
        method: &str,
        headers: &HashMap<String, String>,
        body: Option<&str>,
        multipart: Option<&Vec<MultipartPart>>,
    ) -> Result<Response> {
        let method = Method::from_str(method)?;
        let mut request = self.client.request(method, url);

        // Add headers
        for (key, value) in headers {
            request = request.header(key, value);
        }

        // Handle multipart or body
        if let Some(parts) = multipart {
            request = self.build_multipart_request(request, parts).await?;
        } else if let Some(body_content) = body {
            request = request.body(body_content.to_string());
        }

        let response = request.send().await?;
        Ok(response)
    }

    /// Execute a scenario step
    pub async fn execute_scenario(
        &self,
        base_url: Option<&str>,
        scenario: &Scenario,
        variables: &HashMap<String, String>,
    ) -> Result<Response> {
        // Build full URL with variable substitution
        let raw_url = self.substitute_variables(&scenario.url, variables);
        let url = if raw_url.starts_with("http://") || raw_url.starts_with("https://") {
            raw_url
        } else if let Some(base) = base_url {
            format!("{}{}", base.trim_end_matches('/'), raw_url)
        } else {
            raw_url
        };

        let method = Method::from_str(&scenario.method)?;
        let mut request = self.client.request(method, &url);

        // Add headers with variable substitution
        for (key, value) in &scenario.headers {
            let substituted_value = self.substitute_variables(value, variables);
            request = request.header(key, substituted_value);
        }

        // Handle multipart or body
        if let Some(parts) = &scenario.multipart {
            let rendered = self.render_multipart_parts(parts, variables, &scenario.name)?;
            request = self.build_multipart_request(request, &rendered).await?;
        } else if let Some(body_content) = &scenario.body {
            let substituted_body = self.substitute_variables(body_content, variables);
            request = request.body(substituted_body);
        }

        let response = request.send().await?;
        Ok(response)
    }

    /// Apply scenario variables to every field of each multipart part.
    ///
    /// Multipart parts are rendered before the form is built so extracted
    /// variables reach field names, field values and file paths, exactly as
    /// they do for the URL, headers and a raw body. A placeholder that no
    /// variable resolves is a hard error: sending literal `{{ name }}` to the
    /// target would be an unintended request.
    fn render_multipart_parts(
        &self,
        parts: &[MultipartPart],
        variables: &HashMap<String, String>,
        scenario_name: &str,
    ) -> Result<Vec<MultipartPart>> {
        parts
            .iter()
            .map(|part| {
                let rendered = MultipartPart {
                    part_type: self.substitute_variables(&part.part_type, variables),
                    name: self.substitute_variables(&part.name, variables),
                    path: part
                        .path
                        .as_deref()
                        .map(|path| self.substitute_variables(path, variables)),
                    value: part
                        .value
                        .as_deref()
                        .map(|value| self.substitute_variables(value, variables)),
                };

                let fields = [
                    ("type", Some(&rendered.part_type)),
                    ("name", Some(&rendered.name)),
                    ("path", rendered.path.as_ref()),
                    ("value", rendered.value.as_ref()),
                ];
                for (field, content) in fields {
                    let Some(content) = content else { continue };
                    if let Some(placeholder) = unresolved_placeholder(content) {
                        anyhow::bail!(
                            "Unresolved variable '{placeholder}' in multipart '{field}' of \
                             part '{}' in scenario '{scenario_name}'; no earlier step \
                             extracted it",
                            part.name
                        );
                    }
                }

                Ok(rendered)
            })
            .collect()
    }

    /// Build multipart form request
    async fn build_multipart_request(
        &self,
        request: reqwest::RequestBuilder,
        parts: &[MultipartPart],
    ) -> Result<reqwest::RequestBuilder> {
        let mut form = reqwest::multipart::Form::new();

        for part in parts {
            match part.part_type.as_str() {
                "file" => {
                    if let Some(ref path) = part.path {
                        let file_path = Path::new(path);

                        if !file_path.exists() {
                            anyhow::bail!("File not found: {}", path);
                        }

                        let file_name = file_path
                            .file_name()
                            .and_then(|n| n.to_str())
                            .unwrap_or("file");

                        let file_content = tokio::fs::read(file_path).await?;

                        let multipart_part = reqwest::multipart::Part::bytes(file_content)
                            .file_name(file_name.to_string());

                        form = form.part(part.name.clone(), multipart_part);
                    }
                }
                "field" => {
                    if let Some(ref value) = part.value {
                        form = form.text(part.name.clone(), value.clone());
                    }
                }
                _ => {
                    anyhow::bail!("Unknown multipart type: {}", part.part_type);
                }
            }
        }

        Ok(request.multipart(form))
    }

    /// Substitute variables in a string using {{ variable }} syntax
    fn substitute_variables(&self, template: &str, variables: &HashMap<String, String>) -> String {
        let mut result = template.to_string();

        for (key, value) in variables {
            let placeholder = format!("{{{{ {} }}}}", key);
            result = result.replace(&placeholder, value);
        }

        result
    }
}

/// Return the name inside the first `{{ ... }}` placeholder that survived
/// substitution, if any.
fn unresolved_placeholder(value: &str) -> Option<String> {
    let start = value.find("{{")?;
    let rest = &value[start + 2..];
    let end = rest.find("}}")?;
    Some(rest[..end].trim().to_string())
}

impl Default for HttpClient {
    fn default() -> Self {
        Self::new(Duration::from_secs(30)).expect("Failed to create HTTP client")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Scenario;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn multipart_scenario(parts: Vec<MultipartPart>) -> Scenario {
        Scenario {
            name: "upload".to_string(),
            method: "POST".to_string(),
            url: "/uploads".to_string(),
            headers: HashMap::new(),
            body: None,
            multipart: Some(parts),
            extract: HashMap::new(),
            depends_on: None,
            think_time: None,
            retry_count: None,
            retry_delay: None,
            retry_on_status: None,
            assertions: None,
        }
    }

    /// Accept one request, return everything that was sent, then answer 200.
    async fn capture_one_request(listener: TcpListener) -> String {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut received = Vec::new();
        let mut chunk = [0_u8; 4096];

        loop {
            match tokio::time::timeout(Duration::from_millis(300), socket.read(&mut chunk)).await {
                Ok(Ok(0)) | Err(_) => break,
                Ok(Ok(read)) => received.extend_from_slice(&chunk[..read]),
                Ok(Err(_)) => break,
            }
        }

        let _ = socket
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .await;
        String::from_utf8_lossy(&received).into_owned()
    }

    fn temp_file(name: &str, contents: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(name);
        std::fs::write(&path, contents).unwrap();
        path
    }

    #[tokio::test]
    async fn test_scenario_variables_render_in_multipart() {
        let unique = format!("{}-{}", std::process::id(), line!());
        let file_name = format!("flux-upload-{unique}.txt");
        let file_path = temp_file(&file_name, "uploaded contents");
        let directory = file_path.parent().unwrap().display().to_string();

        let scenario = multipart_scenario(vec![
            MultipartPart {
                part_type: "field".to_string(),
                name: "{{ field_name }}".to_string(),
                path: None,
                value: Some("{{ token }}".to_string()),
            },
            MultipartPart {
                part_type: "file".to_string(),
                name: "attachment".to_string(),
                path: Some(format!("{directory}/{{{{ file_name }}}}")),
                value: None,
            },
        ]);
        let variables = HashMap::from([
            ("field_name".to_string(), "session".to_string()),
            ("token".to_string(), "abc123".to_string()),
            ("file_name".to_string(), file_name.clone()),
        ]);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(capture_one_request(listener));

        let client = HttpClient::default();
        let response = client
            .execute_scenario(Some(&format!("http://{address}")), &scenario, &variables)
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 200);

        let request = server.await.unwrap();
        std::fs::remove_file(&file_path).unwrap();

        assert!(request.contains("name=\"session\""), "{request}");
        assert!(request.contains("abc123"), "{request}");
        assert!(request.contains("name=\"attachment\""), "{request}");
        assert!(
            request.contains(&format!("filename=\"{file_name}\"")),
            "{request}"
        );
        assert!(request.contains("uploaded contents"), "{request}");
        assert!(!request.contains("{{"), "{request}");
    }

    #[tokio::test]
    async fn test_unresolved_multipart_placeholder_is_rejected_before_sending() {
        let scenario = multipart_scenario(vec![MultipartPart {
            part_type: "field".to_string(),
            name: "session".to_string(),
            path: None,
            value: Some("{{ token }}".to_string()),
        }]);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();

        let client = HttpClient::default();
        let error = client
            .execute_scenario(
                Some(&format!("http://{address}")),
                &scenario,
                &HashMap::new(),
            )
            .await
            .unwrap_err()
            .to_string();

        assert!(error.contains("Unresolved variable 'token'"), "{error}");
        assert!(error.contains("scenario 'upload'"), "{error}");
        // Nothing was dispatched, so no connection was ever accepted.
        assert!(
            tokio::time::timeout(Duration::from_millis(100), listener.accept())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn test_missing_multipart_file_still_reports_rendered_path() {
        let scenario = multipart_scenario(vec![MultipartPart {
            part_type: "file".to_string(),
            name: "attachment".to_string(),
            path: Some("/does/not/exist/{{ file_name }}".to_string()),
            value: None,
        }]);
        let variables = HashMap::from([("file_name".to_string(), "missing.txt".to_string())]);

        let client = HttpClient::default();
        let error = client
            .execute_scenario(Some("http://127.0.0.1:1"), &scenario, &variables)
            .await
            .unwrap_err()
            .to_string();

        assert!(
            error.contains("File not found: /does/not/exist/missing.txt"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn test_simple_mode_multipart_is_not_substituted() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(capture_one_request(listener));

        let client = HttpClient::default();
        let parts = vec![MultipartPart {
            part_type: "field".to_string(),
            name: "session".to_string(),
            value: Some("{{ token }}".to_string()),
            path: None,
        }];
        let response = client
            .execute_simple(
                &format!("http://{address}"),
                "POST",
                &HashMap::new(),
                None,
                Some(&parts),
            )
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 200);

        let request = server.await.unwrap();
        // Simple mode has no scenario variables, so values are sent verbatim.
        assert!(request.contains("{{ token }}"), "{request}");
    }

    #[test]
    fn test_unresolved_placeholder_detection() {
        assert_eq!(
            unresolved_placeholder("/data/{{ file_name }}").as_deref(),
            Some("file_name")
        );
        assert_eq!(unresolved_placeholder("/data/file.txt"), None);
        assert_eq!(unresolved_placeholder("{{ unclosed"), None);
    }

    #[test]
    fn test_substitute_variables() {
        let client = HttpClient::default();
        let mut vars = HashMap::new();
        vars.insert("token".to_string(), "abc123".to_string());
        vars.insert("user".to_string(), "john".to_string());

        let template = "Bearer {{ token }} for {{ user }}";
        let result = client.substitute_variables(template, &vars);

        assert_eq!(result, "Bearer abc123 for john");
    }

    #[test]
    fn test_substitute_no_variables() {
        let client = HttpClient::default();
        let vars = HashMap::new();

        let template = "No variables here";
        let result = client.substitute_variables(template, &vars);

        assert_eq!(result, "No variables here");
    }

    #[test]
    fn test_substitute_variables_in_url() {
        let client = HttpClient::default();
        let mut vars = HashMap::new();
        vars.insert("user_id".to_string(), "42".to_string());

        let template = "/users/{{ user_id }}/profile";
        let result = client.substitute_variables(template, &vars);

        assert_eq!(result, "/users/42/profile");
    }
}
