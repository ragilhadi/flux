# ⚡ Flux – High-Performance Container-Native Load Testing

[![Tests](https://github.com/ragilhadi/flux/workflows/Unit%20Tests/badge.svg)](https://github.com/ragilhadi/flux/actions)
[![Docker](https://img.shields.io/docker/v/ragilhadi/flux?label=docker)](https://hub.docker.com/r/ragilhadi/flux)
[![License](https://img.shields.io/badge/license-Apache%202.0-blue.svg)](LICENSE)

Flux is a fast, Docker-only load testing tool written in Rust.

No installation.  
No dependencies.  
Just Docker + YAML.

---

## 🚀 Features

- **Async or Sync** load generation with Tokio
- **Multi-step scenarios** with variable extraction
- **Multipart form-data** with file upload support
- **JSON + HTML reports** with beautiful charts
- **Real-time terminal display** with progress bars
- **JSONPath extraction** for chaining requests
- **Pure Docker usage** - no local installation needed
- **High performance** - built with Rust for maximum throughput

---

## 📦 Quick Start

### 1. Build the Docker image

```bash
docker build -t flux:latest .
```

### 2. Create required folders

```bash
mkdir -p data results
```

### 3. Put your files inside `data/` (for multipart uploads)

```bash
echo "Sample file content" > data/sample.txt
```

### 4. Create `config.yaml`

See the `samples/` folder for examples.

### 5. Run Flux

```bash
docker run --rm \
  -v $(pwd)/config.yaml:/app/config.yaml \
  -v $(pwd)/data:/app/data \
  -v $(pwd)/results:/app/results \
  flux:latest
```

---

## 🧩 Configuration

### Simple GET Request

```yaml
target: "https://api.example.com/endpoint"
method: "GET"

headers:
  Accept: "application/json"

concurrency: 20
duration: "30s"
timeout: "10s"
ramp_up: "10s"
think_time: "250ms"
retry_count: 2
retry_delay: "500ms"
retry_on_status: [502, 503, 504]
mode: "async"
prometheus_port: 9090

assertions:
  max_error_rate: 1.0
  max_p95_ms: 300
  max_p99_ms: 500
  max_avg_ms: 200

output:
  json: "/app/results/output.json"
  html: "/app/results/report.html"
  csv: "/app/results/requests.csv"
```

### Live Prometheus Metrics

Set `prometheus_port` to expose metrics while a test is running. Omit it to disable the HTTP server.

```yaml
prometheus_port: 9090
```

Scrape `http://localhost:9090/metrics` for success and failure counters, latency
quantiles, and the current request rate:

```bash
curl http://localhost:9090/metrics
```

### POST with JSON Body

```yaml
target: "https://api.example.com/users"
method: "POST"

headers:
  Content-Type: "application/json"

body: |
  {
    "username": "test",
    "email": "test@example.com"
  }

concurrency: 10
duration: "15s"
mode: "async"

output:
  json: "/app/results/output.json"
  html: "/app/results/report.html"
```

### Multipart Form-Data Upload

```yaml
target: "https://api.example.com/upload"
method: "POST"

multipart:
  - type: "file"
    name: "avatar"
    path: "/app/data/avatar.png"

  - type: "field"
    name: "username"
    value: "john"

  - type: "field"
    name: "age"
    value: "25"

concurrency: 5
duration: "10s"
mode: "async"

output:
  json: "/app/results/output.json"
  html: "/app/results/report.html"
```

### Multipart Upload With Scenario Variables

Inside scenarios, `{{ variable }}` placeholders are substituted in multipart
field names, field values and file paths, using variables extracted by earlier
steps:

```yaml
scenarios:
  - name: "login"
    method: "POST"
    url: "/login"
    body: '{"user":"demo","password":"demo"}'
    extract:
      token: "$.token"
      file_name: "$.upload.file_name"

  - name: "upload"
    method: "POST"
    url: "/uploads"
    depends_on: "login"
    multipart:
      - type: "field"
        name: "session"
        value: "{{ token }}"

      - type: "file"
        name: "attachment"
        path: "/app/data/{{ file_name }}"
```

If a placeholder cannot be resolved — a typo, or a variable no earlier step
extracted — the step fails with an explicit error and the request is *not*
sent, so the target never receives a literal `{{ token }}`.

Simple (non-scenario) multipart has no variables to substitute and is sent
exactly as written.

### Multi-Step Scenario with Variable Extraction

```yaml
target: "https://api.example.com"

scenarios:
  - name: "login"
    method: "POST"
    url: "/auth/login"
    headers:
      Content-Type: "application/json"
    body: |
      {
        "username": "test",
        "password": "secret"
      }
    extract:
      token: "$.access_token"
      user_id: "$.user.id"
    assert:
      status_code: 200
      body_contains: "access_token"

  - name: "get-profile"
    method: "GET"
    url: "/users/{{ user_id }}/profile"
    headers:
      Authorization: "Bearer {{ token }}"
    depends_on: "login"

  - name: "update-profile"
    method: "PUT"
    url: "/users/{{ user_id }}/profile"
    headers:
      Authorization: "Bearer {{ token }}"
      Content-Type: "application/json"
    body: |
      {
        "bio": "Updated bio"
      }
    depends_on: "get-profile"

concurrency: 10
duration: "30s"
mode: "async"

output:
  json: "/app/results/output.json"
  html: "/app/results/report.html"
```

---

## 📊 Configuration Options

### Global Settings

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `target` | string | Yes* | - | Base URL for requests |
| `method` | string | No | GET | HTTP method (GET, POST, PUT, DELETE, etc.) |
| `headers` | map | No | {} | HTTP headers |
| `body` | string | No | - | Request body (ignored if multipart is set) |
| `multipart` | array | No | - | Multipart form data |
| `scenarios` | array | No | [] | Multi-step scenarios |
| `concurrency` | integer | No | 10 | Number of concurrent workers |
| `duration` | string | No | 30s | Test duration (e.g., "30s", "5m", "1h") |
| `timeout` | string | No | 30s | Per-request timeout (e.g., "5s", "2m") |
| `ramp_up` | string | No | - | Warm-up: spread worker startup over this duration, *before* `duration` starts |
| `think_time` | string | No | - | Pause after each simple-mode request |
| `retry_count` | integer | No | 0 | Retry attempts after the initial request |
| `retry_delay` | string | No | 0s | Pause between retry attempts |
| `retry_on_status` | array | No | [] | HTTP status codes eligible for retry |
| `assertions` | object | No | - | Aggregate error-rate and latency quality gates |
| `mode` | string | No | async | Execution mode: "async" or "sync" |
| `output` | object | Yes | - | Output configuration |

\* Required if not using scenarios with full URLs

### Multipart Part

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `type` | string | Yes | "file" or "field" |
| `name` | string | Yes | Form field name |
| `path` | string | Yes (for file) | File path (must be in /app/data) |
| `value` | string | Yes (for field) | Field value |

### Scenario Step

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `name` | string | Yes | Step name; must be non-empty and unique |
| `method` | string | Yes | HTTP method |
| `url` | string | Yes | URL path or full URL |
| `headers` | map | No | HTTP headers |
| `body` | string | No | Request body |
| `multipart` | array | No | Multipart form data |
| `extract` | map | No | JSONPath extraction rules |
| `depends_on` | string | No | Name of an **earlier** step this depends on |
| `think_time` | string | No | Pause after this step |
| `retry_count` | integer | No | Override the global retry count |
| `retry_delay` | string | No | Override the global retry delay |
| `retry_on_status` | array | No | Override global retryable statuses |
| `assert` | object | No | Expected `status_code` and/or `body_contains` value |

### Scenario Dependencies

`depends_on` names another step in the same list. Because scenarios execute in
declaration order, the referenced step must be declared **before** the step that
uses it. Flux checks the whole scenario graph while loading the configuration
and refuses to start when it finds:

- an empty or duplicated scenario name;
- a `depends_on` value naming a scenario that does not exist (a typo);
- a step that depends on itself;
- a step that depends on a later step — which also rules out dependency cycles.

These are configuration mistakes, so they fail before a single request is sent
instead of silently turning a user journey into a partial load test.

At runtime a dependency can still fail — a bad response, a failed assertion, a
network error. The dependent step is then skipped for that iteration, and the
skip is counted per step in the terminal summary, the JSON report
(`summary.skipped_scenarios`) and the HTML report.

### Quality Gates

Aggregate assertions support `max_error_rate`, `max_p95_ms`, `max_p99_ms`, and `max_avg_ms`. Failed assertions are printed after reports are generated and cause Flux to exit with status code 1.

Scenario response assertions are recorded as request failures when the expected status or body content does not match.

### Report Outputs

`output.json` and `output.html` remain required. Set optional `output.csv` to write one row per request with timestamp, scenario, latency, status code, and error details.

### Variable Extraction

Use JSONPath syntax to extract values from JSON responses:

```yaml
extract:
  token: "$.access_token"
  user_id: "$.user.id"
  email: "$.user.email"
```

Then use extracted variables with `{{ variable_name }}` syntax:

```yaml
headers:
  Authorization: "Bearer {{ token }}"
url: "/users/{{ user_id }}/profile"
```

### Environment Variables

Any string configuration value can reference an environment variable with `${VAR}`. Flux resolves these values before validating the configuration and exits with a clear error if a referenced variable is not set.

```yaml
target: "${API_BASE_URL}"
headers:
  Authorization: "Bearer ${API_TOKEN}"
```

### Command-Line Overrides

Use `--config` (or `-c`) to select a configuration file; `FLUX_CONFIG` is supported as an environment-variable fallback. Command-line overrides take precedence over YAML values:

```bash
flux --config ./load-test.yaml --concurrency 50 --duration 60s \
  --output-json results.json --output-html report.html --output-csv requests.csv
```

Run `flux --help` for the complete flag reference.

---

## ⏱️ Duration and Ramp-up

`duration` is the measured load window and it starts once every worker is
running. `ramp_up` is warm-up time added *in front* of it, so a run takes
about `ramp_up + duration` in total:

```yaml
concurrency: 10
ramp_up: "10s"     # a worker starts every second
duration: "10s"    # each worker then has its own full 10 seconds
```

The run above takes roughly 19 seconds of wall clock (nine one-second gaps plus
the ten-second window). Every worker gets a full-length active period no matter
when it started — previously the last workers could be started with no time
left and never send a single request.

Because ramp-up is warm-up, `ramp_up` may be longer than `duration`, and the
reports separate the two:

- `total_duration_secs` — wall clock for the whole run, ramp-up included;
- `ramp_up_secs` — time spent starting workers;
- `measured_duration_secs` / `measured_requests` — the full-concurrency window;
- `throughput_rps` — requests per second over the measured window only.

## 🛑 Stopping a Test Early

`SIGINT` (Ctrl+C) and `SIGTERM` both cancel a running test immediately and take
the same shutdown path:

- no new workers are started and no new requests are issued;
- ramp-up pauses, think time, retry delays, and the sync-mode pacing delay wake
  up right away instead of waiting out their configured duration;
- in-flight requests are awaited, or end through the configured `timeout`;
- the terminal summary and the JSON/HTML/CSV reports are still generated from
  the requests that completed before cancellation.

Because the run stops early, the reported duration and throughput cover only the
elapsed part of the test.

```bash
docker run --rm --name flux-run ... flux:latest
# in another shell
docker stop flux-run     # SIGTERM: reports are still written
```

## 📈 Metrics Collected

Flux collects comprehensive metrics for each request:

- **Latency** (min, max, mean, p50, p90, p95, p99)
- **Throughput** (requests per second)
- **Status codes** distribution
- **Error rate** and error messages
- **Request timestamps** for timeline analysis

---

## 📄 Reports

### JSON Report

Contains full raw data and summary statistics:

```json
{
  "summary": {
    "total_requests": 12430,
    "successful_requests": 12002,
    "failed_requests": 428,
    "throughput_rps": 414.33,
    "p50_latency_ms": 84,
    "p90_latency_ms": 152,
    "p99_latency_ms": 231,
    "error_rate": 3.44
  },
  "results": [...]
}
```

### HTML Report

Beautiful interactive report with:
- Summary statistics cards
- Latency distribution histogram
- Latency over time line chart
- Status code distribution pie chart
- Percentiles table

---

## 🎯 Execution Modes

### Async Mode (Default)

Uses Tokio for maximum concurrency. Recommended for most use cases.

```yaml
mode: "async"
concurrency: 100
```

### Sync Mode

Blocking workers with controlled request rate. Useful for testing rate limiting.

```yaml
mode: "sync"
concurrency: 10
```

---

## 🐳 Docker Usage

### Basic Usage

```bash
docker run --rm \
  -v ./config.yaml:/app/config.yaml \
  -v ./data:/app/data \
  -v ./results:/app/results \
  flux:latest
```

### With Custom Logging

```bash
docker run --rm \
  -e RUST_LOG=debug \
  -v ./config.yaml:/app/config.yaml \
  -v ./data:/app/data \
  -v ./results:/app/results \
  flux:latest
```

### Volume Mounts

- `/app/config.yaml` - Configuration file (required)
- `/app/data` - Directory for multipart files (optional)
- `/app/results` - Directory for output reports (required)

---

## 🔧 Building from Source

### Prerequisites

- Rust
- Docker (for containerized builds)

### Local Build

```bash
cargo build --release
./target/release/flux
```

### Docker Build

```bash
docker build -t flux:latest .
```

---

## 📝 Examples

See the `samples/` directory for complete examples:

- `simple-get.yaml` - Basic GET request
- `simple-post.yaml` - POST with JSON body
- `multipart-upload.yaml` - File upload with multipart
- `scenario-auth.yaml` - Multi-step authentication flow

---

## 🛠️ Development

### Project Structure

```
flux/
├── src/
│   ├── main.rs              # Entry point and orchestration
│   ├── cancel.rs            # Cooperative cancellation token
│   ├── config.rs            # YAML configuration parsing
│   ├── client.rs            # HTTP client wrapper
│   ├── executor.rs          # Load test execution engine
│   ├── metrics.rs           # Metrics collection
│   ├── reporter.rs          # Report generation
│   ├── ui.rs                # Terminal UI
│   └── templates/
│       └── report.html      # HTML report template
├── samples/
│   ├── simple-get.yaml      # GET example
│   ├── simple-post.yaml     # POST example
│   ├── multipart-upload.yaml # Upload example
│   ├── scenario-auth.yaml   # Scenario example
│   └── sample.txt           # Sample file
├── data/                    # Directory for multipart files
├── results/                 # Directory for output reports
├── Cargo.toml               # Rust dependencies
├── Cargo.lock               # Dependency lock file
├── Dockerfile               # Container image definition
├── Makefile                 # Build and development commands
├── build.sh                 # Build script
├── run-example.sh           # Run script
├── config.yaml              # Default configuration
├── README.md                # This file
├── IMPLEMENTATION.md        # Implementation details
└── QUICKSTART.md            # Quick start guide
```

For detailed implementation information, architecture, and technical decisions, see [IMPLEMENTATION.md](IMPLEMENTATION.md).

### Running Tests

```bash
cargo test
```

### Code Style

```bash
cargo fmt
cargo clippy
```

---

## 🤝 Contributing

Contributions are welcome! Please ensure:

1. Code follows Rust best practices
2. All tests pass
3. Documentation is updated
4. Commit messages are clear

---

## 💡 Tips

1. **Start small**: Begin with low concurrency and short duration
2. **Monitor resources**: Watch CPU and memory usage
3. **Use async mode**: For maximum throughput
4. **Check reports**: HTML reports provide visual insights
5. **Test locally first**: Validate config before production testing

---

**Built with ❤️ using Rust**
