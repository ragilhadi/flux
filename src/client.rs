use crate::config::{MultipartPart, Scenario};
use anyhow::Result;
use bytes::Bytes;
use reqwest::{Client, Method, Response};
use std::collections::HashMap;
use std::path::Path;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Multipart file contents are cached by resolved path so a fixed upload
/// fixture is read from disk once rather than on every request. Capped so a
/// scenario with a per-request (extracted) file name cannot grow this
/// without bound; such paths simply fall back to reading each time.
const MAX_CACHED_MULTIPART_FILES: usize = 128;

/// HTTP client wrapper for making requests
///
/// Cheap to clone: `reqwest::Client` is `Arc`-backed internally, so cloning
/// shares the same connection pool rather than opening a new one. The file
/// cache is shared the same way.
#[derive(Clone)]
pub struct HttpClient {
    client: Client,
    file_cache: Arc<Mutex<HashMap<String, Bytes>>>,
}

impl HttpClient {
    /// Create a new HTTP client
    pub fn new(timeout: Duration) -> Result<Self> {
        let client = Client::builder()
            .timeout(timeout)
            .pool_max_idle_per_host(100)
            .build()?;

        Ok(Self {
            client,
            file_cache: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    /// Read a multipart file's contents, from cache when available.
    ///
    /// A path is cached after its first successful read. Most multipart
    /// scenarios upload the same fixture file on every request, so this
    /// turns repeated disk reads under load into one. A path that only ever
    /// appears once still costs a single read either way.
    async fn read_multipart_file(&self, path: &str) -> Result<Bytes> {
        if let Some(cached) = self
            .file_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(path)
        {
            return Ok(cached.clone());
        }

        let file_path = Path::new(path);
        if !file_path.exists() {
            anyhow::bail!("File not found: {}", path);
        }
        let content = Bytes::from(tokio::fs::read(file_path).await?);

        let mut cache = self
            .file_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if cache.len() < MAX_CACHED_MULTIPART_FILES {
            cache.insert(path.to_string(), content.clone());
        }
        Ok(content)
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
        let raw_url = self.substitute_url_variables(&scenario.url, variables);
        check_unresolved("url", &raw_url, &scenario.name)?;
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
            check_unresolved(
                &format!("header '{key}'"),
                &substituted_value,
                &scenario.name,
            )?;
            request = request.header(key, substituted_value);
        }

        // Handle multipart or body
        if let Some(parts) = &scenario.multipart {
            let rendered = self.render_multipart_parts(parts, variables, &scenario.name)?;
            request = self.build_multipart_request(request, &rendered).await?;
        } else if let Some(body_content) = &scenario.body {
            let substituted_body = self.substitute_variables(body_content, variables);
            check_unresolved("body", &substituted_body, &scenario.name)?;
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
                    check_unresolved(
                        &format!("multipart '{field}' of part '{}'", part.name),
                        content,
                        scenario_name,
                    )?;
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
                        let file_name = Path::new(path)
                            .file_name()
                            .and_then(|n| n.to_str())
                            .unwrap_or("file")
                            .to_string();

                        let file_content = self.read_multipart_file(path).await?;

                        let multipart_part =
                            reqwest::multipart::Part::stream(file_content).file_name(file_name);

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

    /// Substitute variables in a string using {{ variable }} syntax.
    ///
    /// A single pass over `template`: each `{{ name }}` is replaced with
    /// `variables[name]` as it is found, and an unknown name is left as-is
    /// for the caller to reject via [`unresolved_placeholder`]. Scanning only
    /// the original template — never text already substituted in — keeps the
    /// result independent of `variables`' (hash-order) iteration order, and
    /// means a substituted value can never itself be mistaken for another
    /// placeholder.
    fn substitute_variables(&self, template: &str, variables: &HashMap<String, String>) -> String {
        substitute_placeholders(template, variables, |value| value.to_string())
    }

    /// Substitute variables into a URL, percent-encoding each inserted value.
    ///
    /// Only the text coming from a variable is encoded; the surrounding
    /// template (scheme, static path segments, literal query syntax) is left
    /// untouched. Without this, a value containing `&`, `?`, `#` or a space —
    /// entirely plausible for a value extracted from a response — corrupts
    /// the request's path or query structure instead of being sent as data.
    fn substitute_url_variables(
        &self,
        template: &str,
        variables: &HashMap<String, String>,
    ) -> String {
        substitute_placeholders(template, variables, |value| {
            percent_encoding::utf8_percent_encode(value, URL_VALUE_ENCODE_SET).to_string()
        })
    }
}

/// Characters a substituted URL value keeps unescaped: RFC 3986 unreserved
/// characters. Everything else — including `&`, `?`, `#`, `/` and space — is
/// percent-encoded so an inserted value cannot alter the URL's structure.
static URL_VALUE_ENCODE_SET: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'~');

/// Replace each `{{ name }}` placeholder in `template` by looking it up in
/// `variables` and passing it through `render`; a name with no match is left
/// as the original `{{ name }}` text.
fn substitute_placeholders(
    template: &str,
    variables: &HashMap<String, String>,
    mut render: impl FnMut(&str) -> String,
) -> String {
    let mut result = String::with_capacity(template.len());
    let mut rest = template;

    while let Some(start) = rest.find("{{") {
        result.push_str(&rest[..start]);
        let after_open = &rest[start + 2..];
        let Some(end) = after_open.find("}}") else {
            result.push_str(&rest[start..]);
            rest = "";
            break;
        };
        let name = after_open[..end].trim();
        match variables.get(name) {
            Some(value) => result.push_str(&render(value)),
            None => result.push_str(&rest[start..start + 2 + end + 2]),
        }
        rest = &after_open[end + 2..];
    }

    result.push_str(rest);
    result
}

/// Return the name inside the first `{{ ... }}` placeholder that survived
/// substitution, if any.
fn unresolved_placeholder(value: &str) -> Option<String> {
    let start = value.find("{{")?;
    let rest = &value[start + 2..];
    let end = rest.find("}}")?;
    Some(rest[..end].trim().to_string())
}

/// Check a rendered field for a placeholder no earlier step extracted, so
/// sending a literal `{{ name }}` to the target is a hard configuration error
/// instead of an unintended request.
fn check_unresolved(context: &str, content: &str, scenario_name: &str) -> Result<()> {
    if let Some(placeholder) = unresolved_placeholder(content) {
        anyhow::bail!(
            "Unresolved variable '{placeholder}' in {context} of scenario '{scenario_name}'; \
             no earlier step extracted it"
        );
    }
    Ok(())
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

    #[test]
    fn test_url_substitution_percent_encodes_inserted_values_only() {
        let client = HttpClient::default();
        let vars = HashMap::from([("query".to_string(), "a&b=c?d#e space".to_string())]);

        let result = client.substitute_url_variables("/search?q={{ query }}&limit=10", &vars);

        assert_eq!(result, "/search?q=a%26b%3Dc%3Fd%23e%20space&limit=10");
    }

    #[test]
    fn test_url_substitution_leaves_the_static_template_untouched() {
        let client = HttpClient::default();
        let vars = HashMap::from([("id".to_string(), "42".to_string())]);

        let result = client
            .substitute_url_variables("https://api.example.com/users/{{ id }}?x=1&y=2", &vars);

        assert_eq!(result, "https://api.example.com/users/42?x=1&y=2");
    }

    #[test]
    fn test_substitution_is_a_single_pass_over_the_original_template() {
        // A substituted value that happens to contain `{{ other }}`-shaped text
        // must not itself be rescanned for placeholders.
        let client = HttpClient::default();
        let vars = HashMap::from([
            ("a".to_string(), "{{ b }}".to_string()),
            ("b".to_string(), "real".to_string()),
        ]);

        let result = client.substitute_variables("start {{ a }} end", &vars);

        assert_eq!(result, "start {{ b }} end");
    }

    #[tokio::test]
    async fn test_unresolved_header_placeholder_is_rejected_before_sending() {
        let scenario = Scenario {
            name: "profile".to_string(),
            method: "GET".to_string(),
            url: "/profile".to_string(),
            headers: HashMap::from([(
                "Authorization".to_string(),
                "Bearer {{ token }}".to_string(),
            )]),
            body: None,
            multipart: None,
            extract: HashMap::new(),
            depends_on: None,
            think_time: None,
            retry_count: None,
            retry_delay: None,
            retry_on_status: None,
            assertions: None,
        };

        let client = HttpClient::default();
        let error = client
            .execute_scenario(Some("http://127.0.0.1:1"), &scenario, &HashMap::new())
            .await
            .unwrap_err()
            .to_string();

        assert!(error.contains("Unresolved variable 'token'"), "{error}");
        assert!(error.contains("header 'Authorization'"), "{error}");
    }

    #[tokio::test]
    async fn test_unresolved_url_placeholder_is_rejected_before_sending() {
        let scenario = Scenario {
            name: "profile".to_string(),
            method: "GET".to_string(),
            url: "/users/{{ user_id }}".to_string(),
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
        };

        let client = HttpClient::default();
        let error = client
            .execute_scenario(Some("http://127.0.0.1:1"), &scenario, &HashMap::new())
            .await
            .unwrap_err()
            .to_string();

        assert!(error.contains("Unresolved variable 'user_id'"), "{error}");
        assert!(error.contains(" url "), "{error}");
    }

    #[tokio::test]
    async fn test_multipart_file_is_cached_across_requests() {
        let unique = format!("{}-{}", std::process::id(), line!());
        let path = temp_file(&format!("flux-cache-{unique}.txt"), "cached contents");

        let scenario = multipart_scenario(vec![MultipartPart {
            part_type: "file".to_string(),
            name: "attachment".to_string(),
            path: Some(path.display().to_string()),
            value: None,
        }]);

        let client = HttpClient::default();

        for _ in 0..2 {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(capture_one_request(listener));

            let response = client
                .execute_scenario(
                    Some(&format!("http://{address}")),
                    &scenario,
                    &HashMap::new(),
                )
                .await
                .unwrap();
            assert_eq!(response.status().as_u16(), 200);
            let request = server.await.unwrap();
            assert!(request.contains("cached contents"), "{request}");
        }

        // Deleting the file after the first read proves the second request
        // was served from the cache rather than reading the file again.
        std::fs::remove_file(&path).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(capture_one_request(listener));
        let response = client
            .execute_scenario(
                Some(&format!("http://{address}")),
                &scenario,
                &HashMap::new(),
            )
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 200);
        let request = server.await.unwrap();
        assert!(request.contains("cached contents"), "{request}");
    }
}
