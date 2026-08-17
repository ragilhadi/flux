use crate::config::Config;
use std::collections::BTreeSet;

/// Replacement written in place of anything sensitive.
pub const REDACTED: &str = "***";

/// Literal secrets shorter than this are ignored.
///
/// A one- or two-character "secret" would match everywhere and turn readable
/// errors into noise, which helps nobody and hides real failures.
const MIN_LITERAL_SECRET_LEN: usize = 4;

/// Header names whose values are treated as secrets.
const SENSITIVE_HEADER_MARKERS: &[&str] = &[
    "authorization",
    "cookie",
    "token",
    "secret",
    "password",
    "api-key",
    "apikey",
    "api_key",
    "auth",
    "signature",
    "session",
];

/// Query-string parameters whose values are treated as secrets.
const SENSITIVE_QUERY_PARAMS: &[&str] = &[
    "token",
    "access_token",
    "refresh_token",
    "id_token",
    "api_key",
    "apikey",
    "auth",
    "authorization",
    "secret",
    "password",
    "passwd",
    "pwd",
    "signature",
    "session",
    "session_id",
    "sessionid",
];

/// Scrubs values that must never leave the process through the live dashboard.
///
/// Two layers, because neither is sufficient alone:
///
/// - literal secrets taken from the configuration (credential header values,
///   request bodies, multipart field values, and anything listed under
///   `live_dashboard.redact`), which catches secrets echoed back verbatim;
/// - shape-based rules for bearer tokens, URL credentials and sensitive query
///   parameters, which catch secrets Flux never saw in the configuration —
///   values extracted from a response at runtime, for instance.
#[derive(Debug, Default, Clone)]
pub struct Redactor {
    literals: Vec<String>,
}

impl Redactor {
    /// Build a redactor from explicit literal secrets.
    ///
    /// Values shorter than [`MIN_LITERAL_SECRET_LEN`] are dropped, and longer
    /// values are matched first so a secret that contains another secret is
    /// replaced as a whole.
    pub fn new<I, S>(secrets: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let unique: BTreeSet<String> = secrets
            .into_iter()
            .map(Into::into)
            .map(|secret| secret.trim().to_string())
            .filter(|secret| secret.chars().count() >= MIN_LITERAL_SECRET_LEN)
            .collect();

        let mut literals: Vec<String> = unique.into_iter().collect();
        literals.sort_by_key(|secret| std::cmp::Reverse(secret.len()));

        Self { literals }
    }

    /// Collect every secret the configuration exposes.
    pub fn from_config(config: &Config) -> Self {
        let mut secrets: Vec<String> = Vec::new();

        for (name, value) in &config.headers {
            if is_sensitive_header(name) {
                secrets.push(value.clone());
            }
        }
        if let Some(body) = &config.body {
            secrets.push(body.clone());
        }
        collect_multipart_secrets(config.multipart.as_deref(), &mut secrets);

        for scenario in &config.scenarios {
            for (name, value) in &scenario.headers {
                if is_sensitive_header(name) {
                    secrets.push(value.clone());
                }
            }
            if let Some(body) = &scenario.body {
                secrets.push(body.clone());
            }
            collect_multipart_secrets(scenario.multipart.as_deref(), &mut secrets);
        }

        if let Some(dashboard) = &config.live_dashboard {
            secrets.extend(dashboard.redact.iter().cloned());
        }

        Self::new(secrets)
    }

    /// Redact a value for display.
    pub fn redact(&self, value: &str) -> String {
        let mut redacted = value.to_string();
        for literal in &self.literals {
            if redacted.contains(literal.as_str()) {
                redacted = redacted.replace(literal.as_str(), REDACTED);
            }
        }
        redacted = redact_bearer_tokens(&redacted);
        redacted = redact_url_credentials(&redacted);
        redact_query_parameters(&redacted)
    }

    /// Redact an optional value, leaving `None` untouched.
    pub fn redact_optional(&self, value: Option<&str>) -> Option<String> {
        value.map(|value| self.redact(value))
    }
}

fn collect_multipart_secrets(
    parts: Option<&[crate::config::MultipartPart]>,
    out: &mut Vec<String>,
) {
    for part in parts.unwrap_or_default() {
        // Field values are request-body content, so they are treated as
        // sensitive; file paths are not secrets and stay readable.
        if let Some(value) = &part.value {
            out.push(value.clone());
        }
    }
}

fn is_sensitive_header(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    SENSITIVE_HEADER_MARKERS
        .iter()
        .any(|marker| name.contains(marker))
}

/// Replace the token in `Authorization: Bearer <token>` style text.
fn redact_bearer_tokens(value: &str) -> String {
    const PREFIX: &str = "bearer ";

    let lowered = value.to_ascii_lowercase();
    let mut redacted = String::with_capacity(value.len());
    let mut cursor = 0;

    while let Some(offset) = lowered[cursor..].find(PREFIX) {
        let start = cursor + offset;
        let token_start = start + PREFIX.len();

        // Only a standalone "bearer" introduces a token.
        let preceded_by_word = value[..start]
            .chars()
            .next_back()
            .is_some_and(|character| character.is_alphanumeric());
        if preceded_by_word {
            redacted.push_str(&value[cursor..token_start]);
            cursor = token_start;
            continue;
        }

        let token_end = value[token_start..]
            .find(|character: char| character.is_whitespace() || character == '"')
            .map_or(value.len(), |offset| token_start + offset);

        redacted.push_str(&value[cursor..token_start]);
        if token_end > token_start {
            redacted.push_str(REDACTED);
        }
        cursor = token_end;
    }

    redacted.push_str(&value[cursor..]);
    redacted
}

/// Replace `user:password@` credentials embedded in a URL.
fn redact_url_credentials(value: &str) -> String {
    const SEPARATOR: &str = "://";

    let mut redacted = String::with_capacity(value.len());
    let mut cursor = 0;

    while let Some(offset) = value[cursor..].find(SEPARATOR) {
        let authority_start = cursor + offset + SEPARATOR.len();
        let authority_end = value[authority_start..]
            .find(|character: char| {
                character.is_whitespace() || matches!(character, '/' | '?' | '#' | '"')
            })
            .map_or(value.len(), |offset| authority_start + offset);
        let authority = &value[authority_start..authority_end];

        redacted.push_str(&value[cursor..authority_start]);
        match authority.rfind('@') {
            Some(at) => {
                redacted.push_str(REDACTED);
                redacted.push_str(&authority[at..]);
            }
            None => redacted.push_str(authority),
        }
        cursor = authority_end;
    }

    redacted.push_str(&value[cursor..]);
    redacted
}

/// Replace the values of sensitive query-string parameters.
fn redact_query_parameters(value: &str) -> String {
    let mut redacted = String::with_capacity(value.len());
    let mut cursor = 0;

    while let Some(offset) = value[cursor..].find(['?', '&']) {
        let key_start = cursor + offset + 1;
        redacted.push_str(&value[cursor..key_start]);
        cursor = key_start;

        let Some(equals) = value[key_start..].find('=') else {
            break;
        };
        let equals = key_start + equals;
        let key = &value[key_start..equals];
        // A separator inside the key means this was not a parameter at all.
        if key.is_empty() || key.contains(['?', '&', ' ']) {
            continue;
        }

        let value_start = equals + 1;
        // Errors quote URLs inside brackets and quotes, so those close the
        // value too; stopping late would leave part of a secret visible.
        let value_end = value[value_start..]
            .find(|character: char| {
                character.is_whitespace() || matches!(character, '&' | '"' | '\'' | '#' | ')' | '>')
            })
            .map_or(value.len(), |offset| value_start + offset);

        if is_sensitive_query_parameter(key) && value_end > value_start {
            redacted.push_str(&value[key_start..value_start]);
            redacted.push_str(REDACTED);
            cursor = value_end;
        } else {
            redacted.push_str(&value[key_start..value_end]);
            cursor = value_end;
        }
    }

    redacted.push_str(&value[cursor..]);
    redacted
}

fn is_sensitive_query_parameter(key: &str) -> bool {
    let key = key.trim().to_ascii_lowercase();
    SENSITIVE_QUERY_PARAMS
        .iter()
        .any(|parameter| key == *parameter)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{LiveDashboardConfig, MultipartPart, OutputConfig, Scenario};
    use std::collections::HashMap;

    fn config_with(headers: HashMap<String, String>, body: Option<&str>) -> Config {
        Config {
            target: Some("https://api.example.com".to_string()),
            method: Some("GET".to_string()),
            headers,
            body: body.map(ToString::to_string),
            multipart: None,
            scenarios: vec![],
            concurrency: 1,
            duration: "1s".to_string(),
            timeout: "1s".to_string(),
            ramp_up: None,
            load_profile: None,
            think_time: None,
            retry_count: 0,
            retry_delay: None,
            retry_on_status: vec![],
            assertions: None,
            prometheus_port: None,
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

    #[test]
    fn test_configured_header_secrets_are_redacted() {
        let headers = HashMap::from([
            (
                "Authorization".to_string(),
                "Bearer s3cr3t-token".to_string(),
            ),
            ("Cookie".to_string(), "session=abcdef123456".to_string()),
            ("Accept".to_string(), "application/json".to_string()),
        ]);
        let redactor = Redactor::from_config(&config_with(headers, None));

        let redacted = redactor.redact("upstream rejected Bearer s3cr3t-token");
        assert!(!redacted.contains("s3cr3t-token"), "{redacted}");
        assert!(redacted.contains(REDACTED), "{redacted}");

        let cookie = redactor.redact("sent session=abcdef123456");
        assert!(!cookie.contains("abcdef123456"), "{cookie}");

        // Non-sensitive header values are not secrets and stay readable.
        assert_eq!(
            redactor.redact("Accept was application/json"),
            "Accept was application/json"
        );
    }

    #[test]
    fn test_request_bodies_and_multipart_fields_are_redacted() {
        let mut config = config_with(HashMap::new(), Some("{\"password\":\"hunter2000\"}"));
        config.multipart = Some(vec![
            MultipartPart {
                part_type: "field".to_string(),
                name: "session".to_string(),
                value: Some("field-secret-value".to_string()),
                path: None,
            },
            MultipartPart {
                part_type: "file".to_string(),
                name: "attachment".to_string(),
                value: None,
                path: Some("/app/data/sample.txt".to_string()),
            },
        ]);
        let redactor = Redactor::from_config(&config);

        let body = redactor.redact("payload {\"password\":\"hunter2000\"} was rejected");
        assert!(!body.contains("hunter2000"), "{body}");

        let field = redactor.redact("multipart field-secret-value failed");
        assert!(!field.contains("field-secret-value"), "{field}");

        // File paths are not secrets: hiding them would hide the failing input.
        assert!(redactor
            .redact("cannot read /app/data/sample.txt")
            .contains("/app/data/sample.txt"));
    }

    #[test]
    fn test_scenario_secrets_and_configured_extras_are_redacted() {
        let mut config = config_with(HashMap::new(), None);
        config.scenarios = vec![Scenario {
            name: "login".to_string(),
            method: "POST".to_string(),
            url: "/login".to_string(),
            headers: HashMap::from([(
                "X-Api-Key".to_string(),
                "scenario-api-key-value".to_string(),
            )]),
            body: Some("{\"user\":\"demo\"}".to_string()),
            multipart: None,
            extract: HashMap::new(),
            depends_on: None,
            think_time: None,
            retry_count: None,
            retry_delay: None,
            retry_on_status: None,
            assertions: None,
        }];
        config.live_dashboard = Some(LiveDashboardConfig {
            bind: "127.0.0.1:9090".to_string(),
            refresh_ms: 1_000,
            redact: vec!["extra-configured-secret".to_string()],
        });
        let redactor = Redactor::from_config(&config);

        assert!(!redactor
            .redact("key scenario-api-key-value rejected")
            .contains("scenario-api-key-value"));
        assert!(!redactor
            .redact("saw extra-configured-secret in the response")
            .contains("extra-configured-secret"));
    }

    #[test]
    fn test_runtime_bearer_tokens_are_redacted_without_configuration() {
        let redactor = Redactor::default();

        let redacted = redactor.redact("request failed with Bearer eyJhbGciOiJIUzI1NiJ9.body");
        assert_eq!(redacted, "request failed with Bearer ***");

        // Case-insensitive, and the rest of the message survives.
        let mixed = redactor.redact("header \"Authorization: bEaReR abc123\" rejected");
        assert!(!mixed.contains("abc123"), "{mixed}");
        assert!(mixed.contains("rejected"), "{mixed}");

        // A word merely ending in "bearer" does not introduce a token.
        assert_eq!(
            redactor.redact("torchbearer carried on"),
            "torchbearer carried on"
        );
    }

    #[test]
    fn test_url_credentials_and_sensitive_query_parameters_are_redacted() {
        let redactor = Redactor::default();

        let credentials =
            redactor.redact("error sending request for https://user:pass@api.example.com/v1");
        assert_eq!(
            credentials,
            "error sending request for https://***@api.example.com/v1"
        );

        let query = redactor
            .redact("GET https://api.example.com/v1?user=demo&token=abc123def&page=2 failed");
        assert!(query.contains("user=demo"), "{query}");
        assert!(query.contains("page=2"), "{query}");
        assert!(!query.contains("abc123def"), "{query}");
        assert!(query.contains("token=***"), "{query}");
    }

    #[test]
    fn test_short_values_are_not_treated_as_secrets() {
        // "GET" as a secret would redact half of every error message.
        let redactor = Redactor::new(["ab", "GET"]);
        assert_eq!(redactor.redact("GET /ab failed"), "GET /ab failed");
    }

    #[test]
    fn test_overlapping_secrets_are_fully_replaced() {
        let redactor = Redactor::new(["token-value", "token-value-extended"]);
        let redacted = redactor.redact("saw token-value-extended here");
        assert_eq!(redacted, "saw *** here");
    }
}
