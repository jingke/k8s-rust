# remote-log-tui

Local terminal UI for **Kubernetes pod logs**: lists pods in your current cluster as **tabs**, streams logs for the selected pod using **[kube-rs](https://github.com/kube-rs/kube)** and [Ratatui](https://github.com/ratatui-org/ratatui).

Everything runs on your machine; the app uses your **kubeconfig** (same as `kubectl`).

## Prerequisites

- Rust toolchain
- A valid **kubeconfig** and network access to the API server (`kubectl get pods` should work)

## Build

```bash
cargo build --release
```

## Usage

```bash
# Default namespace `default`
cargo run --release

# Choose namespace / kubeconfig / context / container
cargo run --release -- --namespace kube-system
cargo run --release -- -n default --kubeconfig /path/to/config --kube-context my-ctx --container app --tail-lines 500
```

### Keys

| Key | Action |
|-----|--------|
| `←` / `→` | Previous / next pod tab (restarts log follow for that pod) |
| `r` | Reload pod list from the API |
| `↑` / `↓` | Scroll log one line |
| `PgUp` / `PgDn` | Scroll log one page |
| `Home` / `End` | Jump to oldest / newest buffered line |
| `g` | Follow tail (snap to live end) |
| `q` / `Esc` | Quit |

## How it works (message flow)

See **`material/k8s_message_flow.yaml`** for the step-by-step flow (kubeconfig → `Client` → list `Pod`s → `Tabs` → `log_stream` → channels → TUI).

## Security note

This only calls the Kubernetes API for list/watch-style log access permitted by your kubeconfig. Use namespaces and RBAC you trust.
