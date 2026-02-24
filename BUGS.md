# Bug Reports

This document lists known bugs in the Flux load testing tool that need to be fixed.
Each entry includes the issue location, a detailed description, steps to reproduce (where applicable), and success criteria.

---

## BUG-01: URL Variable Substitution Missing in Scenario Steps

**Severity:** High  
**File:** `src/client.rs` – `execute_scenario()`  
**Lines:** 61–67

### Description

When running a multi-step scenario, template variables such as `{{ user_id }}` or `{{ token }}` are correctly substituted in request **headers** and **body**, but they are **never substituted in the URL path**. The URL is constructed by joining the base URL with the scenario's `url` field before any variable substitution takes place.

### Example

Given this scenario configuration:

```yaml
scenarios:
  - name: "login"
    method: "POST"
    url: "/auth/login"
    extract:
      user_id: "$.user.id"

  - name: "get-profile"
    method: "GET"
    url: "/users/{{ user_id }}/profile"
    depends_on: "login"
```

The second step sends the literal request `GET /users/{{ user_id }}/profile` instead of resolving the extracted variable, resulting in a 404 or routing error on the server.

### Root Cause

In `execute_scenario()`, the URL is assembled without calling `substitute_variables()`:

```rust
let url = if scenario.url.starts_with("http://") || scenario.url.starts_with("https://") {
    scenario.url.clone()               // no substitution
} else if let Some(base) = base_url {
    format!("{}{}", base.trim_end_matches('/'), scenario.url)  // no substitution
} else {
    scenario.url.clone()               // no substitution
};
```

### What Needs to Be Fixed

The scenario URL must be passed through `substitute_variables()` before it is combined with the base URL. The fix is a one-line change:

```rust
let raw_url = self.substitute_variables(&scenario.url, variables);
let url = if raw_url.starts_with("http://") || raw_url.starts_with("https://") {
    raw_url
} else if let Some(base) = base_url {
    format!("{}{}", base.trim_end_matches('/'), raw_url)
} else {
    raw_url
};
```

### Success Criteria

- A scenario step with `{{ variable }}` placeholders in its URL path receives the extracted value in the actual HTTP request URL.
- An existing unit test (or a new one) verifies that variable substitution is applied to the URL.
- All existing tests continue to pass.

---

## BUG-02: Dependency Tracking in `has_executed_scenario` Uses Wrong Heuristic

**Severity:** High  
**File:** `src/executor.rs` – `has_executed_scenario()`  
**Lines:** 275–283

### Description

`has_executed_scenario()` is supposed to determine whether a named predecessor step has already been executed, so that dependent steps can be skipped when the dependency has not run. Currently the implementation ignores the `scenario_name` parameter entirely and only checks whether the `variables` map is non-empty:

```rust
fn has_executed_scenario(
    &self,
    _scenario_name: &str,
    variables: &HashMap<String, String>,
) -> bool {
    // Simple heuristic: if we have variables, assume dependencies are met
    !variables.is_empty()
}
```

This causes two classes of failures:

1. **False negative:** A scenario step that extracted no variables (i.e., it has an empty `extract` map) leaves `variables` empty. Any step that depends on it is incorrectly skipped, even though the predecessor ran successfully.
2. **False positive:** If *any* earlier step happened to extract variables, a dependent step may be considered eligible even though *its specific* declared dependency did not run.

### What Needs to Be Fixed

Track which scenario names have been successfully executed in a `HashSet<String>` and pass it alongside `variables`. Replace the heuristic check with an explicit membership test:

```rust
fn has_executed_scenario(
    scenario_name: &str,
    executed: &HashSet<String>,
) -> bool {
    executed.contains(scenario_name)
}
```

After each scenario step executes successfully, insert its name into the set before advancing to the next step.

### Success Criteria

- A scenario whose predecessor has **no** `extract` declarations still runs correctly when that predecessor succeeds.
- A scenario is correctly **skipped** when its declared `depends_on` step did not run (e.g., because the step before it failed).
- All existing tests continue to pass.

---

## BUG-03: `run_sync` Does Not Join Worker Tasks Before Returning

**Severity:** Medium  
**File:** `src/executor.rs` – `run_sync()`  
**Lines:** 72–92

### Description

In `run_sync`, worker tasks are spawned with `tokio::spawn` but the function returns as soon as `sleep(duration).await` completes. It does not join (await) the spawned task handles. Workers that are mid-request when the sleep expires will still be running, but their results may be lost or recorded after `generate_summary()` has already been called from `main.rs`.

```rust
async fn run_sync(&self, start: Instant, duration: Duration) -> Result<()> {
    for worker_id in 0..self.config.concurrency {
        // ...
        tokio::spawn(async move { ... });  // handle is discarded
        sleep(Duration::from_millis(10)).await;
    }
    sleep(duration).await;  // returns here; tasks may still be running
    Ok(())
}
```

`run_async`, by contrast, collects all handles in a `Vec` and awaits each one before returning.

### What Needs to Be Fixed

Collect the `JoinHandle` values returned by `tokio::spawn` and await them after the `sleep`, matching the pattern already used in `run_async`:

```rust
async fn run_sync(&self, start: Instant, duration: Duration) -> Result<()> {
    let mut handles = vec![];
    for worker_id in 0..self.config.concurrency {
        // ...
        let handle = tokio::spawn(async move { ... });
        handles.push(handle);
        sleep(Duration::from_millis(10)).await;
    }
    sleep(duration).await;
    for handle in handles {
        let _ = handle.await;
    }
    Ok(())
}
```

### Success Criteria

- In sync mode, all in-flight requests that started before the duration elapsed are recorded in the final metrics.
- No metrics are lost between `executor.run()` returning and `metrics.generate_summary()` being called.
- All existing tests continue to pass.

---

## BUG-04: Histogram Silently Drops Latencies Above 60 000 ms

**Severity:** Medium  
**File:** `src/metrics.rs` – `MetricsCollector::new()` and `record()`  
**Lines:** 57–79

### Description

The HDR histogram is created with a hard maximum of 60 000 ms (60 seconds):

```rust
Histogram::<u64>::new_with_bounds(1, 60_000, 3).unwrap()
```

When a request takes longer than 60 seconds (e.g., against a slow or unresponsive server), `hist.record(latency)` returns an `Err` because the value is out of range. The return value is discarded with `let _ = ...`, so the latency is silently excluded from all percentile calculations. The raw result is still stored in `results`, but percentile values (p50, p90, p99, max) will be computed incorrectly.

### What Needs to Be Fixed

Either:
- Increase the histogram upper bound to a value that covers realistic worst-case latencies (e.g., the configured request timeout + a safety margin), or
- Clamp latency values to the histogram maximum before recording and emit a `warn!` log when clamping occurs.

The `record()` method should also log a warning instead of silently discarding histogram errors:

```rust
if let Err(e) = hist.record(latency.min(60_000)) {
    warn!("Histogram record error for latency {}ms: {}", latency, e);
}
```

### Success Criteria

- Latency values up to the configured request timeout (default 30 s) are recorded without error.
- Any latency that cannot be recorded triggers a `warn!` log message rather than being silently dropped.
- Percentile calculations remain accurate for typical load test durations.
- All existing tests continue to pass.

---

## BUG-05: Config File Path Is Hardcoded to `/app/config.yaml`

**Severity:** Medium  
**File:** `src/main.rs`  
**Line:** 36

### Description

The path to the configuration file is hardcoded:

```rust
let config_path = PathBuf::from("/app/config.yaml");
```

This forces users to mount their configuration at exactly that path. Running the binary outside Docker (e.g., during local development or CI) requires either creating a symlink, rebuilding the binary, or mounting a Docker volume at that exact path. There is no way to pass a custom path.

### What Needs to Be Fixed

Support a configurable config path through:
1. A command-line argument (`--config <path>` or `-c <path>`).
2. As a fallback, an environment variable `FLUX_CONFIG` (or `CONFIG_PATH`).
3. If neither is provided, fall back to the current default `/app/config.yaml` to maintain backward compatibility.

A lightweight CLI argument parser already present in many Rust projects (e.g., `clap`) or simple `std::env::args()` parsing can be used.

### Success Criteria

- Running `flux --config ./my-config.yaml` loads the specified file.
- Setting `FLUX_CONFIG=./my-config.yaml` and running `flux` without arguments loads the specified file.
- When neither is provided, the tool falls back to `/app/config.yaml` (existing behavior).
- An appropriate error message is displayed when the specified file does not exist.
- All existing tests continue to pass.

---

## BUG-06: Only SIGTERM Is Handled; SIGINT (Ctrl+C) Is Not

**Severity:** Low  
**File:** `src/main.rs`  
**Lines:** 65–72

### Description

The graceful-shutdown handler only registers for `SIGTERM`:

```rust
let mut signals = Signals::new([SIGTERM]).expect("Failed to create signal handler");
```

When a user presses **Ctrl+C** in a terminal, the process receives `SIGINT`, not `SIGTERM`. Because `SIGINT` is not registered, the default Rust/OS handler terminates the process immediately, skipping final report generation and summary output.

### What Needs to Be Fixed

Add `SIGINT` to the list of handled signals:

```rust
use signal_hook::consts::{SIGINT, SIGTERM};
// ...
let mut signals = Signals::new([SIGTERM, SIGINT]).expect("Failed to create signal handler");
```

### Success Criteria

- Pressing Ctrl+C during a running test sets the `shutdown_flag`, allowing the executor to complete in-flight requests and generate reports.
- Both SIGTERM and SIGINT trigger the same clean-shutdown path.
- All existing tests continue to pass.

---

## BUG-07: HTTP Error Status Codes (4xx/5xx) Are Not Counted as Failures

**Severity:** Low  
**File:** `src/executor.rs` – `execute_simple_request()` and `execute_scenarios()`  
**Lines:** 133–155, 185–221

### Description

A `RequestResult` has `error: None` whenever the HTTP connection succeeded, regardless of the HTTP status code returned by the server. Requests returning 404, 500, 503, etc. are counted as `successful_requests` in `MetricsSummary`. This makes the error rate and success count metrics misleading when the target server is returning errors.

### What Needs to Be Fixed

Treat non-2xx responses as failures by setting `error` to a descriptive string when the status code indicates an error:

```rust
let error = if !response.status().is_success() {
    Some(format!("HTTP {}", response.status().as_u16()))
} else {
    None
};
```

The definition of "failure" could be made configurable (e.g., only 5xx, or any non-2xx), but as a minimum fix, 5xx responses should be flagged as errors.

### Success Criteria

- Requests with 5xx status codes are counted in `failed_requests` and contribute to `error_rate`.
- The existing `error` field semantics for network/connection failures are preserved.
- A unit test covers the case where a 500 response is recorded as a failure.
- All existing tests continue to pass.
