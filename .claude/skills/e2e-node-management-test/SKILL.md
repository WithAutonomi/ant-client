---
name: e2e-node-management-test
description: Run a comprehensive end-to-end test of all node management features against a real testnet. Builds the binary and exercises every implemented command and API endpoint.
allowed-tools: Bash, Read, Glob, Grep, AskUserQuestion
---

# End-to-End Node Management Test

This command runs a comprehensive end-to-end test of all node management features against a real
testnet. It builds the binary and exercises every implemented command and API endpoint.

## Prerequisites

A testnet must already be deployed and accessible before running this command. The user will provide
the bootstrap address when prompted.

## Critical Rule: Fail Fast

If **any** step in this test fails, **stop immediately** and report the failure. Do not attempt to
recover, retry, work around, or continue past a failure. The purpose is to detect failures, not mask
them. Report which step failed, what the expected outcome was, and what actually happened.

## Test Procedure

### Phase 1: Bootstrap address

Prompt the user to supply the bootstrap address for the testnet (e.g., `1.2.3.4:12000`). This is
mandatory — do not proceed without it. Store this for use in the `--bootstrap` argument later.

### Phase 2: Build

```bash
cargo build --bin ant
```

### Phase 3: Determine binary path and ant command

Determine the platform and set variables accordingly:
- **Linux/macOS**: binary is at `target/debug/ant`
- **Windows**: binary is at `target\debug\ant.exe`

For all subsequent `ant` commands, use the full path to the built binary (e.g.,
`./target/debug/ant`). Use the `--json` flag on every command to get structured output for
validation.

### Phase 4: Clean slate

Ensure we start from a clean state. Run:

```
ant node reset --force --json
```

This may fail if no nodes exist yet — that's OK. A "no nodes to reset" error (or similar) is
acceptable here. Any other error is a failure.

### Phase 5: Daemon lifecycle

**Step 5.1 — Start the daemon:**

```
ant node daemon start --json
```

Verify the JSON response contains `pid` (a number) and `already_running` is `false`.

**Step 5.2 — Check daemon status:**

```
ant node daemon status --json
```

Verify: `running` is `true`, `pid` is present, `port` is present and **non-zero** (the daemon binds
to an OS-assigned port, so port must be a real port number, not 0).

**Step 5.3 — Check daemon info:**

```
ant node daemon info --json
```

Verify: `running` is `true`, `api_base` contains a URL like `http://127.0.0.1:<port>/api/v1`.

Save the `api_base` URL for REST API checks later.

### Phase 6: Node management

**Step 6.1 — Add nodes:**

```
ant node add --rewards-address 0x03B770D9cD32077cC0bF330c13C114a87643B124 --count 3 --bootstrap <bootstrap-address> --json
```

Verify the JSON response shows 3 nodes were added (the `nodes_added` array has 3 entries).

**Step 6.2 — Start all nodes:**

Nodes are added in `stopped` state (add and start are distinct operations, as with most service
managers). Start all nodes before proceeding:

```
ant node start --json
```

Verify the JSON response has a `started` array with 3 entries and an empty `failed` array.

**Step 6.3 — Check node status (all running):**

```
ant node status --json
```

Verify: `nodes` array has 3 entries and all are `running`. For each running node, verify that `pid`
is present and is a number, and `uptime_secs` is present and is a number.

**Step 6.4 — Stop a single node:**

```
ant node stop --service-name node1 --json
```

Verify the JSON response shows `node_id` and `service_name` of `node1`.

**Step 6.5 — Check status (partial):**

```
ant node status --json
```

Verify: one node is `stopped`, two nodes are `running`. For the stopped node (`node1`), verify that
`pid` and `uptime_secs` are **not present** in the JSON (they are omitted via `skip_serializing_if`
when a node is stopped). For the two running nodes, verify that `pid` and `uptime_secs` are present
and are numbers.

**Step 6.6 — Start the stopped node:**

```
ant node start --service-name node1 --json
```

Verify the JSON response shows `node_id`, `service_name` of `node1`, and `pid`.

**Step 6.7 — Stop all nodes:**

```
ant node stop --json
```

Verify the JSON response has a `stopped` array with 3 entries and an empty `failed` array.

**Step 6.8 — Start all nodes:**

```
ant node start --json
```

Verify the JSON response has a `started` array with 3 entries and an empty `failed` array.

### Phase 7: REST API verification

Using the `api_base` URL from Step 5.3, make the following requests with `curl`:

**Step 7.1 — Daemon status endpoint:**

```bash
curl -s <api_base>/status
```

Verify: response is JSON with `nodes_total`, `nodes_running`, etc. Also verify `port` is non-zero.

**Step 7.2 — Nodes status endpoint:**

```bash
curl -s <api_base>/nodes/status
```

Verify: response is JSON with node entries. Each running node should have `pid` (number) and
`uptime_secs` (number) fields present in the response.

**Step 7.3 — OpenAPI spec:**

```bash
curl -s <api_base>/openapi.json
```

Verify: response is valid JSON containing OpenAPI spec.

**Step 7.4 — Status console:**

The console is at the root level (`/console`), not under `/api/v1`. Derive the base URL from
`api_base` by stripping `/api/v1`:

```bash
curl -s http://127.0.0.1:<port>/console
```

Verify: response contains HTML (check for `<html` or `<!DOCTYPE`).

**Step 7.5 — CORS headers:**

CORS is restricted to the daemon's own localhost origin. Use the daemon's actual port (from Step 5.3)
when sending the Origin header:

```bash
curl -s -I -X OPTIONS -H "Origin: http://127.0.0.1:<port>" -H "Access-Control-Request-Method: GET" <api_base>/status
```

Verify: response includes `access-control-allow-origin: http://127.0.0.1:<port>` header
(case-insensitive check). A cross-origin request (e.g., `Origin: http://example.com`) should NOT
receive this header.

### Phase 8: Daemon restart adoption

This phase validates the adoption behaviour introduced by the `adopt_from_registry` path in
`supervisor.rs`. When the daemon restarts, running node processes must be re-attached (not
respawned), their PIDs must match the pre-restart PIDs, and `uptime_secs` must be continuous
(not reset to 0). Both the happy path (via `node.pid` files) and the fallback path (via OS
process-table scan) must succeed, and the liveness monitor must detect an externally killed
node on its next poll.

**Step 8.1 — Capture baseline PIDs, uptimes, and data_dirs:**

```
ant node status --json
```

For each of the 3 `running` nodes, record `node_id`, `service_name`, `pid`, and `uptime_secs`.
These are the baseline values used in the assertions that follow.

Also fetch each node's `data_dir` via the REST API (needed in Step 8.5):

```bash
curl -s <api_base>/nodes/1 | jq -r '.data_dir'
curl -s <api_base>/nodes/2 | jq -r '.data_dir'
curl -s <api_base>/nodes/3 | jq -r '.data_dir'
```

**Step 8.2 — Stop the daemon (leave nodes running):**

```
ant node daemon stop --json
```

Verify: response contains `pid` (the stopped daemon's pid).

Verify the 3 node processes are **still alive** using OS-native tools — the daemon must not
kill its nodes on shutdown:

- Linux/macOS: `ps -p <pid>` for each baseline pid — exit code 0 means alive
- Windows: `tasklist /FI "PID eq <pid>"` for each baseline pid — output should list the process

All 3 node processes must still be running. Any failure here means daemon shutdown is killing
nodes, which is a regression.

**Step 8.3 — Restart the daemon (happy-path adoption via pid file):**

```
ant node daemon start --json
```

Verify: JSON response contains `pid` (the new daemon pid) and `already_running` is `false`.

```
ant node status --json
```

Verify:
- `nodes` array has 3 entries, all with status `running`
- For each node, `pid` **equals** the baseline `pid` captured in Step 8.1 (adopted — not
  respawned)
- For each node, `uptime_secs` is **greater than or equal to** the baseline `uptime_secs`
  (continuous, not reset to 0)

This confirms happy-path adoption via the `node.pid` file in each node's data directory.

**Step 8.4 — Stop the daemon again (prepare for fallback scan):**

```
ant node daemon stop --json
```

Verify the 3 node processes are still alive (same check as Step 8.2).

**Step 8.5 — Simulate a pre-adoption daemon (remove pid files):**

For each `data_dir` captured in Step 8.1, delete the `node.pid` file. This simulates nodes
spawned by a daemon build that predates the adoption feature and never wrote the pid file.

- Linux/macOS:
  ```bash
  rm -f "<data_dir_1>/node.pid" "<data_dir_2>/node.pid" "<data_dir_3>/node.pid"
  ```
- Windows:
  ```
  del "<data_dir_1>\node.pid" "<data_dir_2>\node.pid" "<data_dir_3>\node.pid"
  ```

Verify each file is gone before proceeding.

**Step 8.6 — Restart the daemon (fallback process-table scan):**

```
ant node daemon start --json
```

```
ant node status --json
```

Verify the same assertions as Step 8.3:
- 3 nodes, all `running`
- PIDs match the Step 8.1 baseline
- `uptime_secs` is continuous (>= baseline)

This confirms the fallback path: when `node.pid` is missing, the supervisor scans the OS
process table (via `sysinfo`) and matches processes by `binary_path` + `--root-dir` argument.

**Step 8.7 — Liveness monitor detects an external kill:**

Kill `node1` externally (outside the daemon), using its baseline pid from Step 8.1:

- Linux/macOS: `kill -9 <node1_pid>`
- Windows: `taskkill /F /PID <node1_pid>`

Wait ~10 seconds for the liveness monitor to observe the exit (it polls every 5s; allow
margin):

- Linux/macOS: `sleep 10`
- Windows: `timeout /T 10 /NOBREAK`

Then query status:

```
ant node status --json
```

Verify:
- `node1`'s status is `stopped` (or `errored`) — the daemon detected the exit
- For `node1`, `pid` and `uptime_secs` are **not present** (omitted via `skip_serializing_if`,
  same as Step 6.5)
- `node2` and `node3` remain `running` with their Step 8.1 baseline PIDs

**Step 8.8 — Restart the killed node:**

Return to the "3 running nodes" state expected by the Cleanup phase:

```
ant node start --service-name node1 --json
```

Verify the response shows a new `pid` for `node1` (a fresh process).

```
ant node status --json
```

Verify all 3 nodes are `running` before proceeding to cleanup.

### Phase 9: Cleanup

**Step 9.1 — Stop all nodes:**

```
ant node stop --json
```

Verify all nodes stopped.

**Step 9.2 — Reset:**

```
ant node reset --force --json
```

Verify: `nodes_cleared` is 3.

**Step 9.3 — Stop the daemon:**

```
ant node daemon stop --json
```

Verify: response contains `pid`.

**Step 9.4 — Verify daemon stopped:**

```
ant node daemon status --json
```

Verify: `running` is `false`.

### Phase 10: Report

Print a summary of all test steps and their results. Include the operating system and architecture
at the top of the report (e.g., from `uname -a` on Linux/macOS or `systeminfo` on Windows):

```
=== E2E Node Management Test Results ===
Platform: Linux 6.18.7-arch1-1 x86_64

Phase 5: Daemon Lifecycle
  [PASS] 5.1 Daemon start
  [PASS] 5.2 Daemon status
  [PASS] 5.3 Daemon info

Phase 6: Node Management
  [PASS] 6.1 Add 3 nodes
  [PASS] 6.2 Start all nodes
  [PASS] 6.3 Node status (all running)
  [PASS] 6.4 Stop single node
  [PASS] 6.5 Status (partial - 1 stopped, 2 running)
  [PASS] 6.6 Start single node
  [PASS] 6.7 Stop all nodes
  [PASS] 6.8 Start all nodes

Phase 7: REST API
  [PASS] 7.1 GET /status
  [PASS] 7.2 GET /nodes/status
  [PASS] 7.3 GET /openapi.json
  [PASS] 7.4 GET /console
  [PASS] 7.5 CORS headers

Phase 8: Daemon Restart Adoption
  [PASS] 8.1 Baseline PIDs/uptimes/data_dirs captured
  [PASS] 8.2 Daemon stop (nodes still alive)
  [PASS] 8.3 Daemon restart — happy-path adoption (pid file)
  [PASS] 8.4 Daemon stop (prepare for fallback test)
  [PASS] 8.5 node.pid files removed
  [PASS] 8.6 Daemon restart — fallback process-table scan
  [PASS] 8.7 Liveness monitor detected external kill
  [PASS] 8.8 Killed node restarted

Phase 9: Cleanup
  [PASS] 9.1 Stop all nodes
  [PASS] 9.2 Reset
  [PASS] 9.3 Daemon stop
  [PASS] 9.4 Daemon not running

Result: ALL TESTS PASSED
```

If any step failed, the report should show which step failed and stop there (because of the
fail-fast rule, no subsequent steps would have run).
