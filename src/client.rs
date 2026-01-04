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
    pub fn new() -> Result<Self> {
        let client = Client::builder()
            .timeout(Duration::from_secs(30))
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
        // Build full URL
        let url = if scenario.url.starts_with("http://") || scenario.url.starts_with("https://") {
            scenario.url.clone()
        } else if let Some(base) = base_url {
            format!("{}{}", base.trim_end_matches('/'), scenario.url)
        } else {
            scenario.url.clone()
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
            request = self.build_multipart_request(request, parts).await?;
        } else if let Some(body_content) = &scenario.body {
            let substituted_body = self.substitute_variables(body_content, variables);
            request = request.body(substituted_body);
        }

        let response = request.send().await?;
        Ok(response)
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

impl Default for HttpClient {
    fn default() -> Self {
        Self::new().expect("Failed to create HTTP client")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_substitute_variables() {
        let client = HttpClient::new().unwrap();
        let mut vars = HashMap::new();
        vars.insert("token".to_string(), "abc123".to_string());
        vars.insert("user".to_string(), "john".to_string());

        let template = "Bearer {{ token }} for {{ user }}";
        let result = client.substitute_variables(template, &vars);

        assert_eq!(result, "Bearer abc123 for john");
    }

    #[test]
    fn test_substitute_no_variables() {
        let client = HttpClient::new().unwrap();
        let vars = HashMap::new();

        let template = "No variables here";
        let result = client.substitute_variables(template, &vars);

        assert_eq!(result, "No variables here");
    }
}
