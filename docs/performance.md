# 性能基准（M0）

> 环境：Windows 11，Rust 1.100（release），wasmtime 49.0.5，Go 1.27.4，
> Python 3.12.11，Node 24.16.0。单机单次采样，数量级参考，不做统计显著性声明。
> 复现命令见文末。

## 结论先行

1. **WASM 嵌入式调用非常便宜**：echo 与带 host bridge 的调用都在 25 µs/次
   左右（24–28 µs 波动）。相比启动一个 Python/Node 进程（25.7 ms / 67.3 ms），
   小工具反复调用时差距是千倍级。
2. **CPU 密集计算**：WASM 在这台机器上比 Node 快约 2.4 倍，比 CPython 快约
   300 倍。但 Python/Node 的真实工具大量依赖 C 扩展（hashlib、numpy、crypto、
   JSON），差距会显著缩小。
3. **I/O 与进程型工具**：一次 `cmd.exe /C echo` 的 OS 进程创建约 13.8 ms，
   经 WASM → host → envd 完整链路约 13.9–15.6 ms，栈本身只增加约 1–2 ms。
   改用持久 shell 会话后降到 **0.28–0.30 ms/次**（50 次平均），约 50 倍提升。
   这类场景的瓶颈在进程/网络，会话复用比换语言有效得多。
4. **agent 层面**：LLM 往返通常是数百毫秒到数秒，工具语言差异被淹没；上下文
   管理、工具输出裁剪和会话复用更值得优化。

## 数据

| 项目 | 结果 | 说明 |
| --- | --- | --- |
| WASM echo 调用 | 24–28 µs/次 | 含 Store 创建 + 实例化 + 调用；首轮 18 µs |
| WASM host bridge 调用 | 24–25 µs/次 | 含能力上下文与 `msk.tool_call` 回调 |
| 模块加载（WAT 编译） | 1.9–3.3 ms | 每进程一次 |
| WASM CPU：sum 0..20M | 5.84 ms | i64 循环，wasmtime JIT |
| Node CPU：sum 0..20M | 13.84 ms | Number 循环 |
| Python CPU：sum 0..20M | 1840 ms | CPython for-range |
| 直接进程创建 | 13.77 ms/次 | `cmd.exe /C echo bench` |
| WASM → envd exec | 15.13 ms/次 | 含 WASM、帧、Go server、进程创建 |
| WASM → envd session_exec | 0.28–0.30 ms/次 | 复用持久 shell，50 次平均 |
| Python 进程启动 | 25.69 ms | `python -c pass` |
| Node 进程启动 | 67.33 ms | `node -e ""` |
| msk-bench 进程 + 引擎 + 编译 | 17.36 ms | 进程启动 + wasmtime 引擎 + WAT 编译 |

CPU 基准的校验和一致：`n=20,000,000` 时 sum 为 `199999990000000`。

能力门控版本复测：echo 24–28 µs、host bridge 24–25 µs、WASM → envd exec
15.6 ms。与上表首轮的差异在测量噪声与 CPU 状态范围内；能力上下文每调用克隆
一次，成本远小于测量波动。

## 如何解读

### WASM 的优势在哪里

- **高频小调用**：每次调用约 25 µs，不需要 fork、不需要解释器启动、不需要
  IPC 建连。适合解析、编码、协议帧、结果裁剪这类“胶水”工具。
- **CPU 密集的胶水**：WAT/ Rust 编译到 WASM 后由 Cranelift JIT 执行，紧循环
  接近原生；在这个循环里比 Node 的 JIT 更快。
- **内存上限**：每个实例 64 MiB，比 Python/Node 的运行时基线更可控（本表未测
  RSS，仅指限额）。

### WASM 不自动更快的地方

- **Python/Node 的 C 扩展**：`hashlib`、`cryptography`、`numpy`、Node 的
  `crypto`/`JSON` 都是原生代码。用 WASM 重写这些未必更快，而且生态成本很高。
  这类工作应该继续放在 envd 里用现成工具跑。
- **进程/网络型工具**：nmap、sqlmap、gdb、curl 的耗时在进程创建、网络往返和
  目标响应，WASM 只是把“调用”包了一层，收益接近零。
- **模块规模**：每次调用新建 Store，实例化成本随模块大小和 import 数量增长。
  M0 的 echo/exec 模块很小；真实工具需要模块缓存（预编译产物）和基准数据。
- **fuel 限额**：默认 5000 万 fuel 在 2000 万次循环时耗尽。CPU 型工具必须按
  工作量配置 fuel/时间预算，否则会把正常工具判成 trap。这是可用性问题，不只
  是性能问题。

### 架构上真正该优化的

1. **envd 会话复用（已实现）**：每次 exec 创建进程，Windows 上约 13.8 ms；持久
   shell 的 `session_exec` 实测 0.28–0.30 ms/次，约 50 倍提升。下一步是 PTY、并发
   会话隔离和后台作业。
2. **减少层数与序列化**：一次工具调用经过 WASM、帧、MCP、Go、进程；其中进程
   创建占绝对多数。优先做 session，而不是微调 JSON。
3. **模块缓存**：按内容哈希缓存编译产物；避免每次进程启动都编译 WAT/WASM。
4. **按工具配置限额**：fuel、内存、输出、并发分别配置；CPU 型工具需要更高的
   fuel，I/O 型工具需要更长的墙钟超时。
5. **真实工作负载基准**：下一步用 ELF/PE 解析、pcap 过滤、正则、JSON 处理
   各做一个基准，再决定哪些工具值得 WASM、哪些交给 envd 里的 Python。

## 复现

```sh
cargo build --release -p msk-kernel --bin msk-bench
target/release/msk-bench wasm-echo --iterations 5000
target/release/msk-bench wasm-host --iterations 5000
target/release/msk-bench compile
target/release/msk-bench wasm-cpu --n 20000000 --iterations 5
target/release/msk-bench direct-exec --iterations 10
target/release/msk-bench envd-exec --envd target/debug/envd --iterations 10
target/release/msk-bench session-exec --envd target/debug/envd --iterations 50
python bench/cpu.py 20000000
node bench/cpu.js 20000000
```

启动开销可以用 PowerShell 计时：`python -c pass`、`node -e ""`、
`msk-bench compile` 各重复 10 次取平均。

## 已知偏差

- 单机、单次、Windows；Linux 上进程创建更快，WASM/Node 的相对关系可能变化。
- `msk-bench` 是 release；debug 构建的 wasmtime 会慢很多，不能用 debug 数据下结论。
- CPU 基准是紧循环微基准，不代表解析/哈希/正则等真实负载。
- 未测峰值内存；只有 64 MiB/实例的配置上限。
- envd 使用 Go 默认优化构建，未做专门调优。
- 会话基准使用持久 `cmd.exe`/`/bin/sh`，命令通过标记行同步；Windows 上含
  `&`、括号等复杂命令可能破坏包装，后续版本会换用更稳健的协议。
