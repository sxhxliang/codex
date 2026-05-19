# Codex 远程控制端到端验证指南

本文档描述如何使用本仓库自带的 `codex-mock-chatgpt-account-server`（含 `/wham/remote/control/*` 中继）来快速验证 Codex 的远程控制（"Phone → Codex"）功能。

完整链路：

```
+---------------+        WSS         +-----------------+        WSS        +----------------+
|   ChatGPT App | <----------------> |  Mock ChatGPT   | <---------------> |  Codex CLI /   |
|  (Phone 端)   |    /client         |  Backend Relay  |   /server         |  App-Server    |
+---------------+                    +-----------------+   + HTTPS enroll  +----------------+
       |                                                                          |
       |                                                                          |
       +--------------- 浏览器调试控制台代替 Phone 端 -----------------------------+
                  http://127.0.0.1:8765/codex/remote-control
```

Mock 提供的两个 WebSocket 端点：

- Codex 侧：`GET /backend-api/wham/remote/control/server`
- Phone 侧：`GET /backend-api/wham/remote/control/client`

Codex 通过 `POST /backend-api/wham/remote/control/server/enroll` 注册，拿到 `(server_id, environment_id)` 后建立长连。Phone 通过 `environment_id` 路由到对应 Codex。

---

## 0. 前置条件

```bash
# Rust (推荐 1.93+；与仓库 Cargo.lock 对齐即可)
rustc --version

# Node.js（仅 mock 前端构建时需要）
node --version    # 18 或更新
npm --version
```

---

## 1. 构建 + 启动 Mock 服务器

```bash
cd /path/to/codex/codex-rs/mock-chatgpt-account-server/frontend
npm install          # 首次执行，安装 React/Vite/Tailwind 依赖
npm run build        # 产物写入 frontend/dist/，会被 Rust 端 include_str! 进二进制

cd /path/to/codex/codex-rs
cargo run -p codex-mock-chatgpt-account-server -- \
  --host 127.0.0.1 \
  --port 8765
```

启动成功后控制台会打印：

```
Mock account server listening on http://127.0.0.1:8765
OAuth issuer: http://127.0.0.1:8765
Browser login: debug@example.com / debug-password
Models endpoints: http://127.0.0.1:8765/models, /v1/models, and /backend-api/codex/models
Responses endpoint: http://127.0.0.1:8765/backend-api/codex/responses
ChatGPT backend base URL: http://127.0.0.1:8765/backend-api
Device auth page: http://127.0.0.1:8765/codex/device
```

把这个进程留着，下一步打开浏览器或者另开终端运行 Codex。

### 1.1 常用启动参数

| 参数 | 默认 | 说明 |
|------|------|------|
| `--host` | `127.0.0.1` | 监听地址 |
| `--port` | `8765` | 监听端口 |
| `--chatgpt-account-id` | `org-debug` | Mock 颁发的 JWT 中的 `chatgpt_account_id`；Codex 端 + 浏览器控制台都要用这个 |
| `--access-token` | `mock-chatgpt-access-token` | OAuth token endpoint 返回的 bearer token |
| `--refresh-token` | `mock-chatgpt-refresh-token` | OAuth refresh token |
| `--login-username` / `--login-password` | `debug@example.com` / `debug-password` | 浏览器登录页面凭据 |
| `--strict-account-header` | `false` | 打开后，relay/HTTP 路径会强制校验 `chatgpt-account-id` header 与 `--chatgpt-account-id` 一致 |

完整列表见 `codex-rs/mock-chatgpt-account-server/src/server/args.rs`。

### 1.2 健康检查

```bash
curl -sS http://127.0.0.1:8765/healthz
# {"ok":true}
```

---

## 2. 用浏览器控制台快速验证 relay

如果只想验证 mock 的 relay 本身可用（不接入真实 Codex），最快路径是用内置的调试控制台。它在同一个页面里同时模拟 Phone 和 Codex 两端。

打开：<http://127.0.0.1:8765/codex/remote-control>

页面分四块：

### 2.1 Enroll

1. 已经预填了默认 Bearer / Account / 随机 `installation_id` / `Mock host`。
2. 点 **Enroll**，下面会出现一张表格列出 `server_id` 与 `environment_id`，证明 `POST .../enroll` 工作正常。

### 2.2 Phone WSS

1. 此刻按钮 **连接 Phone** 已经可点。点击后状态变为 **已连接**，下方日志里会出现 `phone WSS → ws://.../client?...`。
2. JSON 文本框里有一条示例 `client_message`。点 **Phone → 发送**，日志里会同步出现 `phone-out` 和 `phone-in`（如果同时也连了 Codex 侧）。

### 2.3 Codex WSS（可选）

1. 点 **连接 Codex**。浏览器伪装成 Codex 本地端建立 `/server` WSS。
2. 在 Codex 那一侧的 JSON 框里默认有一条 `server_message`。点 **Codex → 发送**：
   - Mock 给 envelope 打上 `cursor`，按 `(client_id, stream_id)` 路由到上面的 Phone WSS。
   - Phone 那一侧立刻收到 `codex-in` 日志记录。

### 2.4 协议核心验证按钮

| 按钮 | 触发的不变量 |
|------|------|
| **发送 Ack(最高 seq_id)** | 把 phone 已观察到的最大 `seq_id` 作为 Ack 发给 Codex；mock 同时裁剪自己的 outbound replay buffer（已 ack 的消息断线后不会重放） |
| **ClientClosed (this stream)** | 关闭当前 `(client_id, stream_id)`：phone link 解绑 + outbound buffer 对应条目清空 |
| **ClientClosed (all streams)** | 关闭整个 `client_id`：所有该 client 下的 phone link 都取消，outbound buffer 中该 client 的条目全部清空 |
| **新 installation_id / stream_id / client_id** | 用来观察 enroll 幂等性、多 stream 共存、preempt 等行为 |

### 2.5 调试技巧

- **断线重连重放**：先在 Phone 收到几条 `codex-in`，记下页面里显示的"订阅游标"。把 Phone 断开后再点连接（控制台会自动带上 `subscribe_cursor`），日志里会重放 cursor 之后的所有条目。
- **背压 1011**：把 Codex 端用脚本灌入 ~600 条 100KB envelope（或在 JSON 框里粘贴一条大消息后反复点 **Codex → 发送**），Phone 不读取，最终会收到一条 `Close code=1011`。
- **多 stream 隔离**：在两个浏览器 Tab 里分别用同一个 `client_id`、不同 `stream_id` 连接 Phone WSS；它们的 cursor 与 ack 互不影响。

---

## 3. 用真实 Codex 接入 Mock 验证端到端

这是真正的"Phone（ChatGPT App）远程控制 Codex"链路。

### 3.1 把 Codex 指向 Mock

在 `~/.codex/config.toml` 里写入：

```toml
# 让 Codex 用 mock 当作 ChatGPT 后端
chatgpt_base_url = "http://127.0.0.1:8765/backend-api"

# 推荐：用一个独立的 profile，避免污染真账号的配置
[profiles.mock]
chatgpt_base_url = "http://127.0.0.1:8765/backend-api"
```

> ⚠️ Codex 出于安全考虑只接受 `https://chatgpt.com`、`https://chatgpt-staging.com` 以及 `http(s)://localhost` / 回环地址作为 `chatgpt_base_url`（见 `app-server-transport/src/transport/remote_control/protocol.rs::normalize_remote_control_url`）。所以 mock 必须监听在 `127.0.0.1` 或 `localhost`，端口任意。

如果不想改 `config.toml`，也可以用 `--config` 内联：

```bash
cargo run -p codex-cli -- \
  --config 'chatgpt_base_url="http://127.0.0.1:8765/backend-api"' \
  login
```

### 3.2 用 Mock 的凭据完成登录

```bash
# 方式 A：浏览器登录（会自动打开浏览器或打印 URL）
codex login
# 浏览器跳到 http://127.0.0.1:8765/oauth/authorize?...
# 用户名: debug@example.com
# 密码:   debug-password
# 点"继续"，回到本机回调，Codex 写入 ~/.codex/auth.json

# 方式 B：设备授权流
codex login --device-auth
# 复制终端上的 user_code，访问 http://127.0.0.1:8765/codex/device 点 Approve
```

登录成功后：

```bash
cat ~/.codex/auth.json
# {"access_token":"...mock-chatgpt-access-token-payload...","refresh_token":"mock-chatgpt-refresh-token", ...}

cat ~/.codex/installation_id
# 比如 b67e8d9e-...  这个 ID 等下需要在浏览器控制台里复用
```

### 3.3 启动 Codex 的远程控制

Codex 远程控制是通过 app-server 守护进程提供的。三种典型起法：

**A. 推荐：用 daemon 模式后台常驻**

```bash
codex remote-control start
# 等价于 codex app-server daemon enable-remote-control + start
# 输出 JSON 里包含 socket 路径与 PID
```

**B. 前台跑一次（便于看日志）**

```bash
codex app-server --remote-control --listen stdio:// 2>&1 | tee /tmp/codex-app-server.log
```

**C. 在 TUI 里直接开启（仍是 daemon 模式）**

```bash
codex            # 进入 TUI
# 按 / 调出命令面板，选 "remote control" 之类菜单
```

Codex 启动后会：

1. 调 `POST http://127.0.0.1:8765/backend-api/wham/remote/control/server/enroll`，请求体里 `installation_id = <~/.codex/installation_id 的内容>`、`name = <你的 hostname>`、`os/arch/app_server_version` 来自编译期。
2. 收到 `(server_id, environment_id)` 后建立 WSS `…/server`，并把 `x-codex-server-id`、`x-codex-installation-id`、`x-codex-protocol-version: 3`、`Authorization: Bearer …`、`chatgpt-account-id: org-debug` 写进握手 header。

观察 mock 控制台，应该立刻看到类似：

```
[mock-account-server] POST /backend-api/wham/remote/control/server/enroll -> 200
```

而 Codex 那边的日志会显示 `RemoteControlConnectionStatus::Connected`。

### 3.4 用浏览器控制台扮演 Phone，发送指令给 Codex

1. 打开 <http://127.0.0.1:8765/codex/remote-control>。
2. **务必复用 Codex 的 installation_id 和 hostname**，否则 enroll 会生成一个跟 Codex 不同的 environment：
   - **installation_id**: 把 `~/.codex/installation_id` 的内容粘进去
   - **server name**: 终端运行 `hostname`，把输出粘进去（macOS 默认像 `MacBook.local`）
   - **Bearer token / account-id**: 保持默认就行
3. 点 **Enroll**。表格里出现的 `server_id` 应该和你在 Codex 日志里看到的一致，这就证明走了幂等键回查。
4. **连接 Phone**：选一个 `client_id` 和 `stream_id`（默认随机即可），点连接。
5. 在 JSON 框里把示例换成你想发给 Codex 的真正请求，例如 JSON-RPC 调用：
   ```json
   {
     "type": "client_message",
     "message": {
       "jsonrpc": "2.0",
       "id": 1,
       "method": "initialize",
       "params": { "protocolVersion": "2025-11-25", "capabilities": {} }
     }
   }
   ```
6. 点 **Phone → 发送**。
7. Codex 的 app-server 会收到 envelope，按 JSON-RPC 路由进对应 request processor。Codex 端的回包会沿 WSS 流回 mock relay，再被路由到你的浏览器 Phone WSS，日志里出现 `codex-in`，内容是 Codex 真实回的 `initialize` response。

至此就完成了 **Phone → Mock relay → Codex → Mock relay → Phone** 的真实闭环。

### 3.5 验证四个协议核心不变量

一旦 3.4 跑通了 round-trip，以下不变量都可以用同一个会话顺手验：

1. **多 stream 并存**：再开一个浏览器 Tab，用同一 `client_id` 不同 `stream_id` 连接。Codex 那边的 `ConnectionId` 各自独立，互不串流。
2. **Ack 裁剪重放**：发几条 Phone → Codex / Codex → Phone 消息，记下页面顶部"订阅游标"。然后点 **发送 Ack(最高 seq_id)**，断开 Phone，再用相同 stream_id 重连。控制台不会重放已 ack 的内容。
3. **离线缓冲**：先停掉 Codex（`codex remote-control stop` 或 Ctrl-C），从浏览器发 5 条 `client_message`。重新 `codex remote-control start`，它会通过 `x-codex-subscribe-cursor` 重放之前的 5 条。
4. **慢消费者 1011**：在浏览器控制台里调小一段脚本不断从 Codex 那一侧发大 envelope（>100KB），Phone Tab 闲置不读；几秒内 Phone 那一侧会收到 `Close code=1011`，浏览器日志中显示 `phone WSS closed code=1011`。

---

## 4. 命令速查

```bash
# 后台启动 mock（在另一个终端跑 cargo run 也行）
cd codex-rs && cargo run -p codex-mock-chatgpt-account-server -- --host 127.0.0.1 --port 8765 &

# Codex 端用 mock
echo 'chatgpt_base_url = "http://127.0.0.1:8765/backend-api"' > ~/.codex/config.toml

# 登录 + 启动远控
codex login                       # 用 debug@example.com / debug-password
codex remote-control start        # 守护进程模式

# 查看 Codex 写入的 installation_id（浏览器控制台里要复用它）
cat ~/.codex/installation_id

# 看 Codex 远控状态
codex app-server daemon version
# 或 TUI 里观察 RemoteControlStatusChangedNotification

# 浏览器调试控制台
open http://127.0.0.1:8765/codex/remote-control

# 停止
codex remote-control stop
codex app-server daemon stop
```

---

## 5. 常见问题

### 5.1 `enroll` 返回 401 / 403

- 401: 缺 `Authorization: Bearer …`。Codex 端通常说明 `auth.json` 没生成或 token 失效，重跑 `codex login`。
- 403: `chatgpt-account-id` header 与 mock 的 `--chatgpt-account-id` 不一致。Codex 的 account-id 来自 mock 颁发的 JWT，所以只要登录走的是同一个 mock 就一定一致。如果你手动改过 `chatgpt-account-id` 头才会 403。

### 5.2 `codex` 连不上 `http://127.0.0.1:.../backend-api`，报 "unsupported scheme"

`normalize_remote_control_url` 只接受 `chatgpt.com`、`chatgpt-staging.com`（及其子域 HTTPS）和 `localhost` / 回环 IP 的 HTTP/HTTPS。请确认：

- mock 监听在 `127.0.0.1` 或 `localhost`（而不是 0.0.0.0 / 内网 IP）
- 配置里的 URL host 用 `localhost` 或 `127.0.0.1`

### 5.3 浏览器控制台连 Phone WSS 失败

浏览器无法在 WebSocket 握手里设置 `Authorization` header。Mock 已经做了 query 参数兜底：`?bearer=...&chatgpt-account-id=...`。如果你部署在反向代理后面，注意保留这两个 query 参数，或者改成在反代层注入 header。

### 5.4 我看到 Phone 收到的消息里多了一个 `cursor` 字段

这是 relay 主动盖上的"恢复点"。Phone / Codex 重连时把见过的最大 cursor 作为 `subscribe_cursor`（query 或 header）回传，relay 就会从此 cursor 之后开始重放。

### 5.5 重启 mock 后，Codex 一直报 "unknown server_id"

Relay 把环境放在内存里，进程重启会丢。Codex 的 `installation_id` 不变，所以下一次 enroll 时 mock 会**重新分配新的 server_id**——但 Codex 仍然记着旧的并尝试 WSS 接入，于是 404。两种修法：

- 重启 Codex（`codex remote-control stop && start`）让它重新走 enroll。
- 给 mock 接一个磁盘持久化（目前没实现）。

### 5.6 怎么彻底重置

```bash
codex remote-control stop
rm -f ~/.codex/installation_id ~/.codex/auth.json
# 重启 mock + 重新 login + 重启远控
```

---

## 6. 自动化验证（CI / 回归）

仓库自带 26 个端到端集成测试覆盖了 relay 协议的全部不变量：

```bash
cargo test -p codex-mock-chatgpt-account-server --test remote_control
```

测试矩阵（节选）：

| 测试 | 验证点 |
|------|------|
| `enroll_happy_path_and_idempotent` | enroll 200 + 幂等键回查 |
| `codex_ws_rejects_old_protocol_version` | `x-codex-protocol-version != "3"` → 426 |
| `end_to_end_phone_message_reaches_codex_with_cursor_stamped` | round-trip + cursor 戳记 |
| `codex_reconnect_replays_missed_inbound` | Codex 断线重连按 `x-codex-subscribe-cursor` 重放 |
| `phone_reconnect_replays_missed_outbound` | Phone 断线重连按 `subscribe_cursor` 重放 |
| `same_client_id_with_distinct_stream_ids_routes_independently` | 多 stream 隔离 |
| `same_client_and_stream_id_preempts_but_other_stream_unaffected` | 同 stream 重连抢占 4408，其他 stream 不受影响 |
| `phone_ack_trims_outbound_replay_buffer` | Ack 裁剪 outbound buffer |
| `phone_to_codex_persists_while_codex_is_offline` | receive-then-persist 保证 Codex 离线消息不丢 |
| `slow_phone_is_kicked_with_close_code_1011` | 背压 1011 |
| `client_closed_clears_outbound_buffer_for_named_stream` | ClientClosed(stream_id) 清流缓存 |
| `client_closed_without_stream_id_clears_all_streams_for_client` | ClientClosed(无 stream_id) 清整个 client |

如果你只想跑 lib 单元测试：

```bash
cargo test -p codex-mock-chatgpt-account-server --lib
```

---

## 7. 端点参考

| 路径 | 方法 | 说明 |
|------|------|------|
| `GET /healthz` | GET | 探活 |
| `GET /codex/remote-control` | GET | 浏览器调试控制台（HTML / JSON） |
| `POST /backend-api/wham/remote/control/server/enroll` | POST | Codex 注册 |
| `GET /backend-api/wham/remote/control/server` | WS upgrade | Codex 长连 |
| `GET /backend-api/wham/remote/control/client` | WS upgrade | Phone 长连（query: `environment_id`, `client_id`, `stream_id?`, `subscribe_cursor?`） |
| `GET /oauth/authorize` / `POST /oauth/token` / `POST /oauth/login` | – | OAuth |
| `POST /api/accounts/deviceauth/usercode` / `POST /api/accounts/deviceauth/token` | – | 设备授权 |
| `POST /backend-api/codex/responses` | POST SSE | Responses API 模拟 |
| `GET /backend-api/codex/models` | – | Models 列表 |

完整列表见 `codex-rs/mock-chatgpt-account-server/src/server/routes.rs::dispatch_http` 与 `routes.rs::routes`。
