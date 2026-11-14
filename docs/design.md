# MitnickSolve 设计草案

> 状态：v0.1，草案，未实现。
> 已定决策：Rust 内核；无 native 工具；工具全部 WASM；内核组件为签名 C ABI 动态库（加载期加载、运行期不可变）；默认 TUI；所有机器访问统一经 envd。

---

## 0. 一句话架构

内核是一个跨平台的 Rust 守护进程，负责 LLM 交互、策略、调度、审计与工具运行时；所有工具是 WASM；可替换的周边实现以签名 C ABI 组件在启动时加载；所有执行（包括本机回环）统一走 envd；前端（TUI，后续 GUI）是内核的客户端。

---

## 1. 目标与非目标

### 1.1 目标

- 面向 CTF 的自主 agent：单机可用，可远程接入，可并行处理多个挑战。
- 跨平台：Windows 作为开发主机，Linux 作为主要部署目标。
- 默认 TUI，前端与内核解耦，后续 GUI 复用同一后端。
- 工具不可信、可沙箱、可替换。
- 内核周边实现可更新、可替换，且不需要重新编译内核。
- 统一执行入口 envd：本地嵌入/本地守护、容器、远程机器使用同一套协议与权限模型。
- 可观测、可重放、可评测。

### 1.2 非目标

- 不提供 native 工具（内核自身的基础设施不算工具）。
- 不做运行时热插拔或卸载组件；组件只在启动时加载。
- envd 不做容器编排；容器是部署形态。
- v1 不做多租户内核；一个内核实例服务一个用户。
- v1 不承诺各平台命令语义与安全保证完全一致；Windows 原生支持按 Tier 2 提供，差异由 capability 报告。

---

## 2. 术语

| 术语 | 含义 |
| --- | --- |
| 内核（kernel） | Rust 守护进程，持有一切权限，是唯一的策略决策点。 |
| 骨架（skeleton） | 内核中不可替换的部分：事件循环、策略引擎、能力管理、工具调度、审计、组件加载器。 |
| 端口（port） | 由 trait 定义的接口，例如 LLM provider、传输、存储、日志 sink。 |
| 适配器（adapter） | 端口的一个具体实现。 |
| 内核组件（core component） | 以 C ABI 动态库形式提供适配器的二进制，签名安装，启动时加载。 |
| 组件集（profile） | 一次运行所选定的内核组件集合及其版本与哈希。 |
| 工具（tool） | LLM 可调用的能力，只能由 WASM 实现，运行期加载，不可信。 |
| 前端（frontend） | 内核的客户端：TUI、CLI、后续 GUI。 |
| envd | Go 实现的远程执行环境，MCP 控制面 + 二进制数据面。 |
| 能力（capability） | 工具或组件被显式授予的一类权限。 |
| 共享对象（Object） | 内核持有、可跨调用与工具共享的持久资源，带内容哈希与数据标签。 |
| 视图（View） | 绑定单次工具实例的游标与读写句柄，随实例结束失效。 |
| ObjectRef | 模型可见的共享对象引用：任务内有效、不可猜测，但不是凭据。 |
| Handle | 工具运行期使用的短生命周期句柄，即视图。 |
| 会话（session） | envd 上的一次长生命周期执行上下文，支持 PTY、后台作业与流式 I/O。 |
| 事件日志（event log） | 追加式、可重放的系统事件记录。 |

---

## 3. 设计原则

1. **最小 TCB**：安全关键代码越少越好；能用 WASM 或独立进程解决的，不放进内核。
2. **无隐式权限**：任何能力都必须由 manifest 声明、安装期确认、调用期授予。
3. **信任在安装期建立**：native 组件运行后没有运行期隔离，因此签名、来源、conformance 必须在加载前完成。
4. **控制面与数据面分离**：MCP 负责发现、控制与审计；字节流、PTY、大文件走二进制数据通道。
5. **agent 质量优先**：上下文管理、工具输出裁剪、工件管理、重放与评测是一等公民，不是内核完成后的补充。
6. **一切可重放**：模型流、工具调用、envd 流量全部进事件日志。
7. **边界只用两种 ABI**：C ABI（内核组件）与 WASM ABI（工具）。不让 Rust ABI、C++ ABI 或语言特有类型跨边界。
8. **失败安全默认**：危险能力必须显式开启，而不是默认开启再让用户关闭。

---

## 4. 总体架构

```
┌──────────────┐   IPC (JSON-RPC over UDS / named pipe)   ┌─────────────────────────────┐
│ TUI / CLI    │◄────────────────────────────────────────►│           内核                │
│ (ratatui)    │                                          │                             │
└──────────────┘                                          │  ┌───────────────────────┐  │
                                                          │  │ 骨架（不可替换）        │  │
┌──────────────┐                                          │  │ 事件循环 / 策略 / 能力  │  │
│ GUI（后续）   │◄────────────── 同一 IPC ─────────────────►│  │ 调度 / 审计 / 加载器    │  │
└──────────────┘                                          │  └───────────┬───────────┘  │
                                                          │              │ ports        │
                                                          │  ┌───────────▼───────────┐  │
                                                          │  │ 内核组件（C ABI dylib） │  │
                                                          │  │ LLM / 传输 / 存储 / ... │  │
                                                          │  └───────────────────────┘  │
                                                          │  ┌───────────────────────┐  │
                                                          │  │ 工具运行时（wasmtime）  │  │
                                                          │  │ WASM 工具（不可信）      │  │
                                                          │  └───────────┬───────────┘  │
                                                          └──────────────┼──────────────┘
                                                                         │ MCP 控制面
                                                                         │ 二进制数据面
                                                          ┌──────────────▼──────────────┐
                                                          │            envd              │
                                                          │  本地嵌入/守护 / 容器 / 远程  │
                                                          └──────────────┬──────────────┘
                                                                         │
                                                          ┌──────────────▼──────────────┐
                                                          │      目标机器 / 挑战服务       │
                                                          └─────────────────────────────┘
```

### 4.1 进程拓扑

| 进程 | 职责 | 默认隔离 |
| --- | --- | --- |
| 内核 | 全部策略、调度、审计、WASM 运行时 | 每用户一个实例 |
| 组件宿主（可选） | 加载未审阅的第三方内核组件 | 独立进程，本地 IPC |
| envd | 执行会话、文件、端口转发 | 独立进程；推荐容器或专用用户 |
| 前端 | 展示与输入 | 独立进程 |
| 目标服务 | 挑战环境 | 由 envd 或外部网络管理 |

### 4.2 平台支持层级

| 层级 | 平台 | 内核 / TUI | WASM 工具 | envd | 承诺 |
| --- | --- | --- | --- | --- | --- |
| Tier 1 | Linux x86_64/arm64 | 完整 | 完整 | 完整：PTY、进程组、cgroup、容器 | CI + 发布阻断 |
| Tier 2 | Windows x86_64 | 完整 | 完整，受能力声明约束 | 原生：ConPTY、Job Object、文件、转发；`host`/`dev`；`isolated` 需 WSL2/容器 | CI + 冒烟；差异文档化 |
| Tier 3 | macOS arm64 | 完整 | 完整，受能力声明约束 | `host`/`dev`；PTY；无原生容器 | 构建通过，社区支持 |
| Tier 4 | 其他 | 可构建 | 视宿主 | 不保证 | 不承诺 |

- WASM 模块本身跨平台；差异来自 host imports 与远端执行环境，不是模块格式。
- envd 通过 `env_info` 报告 `capabilities`（如 `pty.conpty`、`process.job`、`sandbox.wsl`）；内核按能力启用或禁用工具。
- 工具 manifest 声明平台/能力要求；不满足时返回 `UNSUPPORTED`，不静默降级。
- “完全跨平台”指设计与构建；各平台的安全声明与语义差异独立陈述。

---

## 5. 信任模型与威胁模型

### 5.1 信任层级

| 层级 | 内容 | 是否在 TCB 内 | 说明 |
| --- | --- | --- | --- |
| T0 | 内核骨架 | 是 | 不可替换，安全论证的锚点。 |
| T1 | 第一方签名的内核组件 | 是（进程内） | 随发行版分发。 |
| T2 | 第三方内核组件 | 默认否（组件宿主进程） | 可选进程内加载，用户显式承担风险。 |
| T3 | WASM 工具 | 否 | 由 wasmtime 隔离，能力受控。 |
| T4 | envd | 否 | 执行边界，安全取决于部署 profile。 |
| T5 | LLM | 否 | 不可信输出，可能被注入。 |
| T6 | 挑战输入 | 否 | 文件、banner、网页、服务响应均为不可信输入。 |

**关键规则**：任何进程内加载的组件都属于 TCB。因此第三方组件默认走组件宿主；用户选择进程内加载时，安全声明必须相应降级。

### 5.2 资产

- LLM API key、envd token、其他凭据。
- workspace 中的题目文件、脚本、flag、笔记。
- 宿主机的文件系统与网络。
- 评测数据与历史运行记录。

### 5.3 主要威胁

| 威胁 | 入口 | 缓解 |
| --- | --- | --- |
| Prompt injection | 题目文件、网页、服务 banner、工具输出 | 数据标签与流控；秘密不出现在模型上下文；危险操作审批。 |
| 恶意或缺陷工具 | WASM 模块 | 能力声明、handle 约束、资源限额、审计。 |
| 供应链 | 第三方组件 | 签名、发布者信任、conformance、SBOM、默认进程外。 |
| 组件越权 | native 代码 | 端口最小化、sealed 端口、进程外宿主、安装期审查。 |
| 执行环境逃逸 | 不可信挑战二进制 | envd profile（容器/VM/专用用户）、默认隔离。 |
| 凭据泄漏 | 命令环境、日志、模型上下文 | token 不进入命令环境；日志脱敏；密钥库存储。 |
| 资源耗尽 | 跑飞的 agent 或工具 | 全局预算、每实例限额、envd 配额。 |

### 5.4 envd 的定位

envd 只做机制，不做策略决策：

- 内置失败安全机制：回环绑定、路径 confine、进程树终止、审计、限流、token 不可被命令读取。
- 是否使用容器、以谁的身份执行、开放哪些出网，由部署 profile 和用户决定。
- 传输分两种模式：`service`（MCP over HTTP + TLS/TCP 或 WebSocket，带 token、重连、多客户端与权限分离）与 `embedded`（内核以子进程启动 envd，stdio 帧同时承载控制与数据，无端口、无 token）。`embedded` 与内核同 uid，不提供权限分离，只允许 `dev` profile 或外层沙箱；`host`/`isolated` 必须用 `service`。
- envd 通过 `env_info` 报告 profile 与 capabilities，内核据此决定哪些工具可用。

---

## 6. Rust 工作区

```
mitnick/
├── Cargo.toml                  # workspace
├── crates/
│   ├── msk-core/               # 领域类型、端口 trait、策略接口
│   ├── msk-abi/                # C ABI 定义（cdylib 与内核共用）
│   ├── msk-kernel/             # 守护进程、装配、骨架
│   ├── msk-loader/             # 组件发现、签名校验、加载、conformance 调用
│   ├── msk-wasm/               # wasmtime 封装、host imports、限额
│   ├── msk-mcp/                # MCP 客户端与 envd 控制面
│   ├── msk-data/               # 数据面客户端、handle、工件存储
│   ├── msk-llm/                # LLM 归一化类型与内置 provider
│   ├── msk-events/             # 事件日志、重放
│   ├── msk-ipc/                # 前端 IPC 协议
│   ├── msk-tui/                # ratatui 前端
│   └── msk-testkit/            # conformance harness、测试替身
├── components/                 # 第一方内核组件（各自独立 crate，cdylib）
│   ├── llm-anthropic/
│   ├── llm-openai-compat/
│   ├── transport-http/
│   └── store-sqlite/
├── tools/                      # WASM 工具（MoonBit / Rust）
├── docs/
│   └── design.md
└── xtask/                      # 构建、签名、打包、评测入口
```

依赖方向：`msk-core` 不依赖任何适配器；适配器依赖 `msk-core` 与 `msk-abi`；`msk-kernel` 负责装配。骨架 crate 内部用 `pub(crate)` 隐藏，端口用 sealed trait 限制实现范围。

技术选型：Rust edition 2024；tokio；wasmtime 49+；`rmcp`；ratatui + crossterm；serde；tracing；ed25519-dalek + sha2；libloading；SQLite（`rusqlite`）作为默认存储。

---

## 7. 端口、适配器与内核组件

### 7.1 端口清单

| 端口 | 默认实现 | 可替换性 | 说明 |
| --- | --- | --- | --- |
| `LlmProvider` | 第一方 anthropic / openai-compat | open | 流式、工具调用、用量统计。 |
| `HttpTransport` | 第一方 reqwest | open | 代理、TLS、超时、重试。 |
| `Store` | 第一方 SQLite | open | 会话、任务、事件索引、工件元数据。 |
| `EventSink` | 第一方 JSONL | open | 可追加远端 sink。 |
| `SecretStore` | 系统密钥库 | open | Windows Credential Manager / Secret Service。 |
| `Clock` / `Rng` | 系统 | open | 便于测试与重放。 |
| `PolicyEngine` | 骨架内置 | **sealed** | 不可由组件替换。 |
| `CapabilityManager` | 骨架内置 | **sealed** | 同上。 |
| `ToolScheduler` | 骨架内置 | **sealed** | 同上。 |
| `Audit` | 骨架内置 | **sealed** | 组件只能写 sink，不能改语义。 |
| `WasmRuntime` | wasmtime | **sealed** | v1 不允许替换，避免安全声明漂移。 |

sealed 判定写死在加载器里，与签名无关：组件 manifest 声明 sealed 端口时直接拒绝。

### 7.2 约束方式

- 用 sealed trait 定义端口，外部 crate 无法实现；只有骨架内的适配器类型可见。
- 编译期用 trait bound 约束；运行期在装配点做类型擦除（`Arc<dyn LlmProvider>`），避免模板式泛型扩散。
- 每个端口配一套 conformance 测试，覆盖流式增量、取消、超时、错误分类、资源释放。concepts 只能约束签名，conformance 才对语义负责。
- 异步 trait 使用 boxed future 或 `trait_variant`；不把 `async fn` 直接暴露为 dyn 接口。

### 7.3 装配与组件集

- 内核启动时读取配置中的组件集：组件 ID、版本、哈希、端口。
- 第一方组件默认随发行版；第三方组件由用户安装到受控目录。
- 每次构建与运行输出组件清单（SBOM），用于安全声明。

---

## 8. 内核组件 ABI（C ABI）

### 8.1 基本原则

- 只使用 C ABI：`#[repr(C)]`、`extern "C"`、opaque handle，不跨边界传 Rust 类型、异常、trait object。
- 版本化：固定头包含 `abi_version` 与 `struct_size`，新增字段只能追加；内核按 size 做前向兼容。
- 内存所有权显式：调用方提供输入缓冲区；输出缓冲区由组件通过宿主分配器分配，调用方通过同一分配器释放。
- 异常与 panic 不得跨越边界：组件必须捕获 panic 并返回错误码；内核在 `extern "C"` 边界再加一层 `catch_unwind`。
- 生命周期：`init` 一次，`shutdown` 一次；`shutdown` 后不得再回调宿主。
- 线程模型：组件声明 `thread_safety`；非线程安全组件由内核串行化调用。
- 只加载，不卸载：不提供热更新。

### 8.2 入口与宿主服务

```c
typedef struct ms_abi_header {
    uint32_t abi_version;   /* MS_ABI_VERSION = 1 */
    uint32_t struct_size;   /* 整个包含结构的大小，不只是本头 */
    const uint8_t *id;      /* 静态 NUL 结尾 UTF-8 */
    const uint8_t *version; /* 静态 NUL 结尾 UTF-8 */
} ms_abi_header;

typedef int32_t (*ms_init_fn)(const ms_host *host, void **out_ctx);

typedef struct ms_host {
    ms_abi_header header;   /* struct_size = sizeof(ms_host) = 72 (64-bit) */
    void *ctx;
    void *(*alloc)(void *ctx, size_t len);
    void  (*free)(void *ctx, void *ptr, size_t len);
    void  (*log)(void *ctx, int32_t level, const uint8_t *msg, size_t len);
    int32_t (*config_get)(void *ctx, const uint8_t *key, size_t key_len,
                          uint8_t *buf, size_t *len);  /* len 输入容量/输出所需 */
    int32_t (*now_unix_ms)(void *ctx, int64_t *out);
} ms_host;

typedef struct ms_component {
    ms_abi_header header;   /* struct_size = sizeof(ms_component) = 56 */
    uint32_t port_kind;
    const void *vtable;
    ms_init_fn init;        /* 返回 i32，绝不返回 Rust enum */
    void (*shutdown)(void *ctx);
} ms_component;

const ms_component *ms_component_entry(void);  /* 进程生命周期静态指针 */
```

端口 vtable（LLM，v1 只启用 `complete`，`stream` 槽保留）：

```c
typedef struct ms_vtable_header {
    uint32_t abi_version;   /* 1 */
    uint32_t struct_size;   /* 整个 vtable 大小 */
    uint32_t port_kind;
    uint32_t reserved;      /* 必须为 0 */
} ms_vtable_header;

typedef struct ms_bytes { uint8_t *ptr; size_t len; } ms_bytes;

typedef int32_t (*ms_llm_complete_fn)(
    void *ctx, const uint8_t *request, size_t request_len,
    ms_bytes *response, uint8_t *err, size_t err_cap, size_t *err_len);

typedef void (*ms_llm_delta_fn)(void *sink_ctx, const uint8_t *ptr, size_t len);
typedef int32_t (*ms_llm_stream_fn)(
    void *ctx, const uint8_t *request, size_t request_len,
    ms_llm_delta_fn sink, void *sink_ctx, ms_bytes *response,
    uint8_t *err, size_t err_cap, size_t *err_len);

typedef struct ms_llm_vtable {
    ms_vtable_header header;  /* struct_size = 80 (64-bit) */
    ms_llm_complete_fn complete;
    ms_llm_stream_fn stream;  /* 可为 NULL；v1 保留 */
    const void *reserved[6];  /* 必须全为 NULL */
} ms_llm_vtable;
```

请求/响应是带 `schema_version=1` 的 UTF-8 JSON（`messages`、`tools`、`max_tokens` → `text`、`tool_calls`、`usage_tokens`）。响应缓冲区必须通过 `host.alloc(host.ctx, len)` 分配，成功后所有权转移给宿主；宿主用分配台账复制后释放，未登记指针拒绝释放。错误路径不信任组件返回的长度。错误码统一为 `int32_t`：0 成功，负数为参数、权限、未找到、超时、取消、不支持、内部；详情通过 1024 字节错误缓冲区返回。完整布局、所有权矩阵、限制与安全边界见 [`components.md`](components.md)。

### 8.3 Manifest 与签名

```toml
[component]
id = "acme.llm-provider"
version = "1.2.0"
abi = 1
port = "llm.provider"
publisher = "acme"
thread_safety = "safe"

[requires]
kernel = ">=0.3, <0.5"

[artifact]
file = "libacme_llm.so"
sha256 = "…"
```

- 签名覆盖规范化后的 manifest（含 artifact 哈希），算法 Ed25519。
- 信任库按发布者公钥组织；内核只接受信任库中的发布者。
- 防降级：同一组件 ID 版本单调递增；回滚需显式操作。
- 支持吊销列表与发布者移除。
- 签名只回答“谁发布、是否完整”，不回答“是否安全”；安全结论来自 conformance 与审查。

### 8.4 加载流水线

1. 发现组件目录中的 manifest。
2. 校验签名与发布者信任。
3. 校验 manifest 与二进制哈希一致。
4. 拒绝 sealed 端口的组件。
5. 在独立子进程中运行该端口的 conformance harness（超时、崩溃检测、合成输入）。
6. 通过后从受控目录以绝对路径加载；Windows 使用 `SetDefaultDllDirectories`，防止 DLL 搜索路径劫持。
7. 调用 `init`；失败则回滚并记录。
8. 注册到组件集；输出 SBOM。

### 8.5 组件宿主

- 对未审阅的第三方组件，默认在独立进程加载。
- 宿主进程实现同一 C ABI；内核侧代理实现对应端口，通过本地 IPC 转发调用。
- 崩溃只影响宿主；内核自动重启宿主并将该组件的任务标记失败。
- 传输为长度前缀的二进制帧，接口按端口类型定义；大块数据走共享内存或文件描述符传递（平台允许时）。

---

## 9. WASM 工具运行时

### 9.1 运行时

- wasmtime 49+；启用 component model、fuel/epoch、`StoreLimits`、pooling allocator（按需）。
- 每次工具调用创建新的 `Store`/`Instance`，工具保持无状态；状态放在内核或 envd。
- 模块按内容哈希缓存编译产物；缓存上限可配置。
- 默认限额：64 MiB 内存、单次调用墙钟超时、epoch 中断、输出上限、并发上限。

### 9.2 ABI

- 规范形式：WIT 世界 `tool`，语言中立；MoonBit 作为官方推荐前端，Rust/C 提供 SDK。
- v1 兼容层：在 MoonBit 的 component model 支持稳定前，允许 core module + 线性内存 JSON 信封（沿用 Mika ABI v1 的成熟做法）；`abi_version` 区分。
- 工具导出 `run(input) -> result`；输入输出为字节序列，语义由工具 schema 定义。
- 工具 manifest 声明：工具名、JSON schema、所需能力、限额、是否允许网络。

host imports 分五类：

| 类别 | 函数 | 说明 |
| --- | --- | --- |
| 日志 | `log` | 结构化、限速。 |
| 数据 | `data_open/attach/read-at/read/write/seek/stat/close/export` | 基于共享对象与视图，见第 10 节。 |
| 会话 | `session_open/exec/read/write/resize/signal/close` | 经内核到 envd 数据面。 |
| 网络 | `http_request` | manifest + 用户授权 + 全局开关三者同时成立。 |
| 环境 | `clock`, `random`, `config_get`, `progress`, `cancelled` | 基础环境；`progress` 上报进度。 |

不提供：LLM 调用、策略修改、组件加载、秘密读取、直接 socket。

### 9.3 取消与长任务

- 工具调用有 deadline；超时通过 epoch 中断并返回取消错误。
- 长任务不阻塞单次调用：工具可创建 envd session，立即返回 `session_id`，后续调用按 session 读取增量。
- 进度通过 `progress` 上报，事件日志记录；前端可见。

### 9.4 工具 SDK

- 提供 MoonBit、Rust、C 的最小 SDK：输入输出编解码、host imports 绑定、错误约定、测试夹具。
- 工具作者可在本地用 `ms-tool-test` 运行同一套 host 桩。

---

## 10. 数据通道与 Handle

### 10.1 两层模型：共享对象与视图

目标：大文件、工件与会话流可以跨工具调用复用，同时不把长期凭据暴露给模型或组件。

- **共享对象（Object）**：内核持有的持久资源，例如文件快照、工件、工具输出 blob、会话流。每个对象有稳定的 `ObjectRef`（128 位随机、任务内有效、不可猜测）、内容哈希、大小、数据标签、来源、代际与 TTL。
- **视图（View）**：工具实例打开对象后得到的短生命周期句柄，携带独立游标、访问模式与配额。视图不跨调用、不跨实例、不序列化。
- **引用与权限分离**：`ObjectRef` 是模型可见的引用字符串，可以出现在工具输入、输出与笔记中；它只是名字，不是凭据。每次 `attach`、每次读写都会重新检查任务作用域、数据标签与配额。`ViewHandle` 只在 host import 与内核之间使用，不进入模型上下文、日志或组件。
- **组件边界**：内核组件不直接持有 ObjectRef；需要访问数据的组件通过专用端口由内核代理，并按端口能力单独授权。

### 10.2 共享对象语义

| 类型 | 来源 | 语义 |
| --- | --- | --- |
| `artifact` | 工件存储 | 不可变，内容寻址，跨重启可用。 |
| `blob` | 工具输出或中间结果 | 不可变，可提升为 artifact。 |
| `file-snapshot` | workspace 或挂载目录 | 打开时记录哈希；源文件变化后旧对象标记 stale，新代际重新打开。 |
| `file-live` | 日志、持续写入的文件 | 跟随模式，读取有界缓冲，可能丢帧并显式标记缺口。 |
| `session` | envd 会话 | 单生产者、多视图，视图按位置读取，溢出标记 gap。 |

- 不可变对象支持多视图并发读，共享分块缓存；写入生成新代际，旧视图继续读取旧快照。
- 文件写入使用临时文件加原子替换；提交后新代际可见。
- 每对象维护引用计数、空闲 TTL 与最大视图数；回收只影响缓存，不使 ObjectRef 失效，重新 attach 时惰性恢复。
- 跨任务默认禁止共享；显式 `export` 生成新对象、重新打标签并写审计（例如把 blob 提升为任务工件）。
- `secret` 标签对象只允许显式授权的工具读取；带 `net.egress` 能力的工具不得读取 secret 对象。
- 默认策略（ADR-011）：跨任务禁止自动共享；普通文件默认 `file-snapshot`，需要跟随时显式选择 `file-live`；模型可见 ObjectRef，视图不可见。

### 10.3 视图 API

```wit
record object-ref {
  id: string,            // 128 位随机，任务作用域
  kind: string,          // artifact | blob | file-snapshot | file-live | session
  size: option<u64>,
  label: string,
  hash: option<string>,
  generation: u32,
}

open:    func(req: open-req) -> result<object-ref, error>
attach:  func(object: object-ref, mode: string) -> result<view, error>
read-at: func(view: view, offset: u64, len: u32) -> result<list<u8>, error>
read:    func(view: view, len: u32) -> result<list<u8>, error>
seek:    func(view: view, offset: u64) -> result<(), error>
write:   func(view: view, data: list<u8>) -> result<u32, error>
stat:    func(view: view) -> stat
close:   func(view: view)
export:  func(object: object-ref, new-label: string) -> result<object-ref, error>
```

- 随机访问用 `read-at`，确定性、易重放；顺序读用 `read` 加游标。
- `open` 只登记对象并返回引用；工具随后 `attach` 得到视图。两者可合并为一次便捷调用。
- 视图操作全部经过标签流控；secret 与网络工具组合非法时返回策略错误。
- 每次 `open/attach/export` 写审计事件；读写只记录字节数与对象 ID。
- 对象元数据落 Store：`{id, kind, source, hash, label, owner_task, generation, ttl, refcount}`；内容按 1 MiB 分块、sha256 寻址，LRU 缓存有字节上限。
- 不跨内核重启持久化 OS 文件描述符；重启后按元数据惰性重开。工件与 blob 始终可用；文件快照在源文件变化后标记 stale。

### 10.4 数据标签与流控

| 标签 | 含义 | 规则 |
| --- | --- | --- |
| `public` | 可公开 | 无限制。 |
| `challenge` | 题目工件 | 默认可读，可被有 `net` 能力的工具处理，但记录审计。 |
| `derived` | 工具生成 | 继承输入中最严格的标签。 |
| `secret` | 凭据、flag、笔记 | 禁止流向外网工具与 LLM 上下文；仅允许显式授权的工具。 |

流控在 `data_read`/`http_request`/`session_write` 三个出口执行，而不是在工具内部。

### 10.5 会话与流

- `session` 是 envd 上的持久执行上下文，`session_id` 是可引用的名字而非凭据。
- 内核或 envd 持有数据面连接与有界缓冲；工具按需读取。
- 支持 PTY、stdin 写入、窗口调整、信号、退出码、后台作业。
- 背压使用信用窗口；慢消费者不会导致无界缓冲。

### 10.6 工件存储

- 内容寻址：`sha256` 为键，元数据记录来源、标签、MIME、大小、创建者。
- 大工具输出自动落为工件，模型只看到摘要与引用。
- 工件可被多个任务引用；删除走引用计数。

M0 实现：`msk-data::FileObjectStore` 落盘到 `<root>/artifacts/{objects,meta}`，对象 ID 为 SHA-256；`artifact_read` 支持 offset/limit，UTF-8 片段直接返回，二进制片段以 base64 返回。

### 10.7 数据面帧格式

数据面是传输无关的帧协议，v2 支持三种绑定：

| 绑定 | 用途 | 控制面 | 数据面 |
| --- | --- | --- | --- |
| `embedded-stdio` | 内核拉起 envd 子进程，本地开发或外层沙箱 | CONTROL 帧承载 MCP JSON-RPC | 同一 stdio 管道上的帧 |
| `tls-tcp` | 本地守护、容器、直连远程 | MCP over HTTP | TLS 原始 TCP |
| `wss` | 经 HTTP 反向代理 | MCP over HTTP | WebSocket 二进制 |

`embedded-stdio` 与内核同 uid，不提供权限分离；只允许 `dev` profile 或外层 bwrap/nsjail/容器命名空间。`host`/`isolated` 必须使用 `service` 绑定。

```
magic "MSDP" | version u8 | kind u8 | flags u16 | stream_id u32 | seq u64 | len u32 | payload
```

`kind`：`HELLO`、`OPEN`、`DATA`、`CREDIT`、`SEEK`、`RESIZE`、`SIGNAL`、`EXIT`、`CLOSE`、`ERROR`、`PING`、`PONG`、`CONTROL`。`stream_id=0` 保留给 CONTROL；控制面走 MCP；数据面不承载工具发现与策略。完整定义见 [envd-protocol.md](envd-protocol.md)。

---

## 11. 能力与策略

### 11.1 能力分类

| 能力 | 作用域 | 示例 |
| --- | --- | --- |
| `llm.call` | 内核内部 | 仅内核骨架。 |
| `net.egress` | 工具 / envd | 允许访问的域名、CIDR、端口。 |
| `exec.session` | 工具 | 允许在哪个 env 上创建/操作 session。 |
| `fs.workspace` | 工具 | 只读/读写、路径根。 |
| `fs.host` | 组件 | 受控目录；默认关闭。 |
| `secrets.read` | 组件 | 指定键；默认关闭。 |
| `mcp.connect` | 内核 | 允许连接的 MCP server。 |
| `data.export` | 工具 | 允许写出到外部的数据标签上限。 |

### 11.2 决策流程

1. 工具 manifest 声明所需能力。
2. 安装时展示能力清单与权限，用户确认。
3. 每次调用由内核策略引擎评估：工具、能力、数据标签、目标 env profile、任务预算、用户审批。
4. 结果为 allow / deny / require_approval。
5. 决策与理由写入事件日志；审批请求送达前端。

M0 实现：`msk-core::PolicyTable` 维护“工具 -> 授予能力”，授权结果为 manifest 声明与策略授予的交集；未授予的能力直接拒绝，manifest 不能自授权限。当前策略表内置，后续改为数据驱动并接入审批。

### 11.3 默认值

- 工具默认无网络、无宿主文件系统、无秘密读取。
- envd 默认只接受回环或显式绑定；token 不进入命令环境。
- 危险操作（删除 env、上传到外部、读取 secret）默认需要审批。
- 全局网络开关与每工具网络开关同时存在，二者都开才放行。

---

## 12. Agent 核心

### 12.1 任务与状态机

- **任务（Task）** 对应一个挑战或一个子目标，拥有独立的目标、上下文、workspace 视图、预算、事件流与工件集合。
- **状态机**：

```
Created -> Planning -> Executing -> Waiting -> Reflecting -> Replanning
                                  \-> Done | Failed | Cancelled | Timeboxed
```

- `Waiting` 覆盖工具执行、审批、人工输入、envd 重连。
- 每次状态迁移写入事件日志；崩溃恢复时未完成的工具调用标记为 unknown，可选择重新执行或放弃。
- 任务支持检查点、克隆（fork）、暂停/恢复与报告导出。

### 12.2 步骤与循环

| 步骤 | 内容 |
| --- | --- |
| `Think` | 一次 LLM 调用，产生推理与工具调用。 |
| `Act` | 一批工具调用，可并行，结果顺序确定。 |
| `Observe` | 工具结果归一化、裁剪、落工件。 |
| `Reflect` | 评估进展、证据与风险，决定继续、改计划或提问。 |
| `Ask` | 请求人工输入或审批。 |
| `Checkpoint` | 持久化上下文、笔记、游标与预算。 |

循环：

```
构建上下文 -> Think -> 解析工具调用 -> 策略检查
  -> Act -> Observe -> 更新笔记与工件 -> 预算检查
  -> 需要时 Reflect -> 下一轮
```

每个步骤带 deadline、取消令牌与预算片；工具批次有总超时，单个工具失败不阻塞其他工具。

### 12.3 上下文分层与预算

| 层 | 内容 | 基线比例 |
| --- | --- | --- |
| L0 系统 | 身份、安全规则、工具使用约定、输出契约 | 5–10% |
| L1 任务 | 目标、题型、成功标准、约束、预算 | 10% |
| L2 记忆 | 笔记、发现、假设、待办、flag 候选 | 15% |
| L3 工作 | 最近若干步骤与推理 | 35% |
| L4 观察 | 工具结果（裁剪后） | 25% |
| L5 引用 | 工件与对象引用、环境信息 | 10% |

- 实际预算动态计算：模型有效窗口乘安全系数（默认 0.85）得到总量，再按题型与阶段调整基线比例；计算过程与结果写入事件日志（ADR-016）。
- 题型影响：recon/web 提高 L4 观察，pwn/rev 提高 L5 工件引用，crypto 提高 L3 工作推理；阶段影响：规划与反思阶段提高 L1/L2。
- 固定事实（flag 候选、端点、凭据引用、已确认假设）永不压缩；压缩只作用于观察与历史推理。
- 工具结果先落工件，上下文只保留摘要、状态与引用；模型可按需用 `read-at` 读取片段。
- 在 70% 与 85% 预算处触发压缩；压缩由独立的小模型调用完成，输出固定结构，压缩前后上下文写事件日志。
- 每层分别计量 token；TUI 提供上下文检查器，显示各层占用与被丢弃的内容。

M0 实现：`msk-core::context::ContextManager` 负责 token 估算、pinned facts 与观察压缩；超过 2 KiB 的工具输出写入 `msk-data` 内容寻址工件存储，上下文只保留头尾数字摘要与 ObjectRef。事件 `data.put` 与 `context.state` 暴露给前端。

### 12.4 记忆

| 类型 | 存储 | 内容 |
| --- | --- | --- |
| 工作记忆 | 上下文窗口 | 当前推理与观察。 |
| 情景记忆 | 事件日志 | 完整步骤、工具调用、模型输出。 |
| 语义记忆 | 笔记文件 | 目标、发现、假设、待办、flag 账本。 |
| 程序记忆 | playbook | 题型打法、工具链、常见陷阱。 |
| 工件记忆 | 共享对象 | 文件、输出、截图、转储。 |

- 模型通过 `note` 工具编辑结构化笔记；系统自动抽取候选（URL、IP、哈希、flag 格式），但标记为未验证。
- flag 账本记录候选、来源证据、验证状态；提交前必须通过格式与来源检查。
- 跨任务只共享 playbook 与已验证的方法，不共享原始凭据与 flag；提升需要显式操作。
- 工件默认文件权限保护，可选加密；原始工具输出默认保留 7 天，flag 账本随任务删除。

### 12.5 工具协议

- 工具面保持小而正交：`session`（执行/PTY）、`fs`（对象读写/列目录/搜索）、`http`、`artifact`（摘要/提取）、`note`、`plan`、`ask_human`、`spawn_agent`、`env`（环境生命周期）。
- 统一结果信封：状态、摘要（限 token）、工件引用、是否截断、费用、是否可重试、下一步提示。
- 截断按工具配置：保留头尾、行数与字节上限，明确标记；大结果落工件并提供 `read-at`，绝不静默丢弃。
- 错误结构化：类别、消息、是否可重试、建议动作；不把堆栈或原始二进制放进上下文。
- schema 由 Rust 类型生成；描述里写清何时用、何时不用、示例与安全注意事项；支持按模型覆盖描述。
- 破坏性操作带幂等键；短窗口内相同调用去重；失败结果做负缓存，避免死循环。

### 12.6 规划、反思与卡住检测

- 初始计划：题型分类、可行路线、步骤列表、验证方式；写入待办。
- 反思时机：每 N 步、失败后、费用或延迟异常时。
- Critic：独立调用（可用更便宜的模型）检查证据是否支持结论、是否遗漏验证、是否在重复。
- 卡住信号：重复相同调用、相同错误、无新工件、上下文持续缩小、预算消耗异常。应对：强制反思、切换打法、开并行分支、请求人工。
- 歧义挑战可 fork 多个假设分支，各自预算，按证据合并。
- 每题设置时间盒；到点输出部分报告与未决问题。

### 12.7 多 agent

- 编排者加专项子 agent：web、pwn、rev、crypto、forensics、recon。
- **v1 交付**：编排者、`spawn_agent`、任务树、预算切分、黑板、取消传播、结果合并与冲突标记全部在 v1 实现；reviewer agent 可后置。
- 默认每个挑战最多 2 个并行子 agent，全局上限可配置；子 agent 默认不能继续创建子 agent，防止无限 fork。
- 预算按任务树静态切分，子 agent 可申请追加；编排者按进展重新分配，不做抢占。
- 子 agent 是任务：独立上下文、预算片、workspace 视图与工具白名单；默认无秘密访问权。
- 通信只通过黑板（笔记与共享对象）与结构化交接消息，不共享自由文本上下文。
- 并行度、预算与取消沿任务树传播；工件不可变，笔记按节合并，冲突显式标记。
- 高风险动作可交给 reviewer agent 复核；歧义结论可选辩论，但不作为默认路径。

### 12.8 人在回路与自主级别

| 级别 | 行为 |
| --- | --- |
| `ask` | 所有有副作用的动作都审批。 |
| `assisted` | 默认（ADR-014）；破坏性、出网、读取 secret、花钱类动作审批。 |
| `auto` | 策略内自动执行，仅异常升级。 |

- 审批载荷：工具、参数摘要、目标、能力、数据标签、风险、费用、理由；超时默认拒绝。
- 支持提示、打断、接管、编辑计划；人工输入作为带标签的消息进入上下文。
- 审批疲劳治理：任务内记住、按范围模板化、非破坏性操作批量确认。
- 每个动作可追溯到计划步骤与证据。

### 12.9 失败处理

| 失败 | 处理 |
| --- | --- |
| LLM 限流/网络 | 退避重试、provider 故障转移、熔断。 |
| 上下文溢出 | 压缩、丢弃低价值观察、保留固定事实。 |
| 工具瞬时错误 | 有限重试；否则作为观察返回。 |
| 策略拒绝 | 换路径或请求人工；记录理由。 |
| envd 断连 | 重连、会话重挂、必要时重建环境。 |
| 参数 schema 不合法 | 返回纠正性错误，允许模型修正一次。 |
| 检测到循环 | 强制反思、切换策略、请求人工。 |
| 到时间盒 | 输出部分报告与未决问题。 |

所有失败都成为观察，不允许静默吞掉；重复失败进入负缓存并升级。

### 12.10 预算与调度

- 维度：token、费用、墙钟、工具调用次数、网络字节、envd 资源。
- 每任务与全局双重预算；软限制触发压缩与降级，硬限制取消。
- 模型路由：例行步骤用便宜模型，规划/反思/critic 用强模型；失败升级。
- 调度考虑截止时间、进展与成本效率；在检查点处可抢占。
- 昂贵或破坏性步骤前强制 checkpoint；支持恢复与迁移。
- 费用按步骤、模型、工具记录，进入事件日志与 TUI。

### 12.11 安全与 prompt injection

- 指令层级：系统 > 开发者 > 用户 > 工具数据；工具输出永远是数据，不是指令。
- 防御以能力为主、提示为辅：出网控制、秘密隔离、审批、数据标签，比“请不要听从工具输出”可靠。
- 三要素同时出现时升级审批或阻断：私有数据 + 不可信内容 + 对外通信。
- 挑战文件在沙箱中解析，不在宿主机执行；不可信二进制只在 `isolated` profile 运行。
- 上下文与日志做秘密脱敏；模型只看到引用与标签。
- 输出校验：flag 格式、命令白名单、路径 confine。
- 评测集包含注入样例，作为回归项。

### 12.12 可观测与评测

- 每步记录上下文快照、token、费用、工具延迟、压缩差异、策略决策。
- 支持 replay、re-execute 与 dry-run；固定子集做回归。
- TUI 视图：计划、上下文预算、工件、审批、事件树、成本。
- 评测按题型统计解决率、耗时、费用、工具错误率、人工干预次数。

### 12.13 CTF 专项

- 题型分类与 playbook：recon、web、pwn、rev、crypto、forensics、misc。
- flag 账本、格式校验、来源证据；提交动作按平台能力决定是否自动化。
- 每题可丢弃环境；环境重置与工件保留策略。
- 团队模式：多个 agent 并行不同题目，共享黑板但隔离秘密。
- 从事件日志生成 writeup 草稿（可选）。

### 12.14 策略接口

- `AgentStrategy` 是内核内部 trait，内置 ReAct、Plan-Execute、CTF-specialist 三种实现，由配置选择。
- 策略只决定下一步，不接触策略引擎、能力管理与审计。
- v1 保持内核内部 trait，不开放为组件端口；替换策略通过配置选择内置实现。

---

## 13. LLM Provider

- 归一化消息、工具 schema、流式增量、用量与错误分类。
- 错误分类：可重试（网络、限流）、认证、上下文溢出、协议错误、取消。
- 重试与退避在内核；provider 只负责单次调用。
- provider 差异尽量数据化：endpoint、header、请求/响应映射、模型元数据作为配置；传输与流式逻辑保持静态。
- 支持多 provider 与故障转移；选择策略属于内核配置，不属于组件。
- API key 只存在于 SecretStore 与 provider 调用路径，不进入工具、不进入模型上下文、不写日志。

```rust
pub trait LlmProvider: Send + Sync {
    fn id(&self) -> &str;
    fn capabilities(&self) -> LlmCapabilities;
    fn stream(&self, request: LlmRequest, sink: &mut dyn LlmSink,
              cancel: CancelToken) -> BoxFuture<'_, Result<LlmUsage, LlmError>>;
}
```

---

## 14. MCP 客户端与 envd

### 14.1 分层

- 内核是 MCP 客户端，envd 是 MCP 服务端。
- 控制面：`initialize`、`tools/list`、`tools/call`，承载环境生命周期、一次性命令、文件元数据、端口转发管理。
- 数据面：session、PTY、大文件、流式 I/O，走第 10 节的二进制通道。
- 大文件也可走短期签名链接，但链接不携带主 token，且受来源与 SSRF 规则约束。

### 14.2 envd v2 契约（待实现）

> 接口、数据面帧、会话状态机与错误模型的完整草案见 [envd-protocol.md](envd-protocol.md)。

**环境与生命周期**

- `env_create/list/clone/delete/info`；环境类型：managed workspace、mounted 目录。
- 每环境配额：CPU、内存、磁盘、进程数、带宽、并发 session。
- 每环境出网策略；默认拒绝，按需放行。
- 环境状态与审计事件可查询。

**会话**

- `session_create`：shell、cwd、env、PTY 尺寸、用户、超时。
- `session_read/write/resize/signal/close`：流式、二进制安全、背压。
- `session_list/kill`：后台作业与清理。
- `exec`：一次性命令，保留为便捷接口；超时终止进程树。

M0 实现：envd 支持 `pty=true` 的 PTY 会话（Windows ConPTY、Unix PTY），提供 `session_write/session_read/session_resize`；`session_exec` 仅用于管道会话，避免终端回显把标记行误判。

**文件**

- `read_file/write_file/list_dir/search/stat`；基于 `os.Root` 的 confine，拒绝绝对路径、`..`、逃逸符号链接。
- 原子替换；大文件走数据面或签名链接。
- 删除、覆盖等破坏性操作默认需要内核侧审批。

**端口转发**

- `forward_create/list/close`：本地到远端、远端到本地；生命周期绑定 session 或 env。

**认证与权限**

- 每 agent 独立 token，带作用域与 TTL；支持撤销。
- token 通过环境变量注入 envd，不写入文件、不出现在命令环境、不可被命令用户读取。
- 支持 mTLS（可选）；TLS 终止由反向代理或内建完成。
- 速率限制、失败冷却、来源限制。

**Profile**

| profile | 执行方式 | 默认用途 |
| --- | --- | --- |
| `isolated` | 容器 / VM / gVisor；只读根、cap drop、no-new-privileges | 运行不可信挑战二进制 |
| `host` | 专用 uid/gid；路径 confine；最小 capability | 受控主机、CTF 工具链 |
| `dev` | 回环、放宽限制、显式标记 | 本机开发 |

`env_info` 返回 profile 与 capabilities；内核据此限制工具（例如不可信二进制仅允许在 `isolated` 中运行）。

**审计**

- 结构化事件：谁、在哪个 env、执行了什么、影响了哪些路径与端口、结果码。
- 不记录命令输出内容；输出由内核事件日志按标签决定是否留存。

**跨平台**

- Linux/macOS：`/bin/sh` 或显式 shell；进程组终止。
- Windows：原生 envd 作为 Tier 2 支持；shell 优先 `pwsh.exe`，回退 `powershell.exe`/`cmd.exe`；PTY 使用 ConPTY，进程树用 Job Object，信号映射为 console control events；`isolated` 仍需 WSL2/容器，`host`/`dev` 可原生。能力差异通过 `env_info` 报告。
- Termux/移动端作为可选部署，不进入 v1 主路径。

### 14.3 部署形态

| 形态 | 传输 | profile | 说明 |
| --- | --- | --- | --- |
| 嵌入 | `embedded-stdio` | `dev` 或外层沙箱 | 内核以子进程启动 envd，无端口、无 token；同 uid，不提供权限分离；EOF 时清理会话。 |
| 本地守护 | MCP HTTP + `tls-tcp` | `host` / `isolated` | 随内核发行，独立进程；token、重连、多客户端。 |
| 容器 | MCP HTTP + `tls-tcp`/`wss` | `isolated` | envd 作为入口进程；只读根、cap drop、no-new-privileges。 |
| 远程 | MCP HTTP + `wss` | `host` / `isolated` | TLS + 反向代理；按 server 配置连接。 |

- `embedded-stdio` 只用于本地开发或已处于 bwrap/nsjail/容器命名空间中的场景；`host`/`isolated` 不允许。
- 容器与远程必须使用 `service` 绑定，保留 token、重连与多客户端。
- 沙箱套件作为可选安装项，但安装器默认提供 `isolated` profile；v1 包含容器与 bwrap/nsjail，gVisor/microVM 作为后续 profile。

### 14.4 命名

`envd` 与 Docker、Microsoft 的 envd 冲突；抽离为独立项目前改名，协议文档与工具 schema 均使用新名。

---

## 15. 事件日志、重放与评测

### 15.1 事件模型

追加式 JSONL（大负载存为 blob，事件只存引用）：

```json
{"id":"01J…","ts":"2026-11-14T12:00:00Z","task":"…","kind":"tool.call",
 "actor":"llm","parents":["01J…"],"data":{"tool":"…","args_ref":"blob:…"},
 "policy":{"decision":"allow","rule":"…"}}
```

事件类型：`task.*`、`llm.request/response/delta`、`tool.call/result`、`policy.decision`、`approval.*`、`data.open/close`、`session.*`、`envd.*`、`component.load`、`error`。

- 默认脱敏：API key、token、secret 标签内容不落盘。
- 每个事件带因果父节点，支持构建调用树。
- 索引写入 Store；原始日志按任务分片。

### 15.2 重放

- `record`：正常执行并记录。
- `replay`：使用记录的模型流与工具结果，不再调用外部；用于 UI 回放与回归。
- `re-execute`：复用模型输出，重新执行工具；用于验证工具或 envd 变更。
- `dry-run`：只做策略评估，不执行。

重放要求工具无状态、状态在会话/工件中；这也是第 9 节实例模型的理由。

### 15.3 评测

- 基线：Cybench、NYU CTF Bench、picoCTF。
- 指标：解决率、耗时、token/费用、工具错误率、人工干预次数。
- 每次架构或 prompt 变更跑固定子集，结果进事件库；回归以 replay 为主。

---

## 16. 前端与 IPC

### 16.1 内核形态

- 内核是守护进程；前端通过本地 IPC 连接。
- Unix：UDS，权限 0600，校验 peer credentials。
- Windows：命名管道，限制 DACL；可选本地 token。
- 协议：版本化 JSON-RPC；`hello` 协商版本与能力；事件通过订阅推送。

### 16.2 TUI（v1）

- 视图：任务列表、对话、工具活动、session 终端、工件、审批、事件检查器、成本面板。
- 终端：PTY 输出经数据面流入，支持输入、缩放、中断。
- 审批：阻塞式对话框，显示工具、能力、目标、数据标签、策略理由。
- 多任务并行可见；每任务独立状态。

### 16.3 GUI（后续）

- 复用同一 IPC；候选 Tauri、egui、iced、slint。
- 不与内核链接；GUI 语言可独立选择。
- 远程前端通过 TLS 网关接入，认证与授权由内核负责。

M0 实现：`mskd` 在 `<root>/kernel.sock`（Windows 命名管道）上提供换行分隔 JSON-RPC，token 存于 `<root>/ipc.token`。`mskctl` 支持 list/create/state/cancel/watch 与 terminal-open/write/read/resize/close；`msk-tui --connect <root>` 接任务列表、事件流和取消，`t` 打开 daemon 代理的 PTY 终端（`Ctrl+Q` 脱离），并用 `vt100` 状态机渲染颜色、光标和备用屏幕。终端数据面使用独立本地套接字 `<root>/kernel.data`（Windows 命名管道），帧为 `MSKD`，支持 DATA/CREDIT/RESIZE/EXIT/CLOSE 和基于 credit 的背压；`mskctl terminal-stream` 和 TUI 终端走这条路径。任务当前以受监管的 `msk` 子进程运行；审批端点为占位。

---

## 17. 配置与组件集

- 配置来源：默认值、用户配置文件、项目配置、命令行；后者覆盖前者。
- 组件集记录：组件 ID、版本、哈希、端口、签名发布者。
- 秘密只存 SecretStore；配置文件中只允许引用键名。
- 安全声明随组件集与 envd profile 生成，并在 TUI 中可见。
- 示例组件集：

```toml
[profile.default]
components = [
  { id = "msk.llm.anthropic", version = "0.3.0" },
  { id = "msk.transport.http", version = "0.3.0" },
  { id = "msk.store.sqlite", version = "0.3.0" },
]

[profile.default.envd]
server = "local"
profile = "host"
```

---

## 18. 安全声明矩阵

| 保证 | 仅第一方组件 | + 已审第三方 | + 未审第三方（进程内） | envd isolated | envd host |
| --- | --- | --- | --- | --- | --- |
| WASM 工具隔离 | 是 | 是 | 是 | 是 | 是 |
| 秘密不进入模型上下文 | 是 | 是 | 否（组件可读进程内存） | 是 | 是 |
| 出网策略对工具有效 | 是 | 是 | 否（组件可绕过） | 是 | 是 |
| 内核崩溃隔离 | — | — | 否 | 是 | 是 |
| 不可信二进制隔离 | — | — | — | 是 | 否 |
| 供应链可追溯 | 是 | 是 | 是（签名） | 是 | 是 |

结论：安全声明必须按组件集与 envd profile 分别陈述；不能把“仅第一方”的保证套用到含未审组件的运行上。

`embedded-stdio` 不提供权限分离：envd 与内核同 uid，命令可访问内核进程可见的资源。它只允许在 `dev` profile 或外层沙箱中使用；上表 envd 行的保证不适用于 `embedded-stdio`。

---

## 19. 里程碑

### M0：垂直切片（约 2–3 周）

- 内核骨架 + TUI + 一个 LLM provider + 事件日志。
- 本地 envd（Go）以嵌入式 stdio 启动，`exec` 与文件读写。
- 一个 WASM echo 工具走通：LLM 调用 -> 策略 -> 工具 -> envd -> 结果 -> 日志。
- CI 覆盖 Linux 与 Windows 的编译、单元测试与冒烟。
- 退出标准：一条命令端到端可追踪、可回放（基础版）；两平台都能启动内核与 TUI。

### M1：Agent 核心、能力与数据通道

- 任务状态机、检查点、分层上下文与动态预算、笔记与 flag 账本。
- 工具结果信封、截断与工件化、共享对象与视图、审批流与数据标签。
- session 一次性 exec + 增量读取。
- 退出标准：不可信工具无法读取 secret、无法越权出网；大输出不撑爆上下文；失败可继续。

### M2：内核组件 ABI 与加载器

- C ABI、manifest、签名、信任库、conformance harness、加载流水线。
- 一个第三方示例组件；组件宿主进程原型。
- 退出标准：替换 LLM provider 无需重编内核；未签名/未通过 conformance 的组件被拒绝。

### M3：会话与数据面

- PTY、stdin、信号、resize、后台作业、端口转发、每环境配额。
- envd v2 契约冻结；数据面协议实现。
- Windows 原生：ConPTY、Job Object、路径与 ACL；`host`/`dev` profile。
- 退出标准：交互式 shell 与反向连接类任务可用；断线重连不丢会话；Windows 原生会话通过冒烟。

### M4：多 agent

- 编排者、专项子 agent、`spawn_agent`、任务树、预算切分、黑板、取消传播。
- 结果合并与冲突标记；secret 隔离；reviewer agent 原型。
- 退出标准：多个挑战并行推进，预算与并发受控，子 agent 不越权。

### M5：重放与评测（v1）

- replay / re-execute、评测 harness、基线数据。
- 退出标准：固定子集可重复运行；架构变更可量化对比；v1 功能冻结。

### M6：GUI 与远程（v1 后）

- GUI 客户端、远程网关、多前端并发。
- 退出标准：GUI 不修改内核代码即可接入。

### 实施进度（P1 首个切片，已落地）

- `runtime.rs` 汇集 host bridge、工具注册表、LLM 请求构造、观测工件化与占位符替换；一次性 demo 与常驻会话共用同一套 WASM/策略边界。
- `agent.rs` 提供 Session/Turn 引擎；`msk --session` 常驻读取 stdin，`--stdio-json` 输出严格 JSON 协议；工具 trap、工具返回错误、占位符缺失都成为观测而非终止进程，provider 错误只结束当前 turn。
- `mskd` 支持 `task.create {"interactive":true}`、`task.message`、`turn.end` 通知；任务在 `waiting_input` 与 `running` 之间切换，`mskctl create --interactive` / `send` / `watch` 可用。
- 前置项：连接级认证（hello 前拒绝其他方法、常量时间 token 比较）、事件 `schema_version` + 进程内单调 `seq`、成对工具调用/结果压缩、基于时间戳的任务 ID。
- 协作式取消：stdin 读取线程设置 `CancelToken`；引擎在模型/工具步骤之间检查，`turn.end` 返回 `interrupted`；被打断的 assistant tool_calls 仍补齐每个结果，保证下一轮请求合法。`mskctl interrupt` 与 `/interrupt` 已接入。
- 对话式 TUI：任务列表、按任务过滤的转录、单行输入编辑器、流式 `agent.text` 累积、`turn.end` 系统行、`s` 中断、`n` 新建交互任务；右侧保留事件流用于调试。
- 三态策略与审批：`allow`/`ask`/`deny`；`exec`、`session_exec`、`session_write` 在交互任务中默认 `ask`。agent 通过 stdout `approval.request` 阻塞等待，mskd 转为通知并由 TUI 模态框或 `mskctl approve/deny` 回写 `approval.respond`；拒绝会变成成对的错误观测，模型可继续。`--approvals allow` 用于无人值守。
- 事件完整性与恢复：`msk-replay` 校验 schema/seq/重复 ID/父链接并输出转录；`msk --session --resume <events.jsonl>` 将截断后的历史转录固定为新会话上下文，并显式标记旧 env/session 已失效。`turn.start` 现在记录用户原文，使日志成为完整对话来源。原生检查点在每轮后原子写入 `<root>/checkpoint.json`（Unix 0600），保存完整消息与 tool call/result 配对；`--resume-checkpoint` 在新 env 下恢复上下文、继续 turn 编号并标记旧会话失效。
- 组件签名（M2 前半）：`ComponentManifest` 带 Ed25519 签名，签名覆盖去掉签名字段的规范化 JSON；`TrustStore` 按发布者显式信任公钥；工件校验 SHA-256 并拒绝越界路径。
- 组件宿主（M2）：`libloading` 解析 `ms_component_entry`，先验签/信任、再校验工件哈希与路径，校验总 `struct_size`、ABI、port、vtable 头/保留槽后才复制指针；`init` 恰好一次，调用串行化，`shutdown` 仅在成功 init 后调用一次，库永不卸载。宿主带分配台账，响应先复制再释放，越权/未登记指针拒释放。`msk --provider component --component-manifest ...` 与 mskd 转发已可用；`msk-component-host` 提供进程外运行，组件 abort 只杀死 host，内核收到协议错误并继续。测试 cdylib 覆盖正常响应、空/越界/未登记/错误/panic/abort 模式。C header、host 重启/资源限制/syscall 沙箱、TOCTOU 加固与流式槽仍属后续（见 `docs/components.md`）。
- envd 服务模式（P5）：`--service` 提供 MCP JSON-RPC over HTTP(S)，Bearer token 与 TLS 1.3；数据面为 TCP/TLS 上的 MSDP `HELLO/OPEN/DATA/RESIZE/CLOSE/EXIT`，直接引用控制面创建的 `session_id`。输出以绝对字节 `seq` 寻址，会话保留有界 replay log；输出 CREDIT 限流（初始 64 KiB、DATA ≤64 KiB、4 MiB cap），`OPEN since_seq` 支持重连并返回 GAP/STALE。数据视图附加期间独占输出，断开后控制面继续收到新输出。Rust `EnvDataClient` 已接入 mskd 的 `--envd-data-addr`（明文 TCP）；TLS/WSS 数据面、输入 CREDIT 与 `SEEK` 仍待接。
- 远程 envd 客户端：`msk --envd-url/--envd-token` 通过 HTTP(S) 调 MCP，`EnvTransport` 抽象让 embedded 与 service 共用同一 WASM 宿主桥；已用服务模式完成 headless 会话、远程 `exec`、mskd 任务与 daemon 终端；断线重连与数据面重连游标待接。
- 文件与网络工具：`read_file/write_file/list_dir/stat_file` 经 envd 的 `fs.workspace` 能力执行，写入默认审批；`http_request` 在宿主侧执行 HTTP(S)，受 `net.egress` 能力约束，仅允许 http/https、默认 30s 超时、4 MiB 响应上限并跟随重定向。SSRF/私网访问目前不做内建阻断，应由部署网络策略或沙箱负责。
- 秘密存储：新增 `msk-secret`，`EnvSecretStore` 将 `$secret:name` 映射到 `MSK_SECRET_NAME`；`http_request` 在宿主侧完成 `secrets.read` 能力检查后解析头部，明文不进入模型观测或事件日志。
- 多 agent（M4 首个切片）：`spawn_agent` 通过控制面创建 headless 子任务，`agent_result` 轮询 `task.result` 并返回状态与转录；两者受 `agent.spawn` 能力约束，spawn 默认审批，子 agent 以委派策略 `allow` 运行，深度上限 2。`task.result` 从事件日志重建转录，`mskctl create --headless` / `result` 可直接使用。
- 验收：同一会话连续两轮消息得到两次 `turn.end`，第二轮请求携带第一轮上下文；真实 mock provider 的 echo 工具调用跨轮保留；工具 trap 后该轮仍能收到下一次模型响应；审批通过后命令实际执行，拒绝后下一轮仍可用；父 agent 能 spawn 子 agent 并取回其转录；`msk --envd-url` 可对远程 envd 执行会话与命令。
- 尚未进入本切片：envd 输入 CREDIT/WSS/SEEK 与 Rust 原生数据客户端、检查点级工具状态恢复、子 agent 结果合并/黑板与并发预算、进程外组件宿主。

---

## 20. 开放问题与决议

仍待验证：

1. wasmtime 的 pooling allocator 与内存上限在长会话下的实测表现（M1/M3 基准）。

已决议：

- MoonBit：v1 使用 core module + 线性内存 JSON 信封，WIT/component model 作为规范目标；MoonBit 支持稳定后切换（ADR-019）。
- 第三方组件：默认进程外宿主；仅第一方或已审阅组件允许进程内（ADR-020）。
- 嵌入模式：必须显式 `--embedded-sandboxed`；自动检测只作为提示（ADR-021）。
- envd：先冻结 v2 协议再抽离仓库；公开发布前改名；协议版本与 MCP revision 解耦。
- Windows：`pwsh` > `powershell` > `cmd`；无 ConPTY 用 pipe 并声明 capability（ADR-018）。
- 多 agent：v1 每挑战最多 2 个子 agent，全局上限，静态预算切分，按进展再分配。
- 沙箱套件：v1 容器 + bwrap/nsjail；gVisor/microVM 作为后续 profile（ADR-022）。
- 工件与 flag：文件权限 + 可选加密；原始输出默认保留 7 天，flag 账本随任务删除。
- GUI：M6 再选型。

此前的共享对象、自主级别、多 agent v1、动态预算、双模式数据面决议见 ADR-011/014/015/016/017。

---

## 21. ADR 索引

| ADR | 决策 | 状态 |
| --- | --- | --- |
| ADR-001 | 内核使用 Rust | 接受 |
| ADR-002 | 工具仅用 WASM，无 native 工具 | 接受 |
| ADR-003 | 内核组件为签名 C ABI 动态库，加载期加载、不可卸载 | 接受 |
| ADR-004 | 端口/适配器模型，骨架不可替换 | 接受 |
| ADR-005 | 控制面 MCP + 二进制数据面 | 接受 |
| ADR-006 | 所有执行经 envd，包括本地嵌入与本地守护 | 接受 |
| ADR-007 | 前端为内核客户端，默认 TUI | 接受 |
| ADR-008 | 事件日志与重放为一等公民 | 接受 |
| ADR-009 | 数据访问采用共享对象 + 视图两层模型；模型只见 ObjectRef | 接受 |
| ADR-010 | 安全声明按组件集与 envd profile 分别陈述 | 接受 |
| ADR-011 | 共享对象默认任务内共享，跨任务需显式 export | 接受 |
| ADR-012 | Agent 以任务状态机、分层上下文与统一结果信封为核心 | 接受 |
| ADR-013 | 工具结果大者落工件，上下文只保留摘要与引用 | 接受 |
| ADR-014 | Agent 默认自主级别为 assisted | 接受 |
| ADR-015 | 多 agent 进入 v1 | 接受 |
| ADR-016 | 上下文预算按模型窗口与题型动态计算 | 接受 |
| ADR-017 | 数据面采用 embedded-stdio + service 双模式；stdio 仅用于 dev/外层沙箱 | 接受 |
| ADR-018 | 跨平台分级：Linux Tier 1、Windows Tier 2 原生、macOS Tier 3；WASM 跨平台，差异在 host imports | 接受 |
| ADR-019 | MoonBit v1 使用 core module + JSON 信封；WIT/component model 为规范目标 | 接受 |
| ADR-020 | 第三方内核组件默认进程外宿主，进程内仅限第一方/已审阅 | 接受 |
| ADR-021 | embedded-stdio 必须显式声明外层沙箱；自动检测仅提示 | 接受 |
| ADR-022 | 沙箱套件 v1 为容器 + bwrap/nsjail；gVisor/microVM 后续 | 接受 |

---

## 附录 A：端口 trait 草案

```rust
pub trait LlmProvider: Send + Sync {
    fn id(&self) -> &str;
    fn capabilities(&self) -> LlmCapabilities;
    fn stream(
        &self,
        request: LlmRequest,
        sink: &mut dyn LlmSink,
        cancel: CancelToken,
    ) -> BoxFuture<'_, Result<LlmUsage, LlmError>>;
}

pub trait Store: Send + Sync {
    fn put_event(&self, event: &Event) -> Result<(), StoreError>;
    fn get_task(&self, id: TaskId) -> Result<Option<Task>, StoreError>;
    fn put_artifact(&self, meta: &ArtifactMeta, reader: &mut dyn Read)
        -> Result<ArtifactId, StoreError>;
}

pub trait EventSink: Send + Sync {
    fn emit(&self, event: Event) -> Result<(), SinkError>;
}

pub trait SecretStore: Send + Sync {
    fn get(&self, key: &SecretKey) -> Result<Secret, SecretError>;
    fn set(&self, key: &SecretKey, value: &Secret) -> Result<(), SecretError>;
}
```

## 附录 B：WASM 工具 WIT 草案

```wit
package msk:tool@1.0.0;

interface host {
  record open-req {
    kind: string,          // "file" | "artifact" | "blob" | "session"
    path: option<string>,
    mode: string,          // "r" | "w" | "rw"
    live: bool,
  }

  record object-ref {
    id: string,
    kind: string,
    size: option<u64>,
    label: string,
    hash: option<string>,
    generation: u32,
  }

  resource view {
    read-at: func(offset: u64, len: u32) -> result<list<u8>, string>;
    read: func(max: u32) -> result<list<u8>, string>;
    write: func(data: list<u8>) -> result<u32, string>;
    seek: func(offset: u64) -> result<(), string>;
    close: func();
  }

  data-open: func(req: open-req) -> result<object-ref, string>;
  data-attach: func(object: object-ref, mode: string) -> result<view, string>;
  data-export: func(object: object-ref, new-label: string) -> result<object-ref, string>;

  record exec-req {
    cmd: list<string>,
    cwd: option<string>,
    env: list<tuple<string, string>>,
    pty: bool,
    timeout-ms: option<u64>,
  }

  session-open: func(req: exec-req) -> result<string, string>;
  session-read: func(id: string, max: u32) -> result<list<u8>, string>;
  session-write: func(id: string, data: list<u8>) -> result<u32, string>;
  session-close: func(id: string) -> result<(), string>;

  http-request: func(method: string, url: string, headers: list<tuple<string, string>>,
                     body: list<u8>) -> result<list<u8>, string>;

  log: func(level: u32, msg: string);
  progress: func(done: u64, total: option<u64>, note: string);
  cancelled: func() -> bool;
}

world tool {
  import host;
  export run: func(input: list<u8>) -> result<list<u8>, string>;
}
```

## 附录 C：默认资源限额

| 资源 | 默认值 | 可配置 |
| --- | --- | --- |
| WASM 实例内存 | 64 MiB | 是 |
| 单次工具墙钟 | 30 s（可通过 progress 延长） | 是 |
| 单次工具输出 | 1 MiB 内联，超出落工件 | 是 |
| 每任务并发工具调用 | 4 | 是 |
| 全局并发工具调用 | 16 | 是 |
| 每任务 token 预算 | 按配置 | 是 |
| envd session 数 | 每环境 8 | 是 |
| 数据面单帧 | 8 MiB | 是 |
| 事件日志 blob | 单文件 1 GiB | 是 |
