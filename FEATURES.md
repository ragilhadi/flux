# Feature Requests

This document lists features planned for the Flux load testing tool.
Each entry includes a detailed description of the feature, what needs to be implemented, and success criteria.

---

## FEAT-01: Configurable Per-Request Timeout

**Priority:** High  
**File(s):** `src/config.rs`, `src/client.rs`

### Description

The HTTP client is built with a hardcoded 30-second request timeout:

```rust
let client = Client::builder()
    .timeout(Duration::from_secs(30))
    ...
```

Different workloads require different timeouts. A short-lived API test may only need 5 seconds, while a file-upload test may need 120 seconds. There is currently no way to change the timeout without modifying and recompiling the source code.

### What Needs to Be Implemented

1. Add an optional `timeout` field to `Config` (in `config.rs`), accepting a duration string consistent with the `duration` field (e.g., `"5s"`, `"2m"`). Default to 30 s when omitted.
2. Pass the parsed timeout value to `HttpClient::new()` (or a new `HttpClient::with_timeout()` constructor).
3. Use the configured timeout when building the `reqwest::Client`.

Example configuration:

```yaml
target: "https://api.example.com/upload"
method: "POST"
timeout: "60s"
concurrency: 5
duration: "30s"
```

### Success Criteria

- Setting `timeout: "5s"` causes requests that take longer than 5 seconds to be recorded as errors.
- Omitting `timeout` retains the current 30-second default.
- A unit test verifies that timeout configuration is parsed and applied correctly.
- All existing tests continue to pass.

---

## FEAT-02: Ramp-Up Load Pattern

**Priority:** High  
**File(s):** `src/config.rs`, `src/executor.rs`

### Description

Currently Flux starts all configured workers at once. Real-world load tests often need a warm-up phase that gradually increases concurrency from a low starting point to the target level. This avoids overloading both the tool and the target server from the very first second.

### What Needs to Be Implemented

1. Add an optional `ramp_up` field to `Config` (duration string, e.g., `"30s"`). When set, the number of active workers increases linearly from 1 to `concurrency` over the ramp-up period.
2. In `run_async()` / `run_sync()`, spawn workers with a calculated inter-worker delay instead of starting all at once.
3. Report ramp-up configuration in the terminal banner.

Example configuration:

```yaml
concurrency: 50
ramp_up: "30s"
duration: "60s"
mode: "async"
```

Workers are added at rate `concurrency / ramp_up_secs` per second (≈ 1–2 workers per second for the example above).

### Success Criteria

- With `ramp_up: "10s"` and `concurrency: 10`, one new worker is spawned every second for the first 10 seconds.
- The live metrics display shows gradually increasing RPS during the ramp-up phase.
- When `ramp_up` is not set, behavior is unchanged (all workers start immediately).
- All existing tests continue to pass.

---

## FEAT-03: Per-Request Think Time (Pacing)

**Priority:** Medium  
**File(s):** `src/config.rs`, `src/executor.rs`

### Description

In real-world browser or API usage, there is a natural pause between consecutive requests from the same user session ("think time"). Without think time, each worker loops as fast as possible, which can produce artificially high load that does not represent realistic usage patterns.

### What Needs to Be Implemented

1. Add an optional `think_time` field to `Config` and to `Scenario` (duration string). When set at the top level it applies to simple-mode requests; when set on a scenario step it applies after that step.
2. In `worker_loop()`, after each request (or each scenario step), sleep for the configured think time before the next iteration.
3. Support a random jitter option (`think_time_jitter`) to vary the pause (±N ms).

Example configuration:

```yaml
think_time: "500ms"     # 500 ms pause between requests in simple mode
concurrency: 10
duration: "30s"
```

### Success Criteria

- With `think_time: "1s"` and `concurrency: 10`, the effective RPS is no higher than 10 RPS regardless of server response time.
- Think time does not count towards request latency metrics.
- All existing tests continue to pass.

---

## FEAT-04: Response Assertions / Pass-Fail Criteria

**Priority:** Medium  
**File(s):** `src/config.rs`, `src/executor.rs`, `src/metrics.rs`

### Description

Flux currently collects metrics but does not allow users to define what constitutes a passing test. Users cannot specify that the test should fail if p99 latency exceeds 500 ms, or if the error rate exceeds 1%, or if a response body does not contain an expected string.

### What Needs to Be Implemented

1. Add an optional `assertions` block to `Config` supporting:
   - `max_error_rate: <percentage>` – test fails if error rate exceeds this value.
   - `max_p99_ms: <milliseconds>` – test fails if p99 latency exceeds this value.
   - `max_p95_ms: <milliseconds>` – test fails if p95 latency exceeds this value.
   - `max_avg_ms: <milliseconds>` – test fails if mean latency exceeds this value.
2. Add an optional `assert` block to individual `Scenario` steps:
   - `status_code: <code>` – request fails if HTTP status differs from expected.
   - `body_contains: "<string>"` – request fails if response body does not include the string.
3. After the test completes, evaluate assertions against the final `MetricsSummary`. If any assertion fails, print a clear failure message and exit with a non-zero status code.

Example configuration:

```yaml
assertions:
  max_error_rate: 1.0
  max_p99_ms: 500
  max_avg_ms: 200

scenarios:
  - name: "login"
    method: "POST"
    url: "/auth/login"
    assert:
      status_code: 200
      body_contains: "access_token"
```

### Success Criteria

- When all assertions pass, the process exits with code 0.
- When any assertion fails, the process exits with code 1 and prints which assertion failed and by how much.
- When no `assertions` block is present, behavior is unchanged.
- A unit test verifies assertion evaluation against a known summary.
- All existing tests continue to pass.

---

## FEAT-05: Per-Scenario Metrics Breakdown in Reports

**Priority:** Medium  
**File(s):** `src/metrics.rs`, `src/reporter.rs`, `src/templates/report.html`

### Description

When a multi-step scenario is configured (e.g., login → get-profile → update-profile), the current reports aggregate all requests together. It is impossible to tell from the JSON or HTML report how each individual step performed, making it difficult to identify which step is slow or error-prone.

### What Needs to Be Implemented

1. In `MetricsSummary`, add a `per_scenario` map from scenario name to a per-scenario summary struct (containing its own total, success, failure counts, throughput, and latency percentiles).
2. In `generate_summary()`, partition results by `scenario_name` and compute separate histograms per scenario.
3. In `generate_json()`, include the `per_scenario` data in the JSON output.
4. In the HTML report, add a collapsible table or separate section listing per-scenario statistics.

### Success Criteria

- The JSON report contains a `per_scenario` key with individual statistics for each named scenario step.
- The HTML report displays a per-scenario breakdown table.
- When running in simple (non-scenario) mode, the `per_scenario` section is empty or absent.
- All existing tests continue to pass.

---

## FEAT-06: CLI Argument Parsing with `clap`

**Priority:** Medium  
**File(s):** `src/main.rs`, `Cargo.toml`

### Description

Flux currently accepts no command-line arguments. The config path is hardcoded and the only way to modify behavior is by editing the YAML file or recompiling. Adding proper CLI argument support enables easier integration with CI pipelines and local development.

### What Needs to Be Implemented

Use the `clap` crate (or `argh`) to parse the following arguments:

| Flag | Short | Default | Description |
|------|-------|---------|-------------|
| `--config` | `-c` | `/app/config.yaml` | Path to configuration file |
| `--concurrency` | `-n` | (from config) | Override concurrency from config |
| `--duration` | `-d` | (from config) | Override duration from config |
| `--output-json` | | (from config) | Override JSON output path |
| `--output-html` | | (from config) | Override HTML output path |

CLI arguments should override values in the YAML config when both are provided.

### Success Criteria

- `flux --config ./my-test.yaml` loads the specified file.
- `flux -c ./my-test.yaml -n 50 -d 60s` runs with concurrency=50 and duration=60s regardless of what the YAML says.
- `flux --help` prints a usage message.
- All existing tests continue to pass.

---

## FEAT-07: Environment Variable Substitution in Config Values

**Priority:** Medium  
**File(s):** `src/config.rs`

### Description

Sensitive or deployment-specific values (API keys, base URLs, credentials) should not be committed to YAML configuration files. Users need a way to inject these values at runtime through environment variables.

### What Needs to Be Implemented

Support `${ENV_VAR}` or `$ENV_VAR` syntax in any string field of the YAML config. After parsing, perform a substitution pass over all string fields before validation.

Example:

```yaml
target: "${API_BASE_URL}"

scenarios:
  - name: "login"
    method: "POST"
    url: "/auth/login"
    body: |
      {
        "username": "${TEST_USER}",
        "password": "${TEST_PASS}"
      }
```

The values are resolved at startup from the process environment. Prefer exporting variables from a shell profile, a `.env` loader, or a secrets manager rather than inlining them on the command line (which would expose them in shell history and process listings):

```bash
# Recommended: export variables before running
export API_BASE_URL=https://staging.example.com
export TEST_USER=user1
export TEST_PASS=secret
flux
```

### Success Criteria

- Any `${VAR}` placeholder in a string config field is replaced by the corresponding environment variable value.
- If a referenced environment variable is not set, the tool either substitutes an empty string (lenient mode) or exits with a clear error message (strict mode, configurable).
- A unit test verifies substitution with both set and unset environment variables.
- All existing tests continue to pass.

---

## FEAT-08: Prometheus Metrics Export

**Priority:** Low  
**File(s):** `src/main.rs`, `Cargo.toml` (new module `src/prometheus.rs`)

### Description

Teams running Flux inside Kubernetes or alongside Prometheus/Grafana setups need to scrape live metrics during a test run, not just from the final report. A Prometheus-compatible `/metrics` HTTP endpoint would enable real-time dashboards.

### What Needs to Be Implemented

1. Add an optional `prometheus_port` field to `Config` (integer). When set, Flux starts a lightweight HTTP server on that port during the test.
2. Expose the following gauge/counter metrics in Prometheus text format:
   - `flux_requests_total{status="success|failure"}` – cumulative request count.
   - `flux_request_duration_ms{quantile="0.5|0.9|0.95|0.99"}` – latency summary.
   - `flux_rps` – current requests per second.
3. Stop the metrics server cleanly when the test ends.

Example configuration:

```yaml
prometheus_port: 9090
concurrency: 20
duration: "60s"
```

### Success Criteria

- While a test is running, `curl http://localhost:9090/metrics` returns a valid Prometheus text document.
- The metrics are updated at least once per second.
- When `prometheus_port` is not set, no HTTP server is started (existing behavior).
- All existing tests continue to pass.

---

## FEAT-09: CSV Report Output

**Priority:** Low  
**File(s):** `src/config.rs`, `src/reporter.rs`

### Description

The current output formats are JSON and HTML. Some users need a machine-readable tabular format (CSV) to import into spreadsheets, BI tools, or custom dashboards without needing to parse JSON.

### What Needs to Be Implemented

1. Add an optional `csv` field to `OutputConfig` in `config.rs`.
2. Implement `Reporter::generate_csv()` that writes one row per `RequestResult` with columns: `timestamp`, `scenario`, `latency_ms`, `status_code`, `error`.
3. Call `generate_csv()` from `main.rs` when the path is configured.

Example configuration:

```yaml
output:
  json: "/app/results/output.json"
  html: "/app/results/report.html"
  csv: "/app/results/results.csv"
```

### Success Criteria

- When `output.csv` is configured, a valid CSV file is written after the test.
- The CSV has a header row and one data row per request.
- When `output.csv` is not configured, no CSV file is created (backward compatible).
- A unit test verifies CSV output format.
- All existing tests continue to pass.

---

## FEAT-10: Retry Logic on Request Failure

**Priority:** Low  
**File(s):** `src/config.rs`, `src/client.rs`, `src/executor.rs`

### Description

Transient network errors (connection reset, DNS timeout) can cause sporadic failures that inflate the error rate. A configurable retry mechanism allows Flux to transparently retry failed requests a given number of times before recording them as failures, producing more stable results in flaky network environments.

### What Needs to Be Implemented

1. Add optional `retry` fields to `Config` and `Scenario`:
   - `retry_count: <integer>` (default 0 – no retries)
   - `retry_delay: "<duration>"` (default `"0s"` – immediate retry)
   - `retry_on_status: [<code>, ...]` (optional; retry on specific HTTP status codes such as 503)
2. In the executor (or client), wrap request execution in a retry loop up to `retry_count` times.
3. Record only the final attempt's latency and status code; log each retry attempt at `debug` level.

Example configuration:

```yaml
retry_count: 3
retry_delay: "500ms"
retry_on_status: [500, 502, 503]
```

### Success Criteria

- A request that fails on the first attempt but succeeds on the second is counted as successful.
- The latency recorded is the latency of the final (successful) attempt.
- Each retry is logged at `debug` level with the attempt number.
- When `retry_count` is 0 (default), behavior is unchanged.
- A unit test verifies retry logic with a mock or stub HTTP response.
- All existing tests continue to pass.
