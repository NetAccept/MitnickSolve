# envd v2 协议草案

> 状态：v0.1，草案，未实现。
> 配套文档：[design.md](design.md)（内核与整体架构）。
> 名称：`envd` 为占位名，与 Docker、Microsoft 的 envd 冲突，公开发布前改名。
> 定位：envd 只做机制，不做策略决策。是否容器化、以谁的身份执行、开放哪些出网，由部署 profile 与内核策略决定。

---

## 0. 范围

本协议定义：

- 控制面：MCP over Streamable HTTP 的工具集与语义。
- 数据面：会话、文件、端口转发的二进制帧协议。
- 会话状态机、错误模型、认证授权、配额、审计。
- 本地嵌入（stdio）、本地守护、容器、远程共用的契约。

本协议不定义：

- 容器编排、镜像管理与 OCI 语义。
- 内核侧的能力策略与审批流程（见 design.md）。
- 任意网络代理、VPN 或流量整形实现。
- Windows 目标机的完整原生命令语义。

---

## 1. 设计目标

1. 一套协议覆盖本地嵌入、本地守护、容器与远程机器。
2. 控制面与数据面分离：MCP 负责发现、控制、审计；字节流走二进制通道。
3. 基线实现只依赖 Go 标准库；WebSocket 等传输作为可选绑定。
4. 多 agent 就绪：每 agent 独立身份、作用域与配额。
5. 失败安全默认：回环绑定、路径 confine、进程树终止、token 不可被命令读取。
6. 可审计、可续传、可重连。
7. 双模式：`embedded-stdio` 用于本地嵌入，`service` 用于守护、容器与远程。

---

## 2. 角色与术语

| 术语 | 含义 |
| --- | --- |
| client | 内核（或任何 MCP 客户端）。 |
| envd | 执行环境守护进程。 |
| env | 一个 workspace 与执行边界，类型为 managed 或 mounted。 |
| session | env 上的一次长生命周期执行上下文。 |
| attachment | 客户端会话的一个连接视图；ro 或 rw。 |
| stream | 数据面上的多路复用单元，类型为 session、file、connect、accept。 |
| forward | 端口转发规则，local 或 remote。 |
| profile | `isolated`、`host`、`dev`，描述隔离级别。 |
| ticket | 可选的短期数据面凭据，用于受限委派。 |
| scope | token 上的权限标签。 |
| request_id | 每次调用的唯一标识，用于审计与排障。 |

---

## 3. 分层

```text
┌──────────────────────────── client ────────────────────────────┐
│  control: MCP HTTP (service) / CONTROL frames (embedded)       │
│  data:    stdio frames, TLS/TCP or WebSocket                   │
│  files:   small inline over MCP; large over data plane         │
│  audit:   MCP query; optional push sink                        │
└────────────────────────────────────────────────────────────────┘
                              │
                 stdio (embedded) / TLS (service)
                              │
┌──────────────────────────── envd ──────────────────────────────┐
│  auth / scopes / quotas / audit                                │
│  env manager  |  session manager  |  file tools  |  forwards   │
│  process groups / PTY / os.Root / cgroups or Job Objects       │
└────────────────────────────────────────────────────────────────┘
```

- 控制面：`service` 模式下为 MCP over Streamable HTTP（`POST /mcp`）；`embedded` 模式下为 CONTROL 帧承载的 MCP JSON-RPC。
- 数据面绑定：
  - `embedded-stdio`：内核以子进程启动 envd，stdin/stdout 上是同一套帧协议；无端口、无 token、无 TLS；控制与数据复用同一管道。
  - `tls-tcp`：`/data`，TLS 上直接发帧；本地守护、容器与直连远程默认。
  - `wss`：`/data`，WebSocket 二进制消息，每条消息一个完整帧；经 HTTP 反向代理时使用。
- 文件：小数据（默认 ≤ 1 MiB）内联 MCP；大数据走数据面；`file_link` 为外部客户端提供能力 URL。

> **实现状态（0.2）**：控制面 `POST /mcp` 与独立 TCP/TLS 数据监听已实现；尚未提供 `/data` 的 HTTP Upgrade/WSS，也尚未把控制与数据合并为单连接多路复用。数据面接受 `CREDIT` 帧但未据此限流。连接断开后 session 继续运行，但当前数据视图不保留重连游标；详见 [`tools/envd/README.md`](../tools/envd/README.md)。

---

## 4. 版本与能力协商

### 4.1 协议版本

- 当前版本：`2.0`，语义化版本；补丁版本不改变线格式。
- MCP revision 支持：`2025-11-25`（首选）、`2025-06-18`、`2025-03-26`。
- 数据面在 `HELLO` 中协商 `protocol.min` / `protocol.max`。

### 4.2 initialize

`initialize` 结果包含：

```json
{
  "protocolVersion": "2025-11-25",
  "serverInfo": {"name": "envd", "version": "2.0.0"},
  "capabilities": {
    "tools": {},
    "envd": {
      "protocol": {"min": 2, "max": 2},
      "tools_version": "2.0",
      "capabilities": [
        "transport.embedded-stdio", "transport.tls-tcp", "transport.wss",
        "env.managed", "env.mounted", "session.pty", "session.pipe",
        "pty.posix", "pty.conpty", "process.job", "signals.posix", "signals.console",
        "fs.acl", "sandbox.container", "sandbox.wsl", "shell.posix", "shell.powershell", "shell.cmd",
        "files", "forward.local", "forward.remote", "resume", "audit.query"
      ],
      "profiles": ["isolated", "host", "dev"],
      "limits": {"max_frame": 8388608, "max_sessions_per_env": 8}
    }
  }
}
```

- 客户端必须检查 `tools_version` 与所需 capabilities；不支持的调用返回 `UNSUPPORTED`。
- 新增能力只追加，不改变既有字段语义。

### 4.3 弃用

- 字段弃用至少保留一个次版本；响应中通过 `warnings` 返回弃用提示。
- 删除字段或改变语义必须升主版本。

---

## 5. 认证与授权

### 5.1 Token

- Bearer token，常数时间比较；每个 agent 一个 token。
- token 可由 `ENVD_TOKEN` 环境变量或权限为 0600 的 token 文件提供。
- `embedded-stdio` 不使用 token：身份来自父子进程关系与内核的进程所有权；该模式只允许本地 `dev` profile 或外层沙箱。
- token 带 `agent_id`、scopes 与过期时间；支持撤销。
- 非回环部署必须 TLS；回环也建议 TLS。
- 浏览器来源请求（带 `Origin`）默认拒绝。
- 认证失败冷却：连续 5 次失败封禁 60 秒。
- 限流：默认每调用方 120 请求/分钟；数据面单独计。

### 5.2 Scope

| scope | 允许 |
| --- | --- |
| `env:read` | `env_list`、`env_info` |
| `env:create` | `env_create`、`env_clone` |
| `env:delete` | `env_delete` |
| `exec` | `exec` |
| `session:read` | 只读 attach、`session_get`、`session_read` |
| `session:write` | 读写 attach、`session_write`、resize、signal |
| `session:admin` | `session_kill`、`session_close` |
| `fs:read` | `read_file`、`list_dir`、`search`、`stat_file`、下载 |
| `fs:write` | `write_file`、上传、原子替换 |
| `forward` | `forward_create/list/close`、connect/accept 流 |
| `audit:read` | `audit_query` |
| `admin` | 全部，包括跨 owner 访问与 token 管理 |

- env 有 owner `agent_id`；默认只有 owner 与 `admin` 可访问。
- 数据面握手使用同一 token；每个 stream 的操作仍按 scope 检查。

### 5.3 Token 与执行用户

- token 不写入命令环境、不写入 workspace、不出现在命令参数。
- 配置 `-exec-user` 时，命令以独立 uid/gid 运行，且不能读取 envd 的 `/proc/1/environ` 或 token 文件。
- 未配置独立执行用户时，token 文件权限必须拒绝 group/other；文档明确风险。
- Windows 无 `-exec-user` 语义：使用独立用户、受限 token 或低完整性级别；token 文件用 ACL 限制到服务账户。

### 5.4 委派

- `ticket_create` 不在 v2 实现：子 agent 共享内核 token，由内核负责作用域隔离。
- 预留协议位置：短期 ticket 绑定 env、scope 子集与 TTL，只用于数据面握手，不能调用控制面。
- 引入时通过 capability 声明；未声明时客户端不得使用。

---

## 6. 控制面：MCP 工具

### 6.1 通用约定

- 工具名使用扁平命名，输入使用 JSON Schema，默认 `additionalProperties: false`。
- 所有响应返回 `structuredContent`：

```json
{
  "ok": true,
  "data": {},
  "request_id": "req_...",
  "warnings": []
}
```

- 失败时 `ok=false`，`error` 为第 9 节错误信封，MCP `isError=true`。
- 分页：`limit`（默认 100，最大 5000）与 `cursor`（不透明）；响应带 `next_cursor` 与 `truncated`。
- 幂等：可变工具接受 `idempotency_key`；同 key 同请求在 TTL（默认 10 分钟）内返回同一结果。
- 取消：MCP 请求取消时，服务端终止对应操作；`exec` 与 session 相关工具必须响应取消。
- 所有工具接受 `_meta.envd.request_id`，未提供则服务端生成。
- `embedded-stdio` 模式下，MCP 请求/响应通过 CONTROL 帧传输，工具语义与 HTTP 模式一致。

### 6.2 环境管理

#### `env_create`

```json
{
  "name": "web-01",
  "kind": "managed",            // managed | mounted
  "root": "/data/workspaces/web-01",
  "profile": "isolated",
  "limits": {"cpu_cores": 1, "memory_bytes": 1073741824, "disk_bytes": 10737418240, "pids": 256},
  "labels": {"challenge": "web"}
}
```

返回：

```json
{"env_id": "env_...", "workspace": "/data/workspaces/web-01", "profile": "isolated", "limits": {}}
```

- `managed`：必须位于 envd 数据目录下。
- `mounted`：必须位于 `-allow-root` 声明的根内；删除只移除元数据，不删用户文件。
- 启动时重新校验 mounted 根；配置失效则拒绝启动。
- 克隆：managed 生成新副本；mounted 拒绝或显式 `copy=true`。

#### `env_list` / `env_info` / `env_clone` / `env_delete`

- `env_info` 返回 profile、capabilities、limits、usage、workspace、state、owner。
- `env_delete` 先终止该 env 的所有 session，再释放；`force` 跳过优雅等待。删除为幂等操作。
- 克隆/删除持 env 写锁；命令与文件操作持读锁。

### 6.3 会话

#### `session_create`

```json
{
  "env_id": "env_...",
  "argv": ["/bin/sh", "-i"],       // 与 shell 二选一
  "shell": "/bin/bash",
  "cwd": "/data/workspaces/web-01",
  "env": {"LANG": "C.UTF-8"},
  "user": "10002",
  "pty": true,
  "cols": 120,
  "rows": 40,
  "timeout_ms": 0,
  "idle_timeout_ms": 1800000,
  "max_lifetime_ms": 7200000,
  "output_buffer_bytes": 4194304,
  "idempotency_key": "..."
}
```

返回：

```json
{"session_id": "ses_...", "state": "starting", "created_at": "2026-11-14T12:00:00Z",
 "data": {"transport": "tls-tcp", "path": "/data", "protocol": 2}}
```

- `pty=true` 时 stdout/stderr 合并，支持 resize 与终端信号。
- `pty=false` 时 stdout/stderr 分离，通过 DATA 帧 `STDERR` 标志区分。
- `shell` 可为绝对路径或族名（`posix`、`powershell`、`cmd`）；服务端按平台解析，缺失返回 `UNSUPPORTED`。
- Windows 上 `pty=true` 使用 ConPTY；不可用时返回 `UNSUPPORTED`，或由客户端显式选择 `pty=false`。
- 同一 env 的 session 数受配额限制。

#### `session_get` / `session_list`

- 返回 state、exit（code/signal）、usage（cpu、内存、字节）、attachments、buffer 范围（first_seq/last_seq）。

#### `session_kill`

```json
{"session_id": "ses_...", "signal": "SIGTERM", "grace_ms": 5000}
```

- 先发信号，宽限期后终止整个进程组；PTY 关闭，缓冲保留。
- 幂等：已退出的 session 返回当前状态。

#### `session_close`

- 仅允许 `exited`/`failed`；释放缓冲与元数据。运行中调用等价于 `kill` + `close`，需要 `session:admin`。

#### `session_resize` / `session_signal`

- 仅 `pty=true` 且 state=running；resize 需要 rw attachment 或 `session:write`。
- signal 支持名称与数字；默认允许集合可配置。

#### `session_read` / `session_write`（便捷接口）

- 面向无法建立数据面的 MCP 客户端；单次上限 1 MiB，base64 编码。
- `read` 支持 `since_seq`；数据不足时返回 `next_seq` 与 `eof`。
- 大流量必须使用数据面；便捷接口不保证吞吐与实时性。

### 6.4 一次性执行

#### `exec`

```json
{
  "env_id": "env_...",
  "argv": ["nmap", "-sV", "10.0.0.5"],
  "cwd": "/data/workspaces/web-01",
  "env": {},
  "stdin": "",
  "timeout_ms": 30000,
  "max_output_bytes": 1048576,
  "output_mode": "inline",        // inline | file
  "user": "10002",
  "idempotency_key": "..."
}
```

返回：

```json
{"exit_code": 0, "signal": 0, "stdout": "...", "stderr": "...",
 "truncated": false, "duration_ms": 1234, "output_file": null}
```

- `output_mode=file` 时，大输出写入 env workspace 的临时文件并返回路径，客户端可通过共享对象读取。
- `shell` 解析同 `session_create`；Windows 默认优先 `pwsh.exe`，回退 `powershell.exe`/`cmd.exe`。
- 流式输出通过 MCP progress 通知（可选）；高吞吐场景改用 `session_create` + 数据面。
- 超时终止整个进程组；`truncated=true` 时明确标记。

### 6.5 文件

#### `read_file`

```json
{"env_id": "env_...", "path": "bin/chal", "offset": 0, "limit": 1048576}
```

返回 base64 `data`、`next_offset`、`eof`、`size`、`sha256`。默认上限 1 MiB。

#### `write_file`

```json
{"env_id": "env_...", "path": "notes.md", "data": "...", "offset": 0, "atomic": true}
```

- 默认原子写：同目录临时文件 + rename。
- 写入上限 1 MiB；大文件走数据面。
- 返回 `size` 与 `sha256`。

#### `list_dir` / `search` / `stat_file`

- `list_dir` 上限 5000 项，带 `next_cursor`。
- `search` 支持 glob、最大结果数、超时；不返回二进制匹配内容。
- `stat_file` 返回 size、mode、mtime、is_dir、symlink 目标（不跟随）。

#### `file_link`

```json
{"env_id": "env_...", "path": "dump.bin", "direction": "down", "ttl_s": 300}
```

- 返回签名 HTTPS URL、过期时间；URL 绑定 env、路径、方向，不携带主 token。
- 默认关闭；启用时要求 TLS；`-read-only` 禁止签发 upload 链接。
- 链接是一次性能力凭据，日志脱敏 `sig`/`signature`。

#### 路径规则

- 只接受相对路径；在 env root 下解析。
- 拒绝绝对路径、`..`、逃逸符号链接、跨 root 硬链接写。
- 使用 `os.Root` 等句柄化 API，避免 check-then-open 的 TOCTOU。
- Windows：工具传入路径必须是相对路径；拒绝盘符、UNC、ADS、设备名与保留名；大小写与分隔符规范化；内部使用长路径前缀。

### 6.6 端口转发

#### `forward_create`

本地转发（kernel 监听，envd 拨目标）：

```json
{"env_id": "env_...", "direction": "local",
 "target": {"host": "10.0.0.5", "port": 80}, "ttl_ms": 0}
```

远端转发（envd 监听，kernel 拨本地目标）：

```json
{"env_id": "env_...", "direction": "remote",
 "listen": {"host": "127.0.0.1", "port": 8888},
 "target": {"host": "127.0.0.1", "port": 3000}, "ttl_ms": 0}
```

- 默认只允许 `127.0.0.1` 监听；`0.0.0.0` 需要显式配置并写审计。
- 返回 `forward_id` 与（remote 的）实际监听地址。
- 关闭时终止所有关联 stream。

`forward_list` / `forward_close` 为管理接口；forward 生命周期绑定 env 或显式 TTL。

### 6.7 状态与审计

- `agent_info`：版本、protocol、capabilities、profiles、limits、usage。
- `health`：存活与就绪；不鉴权或使用独立探针 token。
- `audit_query`：按时间、env、session、event 过滤，带游标；返回结构化事件。

---

## 7. 数据面

### 7.1 连接与握手

- `service` 模式：每 agent 建议一条长连接，多路复用所有 stream。
- `embedded` 模式：内核启动 envd 子进程，stdin/stdout 即数据面；控制与数据共享这一条双向管道，fd 2 仅用于日志。
- 绑定：

| 绑定 | 端点 | 控制面 | 适用 |
| --- | --- | --- | --- |
| `embedded-stdio` | stdin/stdout | CONTROL 帧（MCP JSON-RPC） | 本地开发、外层沙箱 |
| `tls-tcp` | `/data` | MCP HTTP | 本地守护、容器、直连远程 |
| `wss` | `/data` | MCP HTTP | HTTP 反向代理 |

- 握手：

```json
// HELLO
{"protocol": {"min": 2, "max": 2}, "agent_id": "...", "token": "...",
 "resume": [{"stream_id": 7, "last_recv_seq": 42}],
 "capabilities": ["pty", "files", "forward"]}

// HELLO_ACK
{"protocol": 2, "server": "envd 2.0.0", "max_frame": 8388608,
 "credits_initial": 262144,
 "resume": [{"stream_id": 7, "action": "resumed"}]}
```

- `embedded-stdio`：`token` 为空；`resume` 必须为空；`max_frame` 默认 256 KiB，`credits_initial` 默认 64 KiB。
- `tls-tcp` / `wss`：`token` 必填；`max_frame` 默认 8 MiB，`credits_initial` 默认 256 KiB。
- `resume.action`：`resumed`、`gap`、`unknown`。
- 心跳：PING/PONG，15 秒；30 秒无响应断开。
- `service` 模式连接断开不等于 session 结束；session 继续运行，按 idle timeout 清理。
- `embedded` 模式是匿名管道，无法重连；EOF 表示内核退出，envd 终止所有 session 并退出。Linux 使用进程组与 Pdeathsig，Windows 使用带 `KILL_ON_JOB_CLOSE` 的 Job Object。
- `embedded` 模式下 CONTROL 帧优先于 DATA 帧发送，避免大块输出造成控制面头阻塞；stdin/stdout 必须为二进制模式（Windows 禁用 CRLF 转换与 Ctrl-Z）。

### 7.2 帧格式

```text
 0                   1                   2                   3
 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|     'M'       |     'S'       |     'D'       |     'P'       |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|     ver       |     kind      |           flags               |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|                          stream_id                            |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|                       seq (high 32 bits)                      |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|                       seq (low 32 bits)                       |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|                             len                               |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|                          payload ...                          |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
```

- `ver`：1 字节，当前 2。
- `kind`：1 字节，见 7.3。
- `flags`：2 字节大端。`0x0001` STDERR、`0x0002` EOF、`0x0004` LAST（保留）。
- `stream_id`：4 字节大端。`0` 保留给 CONTROL；其他 stream 客户端奇数、服务端偶数。
- `seq`：8 字节大端，按 stream 从 0 递增，仅 DATA 有意义。
- `len`：4 字节大端，payload 长度；`service` 上限 8 MiB，`embedded-stdio` 默认 256 KiB。
- 控制帧 payload 为 UTF-8 JSON；DATA 为原始字节；CREDIT 与 SEEK 为 8 字节大端整数。

### 7.3 帧类型

| kind | 名称 | 方向 | payload |
| --- | --- | --- | --- |
| 0x01 | HELLO | C→S | JSON |
| 0x02 | HELLO_ACK | S→C | JSON |
| 0x03 | OPEN | 双向 | JSON |
| 0x04 | OPEN_ACK | 双向 | JSON |
| 0x05 | DATA | 双向 | bytes |
| 0x06 | CREDIT | 双向 | u64 BE |
| 0x07 | SEEK | C→S | u64 BE |
| 0x08 | RESIZE | C→S | JSON |
| 0x09 | SIGNAL | C→S | JSON |
| 0x0A | EXIT | S→C | JSON |
| 0x0B | CLOSE | 双向 | JSON |
| 0x0C | ERROR | 双向 | JSON |
| 0x0D | PING | 双向 | 空 |
| 0x0E | PONG | 双向 | 空 |
| 0x0F | CONTROL | 双向 | JSON（MCP JSON-RPC） |

### 7.4 Stream 类型

#### control

- 仅在 `embedded-stdio` 模式使用；`stream_id=0`。
- 每条 CONTROL 帧承载一条完整的 MCP JSON-RPC 消息（请求、响应或通知）。
- 控制帧与数据帧共享同一管道；发送端优先调度 CONTROL。
- 该绑定不是 MCP 标准 stdio transport，外部 MCP 客户端不能直接连接。

#### session

```json
// OPEN
{"kind": "session", "session_id": "ses_...", "mode": "rw" | "ro",
 "since_seq": 0, "takeover": false}

// OPEN_ACK
{"ok": true, "session_id": "ses_...", "mode": "rw",
 "first_seq": 0, "last_seq": 1024, "pty": true}
```

- 同一 session 允许一个 rw attachment 与多个 ro attachment。
- `takeover=true` 抢占现有 rw，需要 `session:write` 并写审计。
- 输出 DATA 的 `seq` 递增；PTY 下 stdout/stderr 合并；非 PTY 用 `STDERR` 标志区分。
- 输入 DATA 从客户端到服务端；EOF 通过 `flags & EOF` 或 CLOSE 表示。
- `RESIZE` 仅 PTY；`SIGNAL` 递送终端信号；`EXIT` 携带退出码/信号。

#### file

```json
// OPEN
{"kind": "file", "env_id": "env_...", "path": "dump.bin", "mode": "r" | "w",
 "offset": 0, "atomic": true}

// OPEN_ACK
{"ok": true, "size": 104857600, "sha256": "..."}
```

- 读：服务端按 DATA 发送，受 credit 限制；支持 SEEK。
- 写：客户端发送 DATA；CLOSE 时原子提交（临时文件 + rename），返回最终 sha256。
- `mode=w` 默认截断；`offset` 用于续写或分块上传。
- 大文件不经过 MCP 与 base64。

#### connect

```json
{"kind": "connect", "forward_id": "fwd_...",
 "target": {"host": "10.0.0.5", "port": 80}}
```

- 本地转发：kernel 接受本地连接后，为每条连接开一个 connect stream，envd 拨 target 并双向 splice。
- 也可用于一次性直连（仍需 forward 或显式 allow）。

#### accept

```json
// 服务端发起 OPEN（stream_id 为偶数）
{"kind": "accept", "forward_id": "fwd_...",
 "peer": {"host": "127.0.0.1", "port": 54321},
 "target": {"host": "127.0.0.1", "port": 3000}}
```

- 远端转发：envd 接受入站连接后，主动开 accept stream；kernel 拨本地 target 并 splice。
- 客户端拒绝时回 ERROR；envd 关闭该连接。

### 7.5 流控

- 初始 credit：`HELLO_ACK.credits_initial`；`service` 默认 256 KiB，`embedded-stdio` 默认 64 KiB，按 stream 计。
- 发送方不得发送超过当前 credit 的 DATA 字节数。
- 接收方消费到窗口一半时发送 CREDIT（增量字节数）。
- 双向独立：stdout 与 stdin 各自有 credit。
- 缓冲有界；PTY 输出满时反压进程；`file-live` 满时丢最旧数据并标记 gap。
- credit 为 0 时发送方阻塞，不得无界排队。

### 7.6 续传与缺口

- 每个 stream 维护 `seq` 与环形缓冲（默认 4 MiB，可配置）。
- 重连时 HELLO 携带 `last_recv_seq`；服务端从缓冲重放。
- 缓冲不足时返回 `resume.action=gap`；客户端必须重新 OPEN，session 可能丢失部分输出。
- 文件流不支持 gap 续传；重新打开时用 offset/SEEK 指定位置。
- `embedded-stdio` 不支持续传：连接即进程生命周期，EOF 后 session 全部终止；HELLO 的 `resume` 必须为空。
- 服务端发送 ERROR `GAP`（13）并关闭该 stream。

### 7.7 关闭与取消

- 正常关闭：CLOSE，payload `{"reason": "client_close" | "server_close" | "exited"}`。
- 错误关闭：ERROR，随后关闭 stream。
- 半关闭：DATA 帧 `EOF` 标志表示写端结束，读端继续。
- 取消进行中的 open/操作：发送 CLOSE；服务端释放资源。
- 所有 stream 随连接断开进入 detach 状态；`service` 模式下 session 继续，file/forward stream 关闭。
- `embedded` 模式下连接断开即进程生命周期结束：所有 stream 与 session 终止，不做恢复。

---

## 8. 会话状态机

### 8.1 状态

| 状态 | 含义 |
| --- | --- |
| `created` | 元数据已分配，进程未启动。 |
| `starting` | 正在创建进程组、PTY、管道。 |
| `running` | 进程组存活。 |
| `terminating` | 已请求终止，处于宽限期。 |
| `exited` | 进程已结束；退出码/信号与输出缓冲保留。 |
| `failed` | 启动或内部失败，未进入 running。 |
| `closed` | 资源已释放，不可再操作。 |

### 8.2 迁移

```text
created -> starting -> running -> terminating -> exited -> closed
                    \-> failed -----------------------------> closed
running ---------------------------------------> exited
任何状态 --env_delete--> terminating/closed
```

- 自然退出：`running -> exited`。
- `kill`：`running -> terminating`；宽限期后 SIGKILL，随后 `exited`。
- 启动失败：`starting -> failed`。
- `close`：`exited`/`failed -> closed`；运行中 close 等价于 kill + close。
- `env_delete`：先终止所有 session，再释放 env。

### 8.3 操作矩阵

| 操作 | created | starting | running | terminating | exited | failed | closed |
| --- | --- | --- | --- | --- | --- | --- | --- |
| attach ro | 否 | 否 | 是 | 是（只读） | 是（读缓冲） | 否 | 否 |
| attach rw | 否 | 否 | 是 | 否 | 否 | 否 | 否 |
| write | 否 | 否 | 是 | 否 | 否 | 否 | 否 |
| resize | 否 | 否 | 是 | 否 | 否 | 否 | 否 |
| signal | 否 | 是 | 是 | 是 | 否 | 否 | 否 |
| kill | 是 | 是 | 是 | 是 | 否 | 否 | 否 |
| close | 否 | 否 | 否 | 否 | 是 | 是 | 否 |
| read buffer | 否 | 否 | 是 | 是 | 是 | 否 | 否 |

- Windows 信号映射：`SIGINT` -> `GenerateConsoleCtrlEvent(CTRL_C_EVENT)`；`SIGTERM` -> 优雅 console event 或终止；`SIGKILL` -> `TerminateJobObject`；其他信号返回 `UNSUPPORTED`。

### 8.4 超时与清理

- `idle_timeout_ms`：无 attachment 且无输出/输入达到阈值，进入 `terminating`。
- `max_lifetime_ms`：从 `starting` 起算，到点终止。
- `exited` 缓冲保留 TTL（默认 10 分钟）；过期自动 `closed`。
- 进程树终止：Unix 使用进程组；Windows 使用 Job Object；PTY 关闭后确保子进程不残留。
- Windows PTY 使用 ConPTY（Windows 10 1809+）；不可用时降级为 pipe 并声明 `pty.conpty=false`。

### 8.5 Attachment

- 一个 rw、多个 ro；rw 断开后 session 继续运行。
- `takeover` 抢占需要 `session:write`，旧 rw 收到 CLOSE 并写审计。
- detach 不影响进程；重新 attach 从 `since_seq` 续读。
- attachment 记录写入审计：agent、session、mode、takeover。

### 8.6 传输生命周期

- `service`：连接断开仅 detach，session 继续；重连按 `since_seq` 续读。
- `embedded`：stdin EOF 或子进程退出等价于终止所有 session；不做恢复，缓冲不保留。
- 内核崩溃时，Linux 通过 Pdeathsig/进程组、Windows 通过 Job Object 确保 envd 不残留。

---

## 9. 错误模型

### 9.1 错误信封

```json
{
  "code": 4,
  "name": "NOT_FOUND",
  "category": "not_found",
  "retryable": false,
  "message": "session not found",
  "details": {"session_id": "ses_..."},
  "request_id": "req_...",
  "retry_after_ms": 0
}
```

- `message` 面向模型与用户，必须脱敏、不含宿主机绝对路径。
- `details` 只放结构化、可安全展示的字段。
- 未知错误一律归为 `INTERNAL`，不向客户端回显 Go 错误或堆栈。

### 9.2 代码表

| code | name | category | retryable | HTTP |
| --- | --- | --- | --- | --- |
| 0 | OK | ok | — | 200 |
| 1 | INVALID_ARGUMENT | invalid_argument | 否 | 400 |
| 2 | UNAUTHENTICATED | unauthenticated | 否 | 401 |
| 3 | PERMISSION_DENIED | permission_denied | 否 | 403 |
| 4 | NOT_FOUND | not_found | 否 | 404 |
| 5 | CONFLICT | conflict | 否 | 409 |
| 6 | FAILED_PRECONDITION | failed_precondition | 否 | 412 |
| 7 | RESOURCE_EXHAUSTED | resource_exhausted | 视情况 | 429 |
| 8 | DEADLINE_EXCEEDED | deadline_exceeded | 是 | 504 |
| 9 | CANCELLED | cancelled | 否 | 499 |
| 10 | UNAVAILABLE | unavailable | 是 | 503 |
| 11 | INTERNAL | internal | 视情况 | 500 |
| 12 | UNSUPPORTED | unsupported | 否 | 501 |
| 13 | GAP | unavailable | 否（重开） | 409 |
| 14 | STALE | conflict | 否 | 409 |
| 15 | RATE_LIMITED | resource_exhausted | 是 | 429 |
| 16 | IDEMPOTENCY_CONFLICT | conflict | 否 | 409 |

### 9.3 映射

- 传输/认证失败：HTTP 状态码，MCP 请求不进入工具执行。
- JSON-RPC 结构错误：MCP 协议错误（`-32600`、`-32601`、`-32602`）。
- 工具执行失败：MCP `isError=true` + `structuredContent.error`。
- 数据面失败：ERROR 帧 + 关闭 stream。
- 客户端应只对 `retryable=true` 重试，并遵循 `retry_after_ms`；非幂等调用必须带 `idempotency_key` 才能自动重试。

### 9.4 幂等与冲突

- 支持 `idempotency_key` 的工具：`env_create`、`env_delete`、`env_clone`、`session_create`、`exec`、`write_file`、`forward_create`。
- 同 key 同请求：返回缓存结果，`warnings` 标记 `replayed`。
- 同 key 不同请求：`IDEMPOTENCY_CONFLICT`。
- 缓存 TTL 默认 10 分钟，可配置。

### 9.5 部分失败

- 批量操作（如 `env_delete` 多个、`forward_close` 多个）逐项返回结果；顶层 `ok=false` 仅当全部失败。
- 每项带独立错误码与 `request_id` 后缀。

---

## 10. 配额与限制

### 10.1 维度

| 维度 | 作用域 | 默认 |
| --- | --- | --- |
| `cpu_cores` | env | 1 |
| `memory_bytes` | env | 1 GiB |
| `disk_bytes` | env | 10 GiB |
| `pids` | env | 256 |
| `max_sessions_per_env` | env | 8 |
| `max_streams_per_conn` | 连接 | 256 |
| `max_frame` | 连接 | 8 MiB |
| `output_buffer_bytes` | session | 4 MiB |
| `max_output_bytes` | exec | 1 MiB 内联 |
| `requests_per_minute` | agent | 120 |
| `data_bytes_per_second` | agent | 可配置 |

### 10.2 执行

- 超限返回 `RESOURCE_EXHAUSTED`（7）或 `RATE_LIMITED`（15），`details` 带 `{limit, used, requested}`。
- 瞬时超限带 `retry_after_ms`；配置性上限不可重试。
- usage 通过 `env_info`、`session_get`、`agent_info` 报告。
- 配额执行点：env 创建、session 创建、exec、数据面 stream 打开、文件读写。
- Linux 使用 cgroups v2 限制 CPU、内存与 pids；磁盘使用卷配额或目录用量检查；无硬配额时标记为软限制。
- Windows 使用 Job Object 限制内存、CPU 与进程数；磁盘配额依赖卷级配额或目录用量检查，缺少硬限制时标记 capability。

---

## 11. 审计

### 11.1 事件

```json
{
  "seq": 123,
  "ts": "2026-11-14T12:00:00.123Z",
  "agent_id": "agent_...",
  "env_id": "env_...",
  "session_id": "ses_...",
  "event": "session.create",
  "action": "pty=true argv=/bin/sh",
  "target": "env_...",
  "result": "ok",
  "bytes": 0,
  "duration_ms": 12,
  "request_id": "req_..."
}
```

事件名：`auth.success`、`auth.failure`、`env.create/delete/clone`、`session.create/attach/detach/exit/kill`、`exec`、`fs.read/write/delete`、`forward.create/close`、`stream.open/close`、`quota.exceeded`、`policy.deny`、`data.gap`。

### 11.2 内容规则

- 不记录文件内容、命令输出、stdin、token、签名。
- 命令 argv 默认记录；可配置为只记录命令哈希。
- 路径只记录 env 内相对路径。
- 审计写入追加本地文件；`audit_query` 支持游标与过滤。
- HTTP/Unix socket sink 可选，至少一次、带序号，不阻塞执行。
- 原始输出默认保留 7 天；flag 与关键证据随任务保留；保留期可配置。

---

## 12. 安全模型与部署 Profile

| profile | 执行方式 | 默认用途 | 声明 |
| --- | --- | --- | --- |
| `isolated` | 容器 / VM / gVisor；只读根、cap drop、no-new-privileges、独立 uid | 不可信挑战二进制 | 进程、文件、网络均受限 |
| `host` | 专用 uid/gid；`os.Root`；最小 capability | 受控主机与工具链 | 文件 confine，进程共享宿主内核 |
| `dev` | 回环、放宽限制、显式标记 | 本机开发 | 明确提示风险 |

平台能力（`env_info.capabilities`）：

| capability | Linux | Windows | 说明 |
| --- | --- | --- | --- |
| `pty.posix` | 是 | 否 | POSIX PTY |
| `pty.conpty` | 否 | 是 | Windows 10 1809+ |
| `process.job` | 否 | 是 | Job Object 进程树 |
| `signals.posix` | 是 | 否 | 完整 POSIX 信号 |
| `signals.console` | 否 | 是 | Ctrl 事件映射 |
| `fs.acl` | 否 | 是 | Windows ACL |
| `sandbox.container` | 是 | 是（WSL2/容器） | |
| `sandbox.wsl` | 否 | 是 | |

- `env_info` 返回 profile 与实际 capabilities；内核据此限制工具。
- 容器是推荐的安全包装，不是执行模型；裸机部署明确标注隔离缺失。
- 非回环必须 TLS；反向代理需 `--trust-proxy` 且校验来源。
- `embedded-stdio` 与内核同 uid，不提供权限分离；命令可访问内核进程可见的资源。只允许 `dev` profile 或外层 bwrap/nsjail/容器命名空间；`host`/`isolated` 必须使用 `service`。
- `embedded-stdio` 的 token 为空，依赖进程所有权；审计仍记录本地 agent_id 与请求。
- Windows 原生为 Tier 2：优先 `pwsh.exe`，回退 `powershell.exe`/`cmd.exe`；PTY 使用 ConPTY，进程树用 Job Object；`isolated` 通过 WSL2/容器；文件使用长路径、拒绝 UNC/ADS/设备名，token 文件用 ACL 限制。
- 命令用户不得读取 envd token、`/proc/1/environ` 或审计文件。

---

## 13. 兼容性

- 协议主版本不兼容；次版本只追加能力与字段。
- 客户端在 `initialize` 与 `HELLO` 中声明版本范围；服务端选择交集。
- 工具 schema 版本通过 `_meta.envd.tools_version` 传递。
- MCP revision 变化时保持工具名与语义稳定；transport 差异由客户端适配。
- 未知帧 kind：`UNSUPPORTED`；未知 JSON 字段：忽略并告警（前向兼容）。
- 弃用策略见 4.3。

---

## 14. 一致性测试

必须覆盖：

- env：创建、挂载根校验、克隆、删除语义、锁。
- session：全部状态迁移、PTY/pipe、信号、resize、进程树终止、退出码、超时。
- 数据面：握手、分帧、credit、续传、gap、服务端发起 accept、心跳。
- 文件：路径逃逸、符号链接、硬链接、原子写、大文件流、sha256。
- 认证：缺 token、错 scope、Origin、限流、token 不在命令环境。
- 错误：代码映射、retryable、幂等冲突、部分失败。
- 配额：各维度超限与 usage。
- 审计：事件完整性、脱敏、游标查询。

测试形态：

- Go 集成测试与 race 测试。
- 语言无关的黑盒 conformance 套件，按协议文档驱动。
- 帧解析、路径处理、MCP JSON 的 fuzz 目标。
- 跨平台矩阵：linux/amd64、linux/arm64、darwin/arm64、windows/amd64。

---

## 15. 开放问题与决议

仍开放：

1. 大文件流是否默认启用压缩（zstd），以及是否作为可选 capability。
2. 多租户 ACL 是否引入 env 级 grant，还是长期只保留 owner/admin。
3. `embedded` 模式的 MCP 控制消息是否需要兼容标准 stdio transport，还是只保留 CONTROL 帧。

已决议：

- 嵌入模式必须显式 `--embedded-sandboxed`；自动检测只作为提示。
- Windows 无 ConPTY 时降级为 pipe 并声明 `pty.conpty=false`；shell 优先 `pwsh`。
- `ticket_create` 不在 v2 实现；子 agent 共享内核 token。
- `file_link` 默认关闭，由部署显式开启；大文件主路径是数据面。
- `session_read/write` 保留为兼容接口，限 1 MiB，不保证吞吐。
- 配额后端：Linux cgroups v2，Windows Job Object；磁盘无硬配额时标记为软限制。
- 审计：本地追加文件 + `audit_query`；HTTP sink 可选、至少一次、带序号、不阻塞执行。
- 命名：先冻结 v2 协议再抽离仓库，公开发布前改名；协议版本与 MCP revision 解耦。
- 控制面与数据面分层、文件大对象走数据面等见 design.md 的 ADR-005/009/017。

---

## 附录 A：控制面工具速查

| 工具 | scope | 说明 |
| --- | --- | --- |
| `env_create` | env:create | 创建 managed/mounted env |
| `env_list` | env:read | 列出 env |
| `env_info` | env:read | profile、capabilities、usage |
| `env_clone` | env:create | 克隆 env |
| `env_delete` | env:delete | 终止 session 并删除 |
| `session_create` | session:write | 创建会话 |
| `session_get` | session:read | 状态、退出、usage |
| `session_list` | session:read | 列出会话 |
| `session_kill` | session:admin | 信号 + 进程树终止 |
| `session_close` | session:admin | 释放已退出会话 |
| `session_resize` | session:write | PTY 尺寸 |
| `session_signal` | session:write | 终端信号 |
| `session_read` | session:read | 便捷读取（≤1 MiB） |
| `session_write` | session:write | 便捷写入（≤1 MiB） |
| `exec` | exec | 一次性执行 |
| `read_file` | fs:read | 读取（≤1 MiB） |
| `write_file` | fs:write | 原子写（≤1 MiB） |
| `list_dir` | fs:read | 列目录（≤5000） |
| `search` | fs:read | 搜索 |
| `stat_file` | fs:read | 元数据 |
| `file_link` | fs:read/fs:write | 短期能力 URL |
| `forward_create` | forward | 创建转发 |
| `forward_list` | forward | 列出转发 |
| `forward_close` | forward | 关闭转发 |
| `agent_info` | — | 服务信息 |
| `health` | — | 存活与就绪 |
| `audit_query` | audit:read | 查询审计 |

## 附录 B：数据面帧速查

| kind | 值 | payload | 备注 |
| --- | --- | --- | --- |
| HELLO | 0x01 | JSON | 版本、token、resume |
| HELLO_ACK | 0x02 | JSON | 选中版本、credit、resume 结果 |
| OPEN | 0x03 | JSON | stream 类型与参数 |
| OPEN_ACK | 0x04 | JSON | 结果、初始 seq、size |
| DATA | 0x05 | bytes | flags: STDERR/EOF |
| CREDIT | 0x06 | u64 BE | 增量 credit |
| SEEK | 0x07 | u64 BE | 文件流定位 |
| RESIZE | 0x08 | JSON | PTY 尺寸 |
| SIGNAL | 0x09 | JSON | 信号名或数字 |
| EXIT | 0x0A | JSON | code/signal/reason |
| CLOSE | 0x0B | JSON | 关闭原因 |
| ERROR | 0x0C | JSON | 错误信封 |
| PING | 0x0D | 空 | 心跳 |
| PONG | 0x0E | 空 | 心跳 |
| CONTROL | 0x0F | JSON | embedded 模式的 MCP JSON-RPC |

## 附录 C：典型流程

### C.1 创建环境、启动会话、交互执行

```text
client                                  envd
  |-- tools/call env_create ------------>|
  |<-- {env_id} -------------------------|
  |-- tools/call session_create -------->|
  |<-- {session_id, data endpoint} ------|
  |== data plane HELLO =================>|
  |<=================== HELLO_ACK -------|
  |== OPEN kind=session mode=rw -------->|
  |<=================== OPEN_ACK --------|
  |== DATA (stdin) --------------------->|
  |<=================== DATA (stdout) ---|
  |<=================== EXIT ------------|
  |== CLOSE ---------------------------->|
  |-- tools/call session_close --------->|
  |-- tools/call env_delete ------------>|
```

### C.2 大文件上传

```text
client                                  envd
  |== OPEN kind=file mode=w atomic=true >|
  |<=================== OPEN_ACK --------|
  |== DATA chunks (credit-bound) ------->|
  |<=================== CREDIT ----------|
  |== DATA ... EOF --------------------->|
  |== CLOSE ---------------------------->|
  |<=================== CLOSE {sha256} --|
```

### C.3 远端端口转发

```text
kernel (listener on envd)                envd
  |-- tools/call forward_create(remote) >|
  |<-- {forward_id, listen_addr} --------|
  |                                       |  (inbound connection)
  |<=================== OPEN kind=accept -|
  |== OPEN_ACK -------------------------->|
  |== DATA <--> DATA (spliced) ---------->|
```

### C.4 嵌入式 stdio

```text
client (parent)                          envd (child)
  |== HELLO (no token) ------------------>|
  |<=================== HELLO_ACK --------|
  |== CONTROL (MCP initialize) ---------->|
  |<=================== CONTROL (result) -|
  |== CONTROL (tools/call session_create) >|
  |<=================== CONTROL (result) -|
  |== OPEN kind=session mode=rw --------->|
  |<=================== DATA/EXIT --------|
  |== CLOSE ---------------------------->|
  |== EOF (parent exit) ----------------->|  envd 终止全部 session 并退出
```

---

## 附录 D：部署检查清单

- 绑定地址：默认 `127.0.0.1`；非回环必须 TLS。
- 嵌入式模式：仅 `dev` 或外层沙箱；stdin/stdout 二进制模式；退出清理靠 Pdeathsig/Job Object；不得用于 `host`/`isolated`。
- token：随机生成、权限 0600、不进入命令环境；支持撤销。
- 执行用户：独立 uid/gid；禁止读取 envd 环境与 token。
- workspace：managed 在数据目录下；mounted 只允许声明根。
- profile：不可信二进制只用 `isolated`；`host` 明确风险。
- 审计：开启，确认脱敏；保留期与容量。
- 配额：至少设置 CPU、内存、磁盘、session 数。
- 反向代理：`--trust-proxy` 仅对可信来源开启；校验 `Origin`。
- Windows：原生支持 ConPTY、Job Object、ACL；`isolated` 使用 WSL2/容器；路径做长路径、UNC、ADS、保留名测试。
