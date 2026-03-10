# App Server SDK

Rust SDK for running `codex-app-server` in-process and calling it directly through
functions instead of stdio or websocket transport.

## Example

```bash
cargo run -p codex-app-server-sdk --example direct -- "Summarize this repo"
```

The example uses your normal Codex config and auth, so it behaves like an embedded
app-server client inside the same process.

By default, the in-process SDK disables upstream response websockets and uses HTTP
for model requests. To re-enable response websockets, construct
`InProcessAppServerOptions` with `disable_response_websockets: false`.
