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
- **Load profiles**: fixed concurrency, staged ramps/spikes, or a target arrival rate
- **Multi-step scenarios** with variable extraction
- **Multipart form-data** with file upload support
- **JSON + HTML reports** with beautiful charts
- **Report comparison** with regression budgets for CI (`flux compare`)
- **Real-time terminal display** with progress bars
- **Opt-in live web dashboard** for watching a run from a browser
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

The endpoint has no authentication, so it binds to loopback (`127.0.0.1`) by
default. Set `prometheus_bind` to reach it from elsewhere:

```yaml
prometheus_port: 9090
prometheus_bind: "0.0.0.0" # reachable beyond this host; restrict access at the network level
```

Scrape `http://localhost:9090/metrics` for success and failure counters, latency
quantiles, and the current request rate:

```bash
curl http://localhost:9090/metrics
```

### Live Web Dashboard

Add a `live_dashboard` section to watch a run from a browser. It is opt-in:
without the section Flux opens no socket at all.

```yaml
live_dashboard:
  bind: "127.0.0.1:9090"   # loopback by default
  refresh_ms: 1000         # poll interval used by the page
  redact:                  # extra values to scrub from the dashboard
    - "an-extra-secret"
```

Open `http://localhost:9090/` while the test runs. The page shows current RPS,
error rate, mean and p50/p95/p99 latency, throughput and latency trends for the
last two minutes, the response-status distribution, a per-scenario table
(including skipped steps), and the most recent failures.

| Endpoint | Description |
|----------|-------------|
| `GET /` | Self-contained dashboard page (no external assets) |
| `GET /api/summary` | Current aggregate and per-scenario metrics, plus the run status |
| `GET /api/recent-results` | Bounded, redacted sample of recent requests and failures |
| `GET /healthz` | Run status (`running`, `completed`, `cancelled`), uptime, request count |

```bash
curl http://localhost:9090/api/summary
curl http://localhost:9090/healthz
```

The page polls those endpoints every `refresh_ms` milliseconds. The server
stops as soon as the run ends or is cancelled, so a closed port means the test
is over — the JSON, HTML and CSV reports are still written as usual.

#### Docker Usage

Inside a container, loopback means the container's own loopback, so the
dashboard must bind `0.0.0.0` and the port must be published:

```yaml
live_dashboard:
  bind: "0.0.0.0:9090"
```

```bash
docker run --rm \
  -p 127.0.0.1:9090:9090 \
  -v ./config.yaml:/app/config.yaml \
  -v ./results:/app/results \
  flux:latest
```

Publishing as `-p 127.0.0.1:9090:9090` keeps the dashboard reachable from your
machine only. `-p 9090:9090` binds every host interface instead.

#### Network Exposure and Redaction

The dashboard is unauthenticated, read-only HTTP. Anyone who can reach the port
can read the metrics of the running test, so:

- keep the default loopback bind, or publish the port to a trusted network only;
- Flux logs a warning whenever `bind` is not a loopback address;
- only `GET` is accepted, and unknown paths return 404.

What the dashboard can show is deliberately narrow. It never serves request or
response bodies, headers or cookies — the recent-request rows carry a scenario
name, status code, latency, timestamp and the error message. Error messages are
redacted before they leave the process:

- values of credential headers (`Authorization`, `Cookie`, `X-Api-Key`, and
  similar), request bodies and multipart field values from the configuration;
- `Bearer` tokens, `user:password@` URL credentials and sensitive query
  parameters (`token`, `api_key`, `password`, …), which covers secrets that only
  exist at runtime, such as a token extracted from a response;
- anything listed under `live_dashboard.redact`.

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

### Staged Load Profile (Ramp / Hold / Spike)

Ramp concurrency through a sequence of levels instead of a single fixed
value. Each stage is held for its own duration, so this models a gradual
ramp, a hold period, a ramp-down, or — with a short high-concurrency stage —
a traffic spike:

```yaml
target: "https://api.example.com/health"

load_profile:
  type: stages
  stages:
    - duration: "30s"
      target_concurrency: 10   # warm up
    - duration: "2m"
      target_concurrency: 100  # hold at peak
    - duration: "15s"
      target_concurrency: 300  # spike
    - duration: "30s"
      target_concurrency: 0    # ramp down

output:
  json: "/app/results/staged.json"
  html: "/app/results/staged.html"
```

### Arrival-Rate Load Profile

Pace request starts at a fixed target rate instead of driving a fixed
worker count, bounded by `max_concurrency` in-flight requests:

```yaml
target: "https://api.example.com/health"

load_profile:
  type: arrival_rate
  target_rps: 200
  duration: "5m"
  max_concurrency: 500  # optional; defaults to 1000

output:
  json: "/app/results/arrival-rate.json"
  html: "/app/results/arrival-rate.html"
```

See [Load Profiles](#-load-profiles) below for how staging and arrival-rate
pacing work, and what shows up in the reports.

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
| `load_profile` | object | No | - | Staged or arrival-rate load profile; mutually exclusive with `ramp_up` |
| `think_time` | string | No | - | Pause after each simple-mode request |
| `retry_count` | integer | No | 0 | Retry attempts after the initial request |
| `retry_delay` | string | No | 0s | Pause between retry attempts |
| `retry_on_status` | array | No | [] | HTTP status codes eligible for retry |
| `assertions` | object | No | - | Aggregate error-rate and latency quality gates |
| `prometheus_port` | integer | No | - | Port for the live Prometheus metrics endpoint |
| `prometheus_bind` | string | No | 127.0.0.1 | Address the Prometheus endpoint binds to |
| `live_dashboard` | object | No | - | Live web dashboard; no port is opened unless set |
| `mode` | string | No | async | Execution mode: "async" or "sync" |
| `output` | object | Yes | - | Output configuration |

### Load Profile

`load_profile.type` is either `stages` or `arrival_rate`.

**`stages`**

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `stages` | array | Yes | At least one stage, held in order |
| `stages[].duration` | string | Yes | How long this stage is held (e.g. "30s", "2m") |
| `stages[].target_concurrency` | integer | Yes | Worker count active during this stage (`0` allowed, for a ramp-down) |

**`arrival_rate`**

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `target_rps` | number | Yes | - | Target requests started per second |
| `duration` | string | Yes | - | How long the profile runs |
| `max_concurrency` | integer | No | 1000 | Maximum in-flight requests |

### Live Dashboard

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `bind` | string | No | 127.0.0.1:9090 | Listen address as `ip:port` |
| `refresh_ms` | integer | No | 1000 | Page poll interval, between 100 and 60000 |
| `redact` | array | No | [] | Extra literal values scrubbed from the dashboard |

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

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `json` | string | Yes | - | JSON report path |
| `html` | string | Yes | - | HTML report path |
| `csv` | string | No | - | Per-request CSV path, streamed to disk while the test runs |
| `max_results` | integer | No | 10000 | Per-request rows kept in memory for the JSON/HTML reports (`0` keeps all) |

#### Memory Use on Long Runs

Aggregate statistics — totals, error rate, latency percentiles and the
per-scenario breakdown — are computed as each request completes, so they always
cover every request and use a fixed amount of memory.

Raw per-request rows are a different matter: they are what the JSON and HTML
reports embed, and keeping one for every request of a sustained high-throughput
run (10,000 req/s for an hour is 36 million rows) can exhaust the container and
invalidate the test. `output.max_results` therefore caps how many rows are kept,
at 10,000 by default. When the cap is reached:

- the summary reports `retained_results` and `dropped_results`, and both the
  terminal output and the HTML report say the rows are a sample;
- totals, error rate, percentiles and per-scenario statistics are unaffected.

Set `max_results: 0` to keep every row, and be explicit about the memory that
implies.

If you need every request row, use `output.csv`: rows are streamed to the file
as they are produced, so the CSV is complete regardless of `max_results` and
without holding the rows in memory.

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

Run `flux --help` for the complete flag reference, and `flux compare --help`
for the report comparison subcommand described in
[Comparing Reports and Regression Budgets](#-comparing-reports-and-regression-budgets).

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

## 🌊 Load Profiles

`concurrency` (with optional `ramp_up`) models a fixed number of workers. For
traffic shapes that isn't enough — a stepped ramp, a hold period, a spike, or
a target requests-per-second — configure `load_profile` instead. It replaces
`concurrency`, `duration` and `ramp_up`: the profile's own stages or
arrival-rate duration decide how long the run takes, and `--concurrency` /
`--duration` command-line overrides are rejected when `load_profile` is set,
so timing always comes from one place.

### Stages

```yaml
load_profile:
  type: stages
  stages:
    - duration: "30s"
      target_concurrency: 10
    - duration: "2m"
      target_concurrency: 100
    - duration: "30s"
      target_concurrency: 0
```

Every worker any stage could need is started up front; a worker outside the
active stage's `target_concurrency` idles instead of sending requests. That
keeps stage transitions predictable — concurrency changes on schedule with no
worker-startup latency mid-run — while still using ordinary worker loops
underneath, so retries, think time, scenarios and cancellation all behave
exactly as they do for fixed concurrency.

### Arrival Rate

```yaml
load_profile:
  type: arrival_rate
  target_rps: 200
  duration: "5m"
  max_concurrency: 500
```

Request starts are paced at `target_rps` — independent of how long each
request takes — rather than driven by a worker loop. In-flight requests are
bounded by `max_concurrency`: when the target rate outpaces what the backend
can sustain, a pacing tick that finds no free slot is counted as **saturated**
and skipped rather than queued, so an overloaded target shows up as a number
in the report instead of unbounded pending work.

### What Shows Up in Reports

Both profiles add to the terminal summary, the JSON report and the HTML
report:

- `load_profile` — the profile's `kind`, plus `target_rps` / `achieved_rps`
  and `scheduled_ticks` / `saturated_ticks` for `arrival_rate`;
- `stages` — one entry per stage (or one entry for the whole arrival-rate
  window) with its configured target, planned duration, and the usual
  request/latency/error metrics observed while it was active.

A plain fixed-concurrency run has no `load_profile` and an empty `stages`
list, so existing reports and tooling are unaffected.

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
- **Retention counters** (`retained_results` / `dropped_results`) when raw rows are capped

---

## 📄 Reports

### JSON Report

Contains full raw data and summary statistics:

```json
{
  "schema_version": 1,
  "summary": {
    "total_requests": 12430,
    "successful_requests": 12002,
    "failed_requests": 428,
    "throughput_rps": 414.33,
    "p50_latency_ms": 84,
    "p90_latency_ms": 152,
    "p99_latency_ms": 231,
    "error_rate": 3.44,
    "status_codes": { "0": 12, "200": 12002, "500": 416 },
    "per_scenario": { "...": {} }
  },
  "results": [...]
}
```

`schema_version` records the layout of the document so `flux compare` knows
what it is reading. Reports written before versioning existed carry no such
field and are read as schema `v0`; they still compare, minus anything they did
not record. `status_codes` counts every request by response status, with `0`
for requests that never got a response (connection errors, timeouts).

### HTML Report

Beautiful interactive report with:
- Summary statistics cards
- Latency distribution histogram
- Latency over time line chart
- Status code distribution pie chart
- Percentiles table

---

## 🆚 Comparing Reports and Regression Budgets

`flux compare` reads two JSON reports that already exist and reports what
changed between them. It sends no traffic, needs no configuration file, and
exits non-zero when a configured regression budget is exceeded — which is what
makes it usable as a CI gate.

```bash
flux --config perf.yaml --output-json artifacts/current.json

flux compare artifacts/baseline.json artifacts/current.json \
  --max-p95-regression 10% --max-error-rate-increase 0.5
```

The comparison covers request counts, throughput, error rate, mean and
p50/p90/p95/p99 latency, and the status-code distribution — aggregate first,
then every scenario present in both reports:

```
Aggregate Deltas:
  Metric                       Baseline      Candidate          Delta           Change       Budget
  total requests                   1000           1000              0             0.0%            -
  throughput                33.30 req/s    30.00 req/s    -3.30 req/s            -9.9%            -
  error rate                      0.20%          1.10%        +0.90pp          +450.0%        0.5pp
  p95 latency                     110ms          140ms          +30ms           +27.3%          10%

Status Code Distribution:
  Status         Baseline    Candidate      Delta     Base %     Cand %
  none                  0            1         +1       0.0%       0.1%
  200                 998          989         -9      99.8%      98.9%
  500                   2           10         +8       0.2%       1.0%

❌ FAIL — regression budgets exceeded:
  • aggregate error rate: +0.90pp exceeds the 0.5pp increase budget
  • aggregate p95 latency: +30ms (+27.3%) exceeds the 10% increase budget
```

### Budgets

| Flag | Budget on | Accepts |
|------|-----------|---------|
| `--max-mean-regression` | increase in mean latency | `10%` or `25` / `25ms` |
| `--max-p50-regression` | increase in p50 latency | `10%` or `25` / `25ms` |
| `--max-p90-regression` | increase in p90 latency | `10%` or `25` / `25ms` |
| `--max-p95-regression` | increase in p95 latency | `10%` or `25` / `25ms` |
| `--max-p99-regression` | increase in p99 latency | `10%` or `25` / `25ms` |
| `--max-throughput-drop` | drop in throughput | `10%` or `5` / `5 req/s` |
| `--max-error-rate-increase` | increase in error rate | percentage points, e.g. `0.5` |
| `--per-scenario-budgets` | applies the same budgets to each shared scenario | flag |

A budget ending in `%` is relative to the baseline; anything else is absolute,
in the metric's own unit. Error rate is itself a percentage, so its budget is
always in **percentage points** (`0.5` means 1.0% → 1.5% fails); a `%` sign
there is rejected rather than guessed at.

Only budgets you pass are enforced. With none, the comparison prints the deltas
and exits `0`.

### Denominators, added and removed scenarios

- Percentage change always uses the **baseline** as the denominator. When the
  baseline value is `0` there is no denominator, so the change is printed as
  `n/a (baseline 0)` rather than a fabricated number — and a percentage budget
  treats any movement in the bad direction from a zero baseline as exceeded.
- Status-code shares are a percentage of that report's own total requests, and
  print as `n/a` when a run made no requests at all.
- Scenarios are matched by name. Scenarios only in the candidate are listed as
  **added** (no delta exists), scenarios only in the baseline as **removed**.
  Per-scenario budgets apply only to scenarios present in both.
- If either report predates status recording, the status table is skipped with
  a note instead of reporting every status as new.

### Exit codes

| Code | Meaning |
|------|---------|
| `0` | every configured budget met (or none configured) |
| `1` | at least one budget exceeded |
| `2` | the comparison could not be made (missing/unreadable report, unparseable budget, report written by a newer schema) |

### CI artifacts

```bash
flux compare baseline.json current.json \
  --max-p95-regression 10% \
  --output-json comparison.json \
  --output-markdown comparison.md
```

`--output-json` writes the full comparison (every delta, the budgets, and each
violation) for later processing; `--output-markdown` writes tables ready to
paste into a job summary or a PR comment. Both are written before the exit code
is decided, so they exist even when the run fails.

### GitHub Actions example

```yaml
name: Performance

on: pull_request

jobs:
  load-test:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4

      - name: Restore the performance baseline
        uses: actions/cache@v4
        with:
          path: artifacts/baseline.json
          key: flux-baseline-${{ hashFiles('perf.yaml') }}

      - name: Run the load test
        run: |
          mkdir -p artifacts
          docker run --rm \
            -v ${{ github.workspace }}/perf.yaml:/app/config.yaml \
            -v ${{ github.workspace }}/artifacts:/app/results \
            ragilhadi/flux:latest --output-json /app/results/current.json

      - name: Compare against the baseline
        if: hashFiles('artifacts/baseline.json') != ''
        run: |
          docker run --rm \
            -v ${{ github.workspace }}/artifacts:/artifacts \
            ragilhadi/flux:latest compare \
              /artifacts/baseline.json /artifacts/current.json \
              --max-p95-regression 10% \
              --max-error-rate-increase 0.5 \
              --output-markdown /artifacts/comparison.md

      - name: Publish the comparison
        if: always() && hashFiles('artifacts/comparison.md') != ''
        run: cat artifacts/comparison.md >> $GITHUB_STEP_SUMMARY

      - uses: actions/upload-artifact@v4
        if: always()
        with:
          name: flux-performance
          path: artifacts/
```

The compare step fails the job on exit code `1`, and the summary shows which
budget was exceeded.

### Read the result honestly

Two runs of the same test never produce the same numbers: a shared CI runner
adds far more variance than most code changes do. A comparison is an
**operational budget check, not a statistical test**, and Flux prints that
caveat with every comparison.

To keep it meaningful:

- record the baseline on the same class of machine as the candidate, and
  refresh it when the environment changes;
- run long enough that percentiles settle — seconds-long runs mostly measure
  noise;
- set budgets wide enough to clear the noise you observe between two unchanged
  runs, then tighten them;
- re-run before acting on a result that lands close to its budget, rather than
  reading a single close call as proof of a regression.

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
- `live-dashboard.yaml` - Run with the live web dashboard enabled

---

## 🛠️ Development

### Project Structure

```
flux/
├── src/
│   ├── main.rs              # Entry point and orchestration
│   ├── cancel.rs            # Cooperative cancellation token
│   ├── compare.rs           # Report comparison and regression budgets
│   ├── config.rs            # YAML configuration parsing
│   ├── client.rs            # HTTP client wrapper
│   ├── dashboard.rs         # Opt-in live web dashboard
│   ├── executor.rs          # Load test execution engine
│   ├── metrics.rs           # Metrics collection
│   ├── prometheus.rs        # Live Prometheus endpoint
│   ├── redact.rs            # Secret redaction for live output
│   ├── reporter.rs          # Report generation
│   ├── ui.rs                # Terminal UI
│   └── templates/
│       ├── dashboard.html   # Live dashboard page
│       └── report.html      # HTML report template
├── samples/
│   ├── simple-get.yaml      # GET example
│   ├── simple-post.yaml     # POST example
│   ├── multipart-upload.yaml # Upload example
│   ├── scenario-auth.yaml   # Scenario example
│   ├── live-dashboard.yaml  # Live dashboard example
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
