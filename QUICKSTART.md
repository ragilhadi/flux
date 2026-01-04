# ⚡ Flux Quick Start Guide

Get started with Flux in 5 minutes!

## Prerequisites

- Docker installed and running
- Basic understanding of HTTP APIs

## Step 1: Build the Docker Image

```bash
cd flux
./build.sh
```

Or manually:
```bash
docker build -t flux:latest .
```

## Step 2: Prepare Your Environment

Create the required directories:

```bash
mkdir -p data results
```

## Step 3: Choose a Configuration

### Option A: Use a Sample Configuration

We provide several ready-to-use examples:

```bash
# Simple GET request
./run-example.sh samples/simple-get.yaml

# POST with JSON body
./run-example.sh samples/simple-post.yaml

# Multi-step authentication scenario
./run-example.sh samples/scenario-auth.yaml
```

### Option B: Create Your Own Configuration

Create `config.yaml`:

```yaml
target: "https://jsonplaceholder.typicode.com/posts"
method: "GET"

headers:
  Accept: "application/json"

concurrency: 10
duration: "10s"
mode: "async"

output:
  json: "/app/results/output.json"
  html: "/app/results/report.html"
```

## Step 4: Run Your First Load Test

```bash
docker run --rm \
  -v $(pwd)/config.yaml:/app/config.yaml \
  -v $(pwd)/data:/app/data \
  -v $(pwd)/results:/app/results \
  flux:latest
```

## Step 5: View the Results

### Terminal Output

You'll see real-time progress:

```
═══════════════════════════════════════════════════════════════════
⚡ Flux Load Test Started
═══════════════════════════════════════════════════════════════════
Target               : https://jsonplaceholder.typicode.com/posts
Concurrency          : 10 workers
Duration             : 10s
Mode                 : ASYNC
═══════════════════════════════════════════════════════════════════

 [00:00:05] [████████████████████░░░░░░░░░░░░] 5/10s (5s)
RPS: 245 | Avg Latency: 42ms | Errors: 0 (0.0%)
```

### HTML Report

Open `results/report.html` in your browser to see:
- Beautiful charts and graphs
- Latency distribution
- Status code breakdown
- Detailed statistics

### JSON Report

The `results/output.json` contains raw data for further analysis.

## Common Use Cases

### 1. Test a REST API Endpoint

```yaml
target: "https://api.example.com/users"
method: "GET"
headers:
  Authorization: "Bearer YOUR_TOKEN"
concurrency: 20
duration: "30s"
mode: "async"
output:
  json: "/app/results/api-test.json"
  html: "/app/results/api-test.html"
```

### 2. Upload Files

```yaml
target: "https://api.example.com/upload"
method: "POST"
multipart:
  - type: "file"
    name: "document"
    path: "/app/data/myfile.pdf"
  - type: "field"
    name: "description"
    value: "Test upload"
concurrency: 5
duration: "15s"
mode: "async"
output:
  json: "/app/results/upload-test.json"
  html: "/app/results/upload-test.html"
```

### 3. Test Authentication Flow

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
  
  - name: "get-profile"
    method: "GET"
    url: "/profile"
    headers:
      Authorization: "Bearer {{ token }}"
    depends_on: "login"

concurrency: 10
duration: "20s"
mode: "async"
output:
  json: "/app/results/auth-test.json"
  html: "/app/results/auth-test.html"
```

## Tips for Success

### 1. Start Small

Begin with low concurrency and short duration:
```yaml
concurrency: 5
duration: "10s"
```

Then gradually increase to find your system's limits.

### 2. Monitor Your System

Watch CPU and memory usage:
```bash
# In another terminal
docker stats
```

### 3. Use Async Mode

For maximum throughput, use async mode:
```yaml
mode: "async"
```

### 4. Check the HTML Report

The HTML report provides visual insights that are easier to understand than raw numbers.

### 5. Test Locally First

Always test your configuration locally before running against production systems.

## Troubleshooting

### Issue: "Failed to load configuration"

**Solution**: Check that your YAML syntax is correct:
```bash
# Validate YAML syntax
python3 -c "import yaml; yaml.safe_load(open('config.yaml'))"
```

### Issue: "File not found" for multipart uploads

**Solution**: Ensure files are in the `data/` directory:
```bash
ls -la data/
```

### Issue: Connection refused

**Solution**: Check that the target URL is accessible:
```bash
curl -I https://your-target-url.com
```

### Issue: High error rate

**Solution**: 
1. Reduce concurrency
2. Increase duration to spread load
3. Check target server capacity

## Next Steps

1. **Read the full README**: `README.md` for detailed documentation
2. **Explore samples**: Check `samples/` directory for more examples
3. **Customize reports**: Modify `src/templates/report.html` if needed
4. **Scale up**: Increase concurrency and duration for real load tests

## Getting Help

- Check `README.md` for detailed documentation
- Review `IMPLEMENTATION.md` for technical details
- Examine sample configurations in `samples/`

## Example Session

Here's a complete example session:

```bash
# 1. Build
./build.sh

# 2. Create config
cat > config.yaml << 'EOF'
target: "https://jsonplaceholder.typicode.com/posts/1"
method: "GET"
concurrency: 10
duration: "10s"
mode: "async"
output:
  json: "/app/results/test.json"
  html: "/app/results/test.html"
EOF

# 3. Create directories
mkdir -p data results

# 4. Run test
docker run --rm \
  -v $(pwd)/config.yaml:/app/config.yaml \
  -v $(pwd)/data:/app/data \
  -v $(pwd)/results:/app/results \
  flux:latest

# 5. View results
open results/test.html  # macOS
xdg-open results/test.html  # Linux
```

## Performance Expectations

On modern hardware (4 cores, 8GB RAM), you can expect:

- **Simple GET**: 5,000-10,000 RPS
- **POST with JSON**: 3,000-8,000 RPS
- **Multipart uploads**: 500-2,000 RPS
- **Multi-step scenarios**: 1,000-5,000 RPS

Actual performance depends on:
- Network latency
- Target server capacity
- Request/response size
- System resources

---

**Happy Load Testing! ⚡**
