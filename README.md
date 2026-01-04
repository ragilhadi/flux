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
mode: "async"

output:
  json: "/app/results/output.json"
  html: "/app/results/report.html"
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
| `name` | string | Yes | Step name |
| `method` | string | Yes | HTTP method |
| `url` | string | Yes | URL path or full URL |
| `headers` | map | No | HTTP headers |
| `body` | string | No | Request body |
| `multipart` | array | No | Multipart form data |
| `extract` | map | No | JSONPath extraction rules |
| `depends_on` | string | No | Name of step this depends on |

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

---

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
