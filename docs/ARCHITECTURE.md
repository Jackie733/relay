# Relay 技术架构

> 版本：0.1-draft · 2026-10-20 · 这是设计提案，不代表依赖已验证或代码已实现。

## 1. 架构目标与不可妥协的边界

1. SSH Shell 是**长期存在的 PTY 会话**，而非执行一次 `exec()` 后渲染输出。
2. Transfer Engine 必须与 TUI 完全解耦；后台文件复制不得阻塞界面事件循环。
3. 任意文件端点是 `(HostId, Path)`，而非写死为“本地栏与远程栏”。
4. 初期依赖已有的 OpenSSH/SFTP 服务，服务器端无需安装 Relay Agent。
5. 明确区分 SSH 传输网络、SSH 身份认证以及文件传输路由。
6. 不为了跨协议兼容性牺牲 SSH/SFTP 主路径的安全和可测性。

## 2. 模块结构

```text
relay/
├── Cargo.toml             # workspace
├── crates/
│   ├── relay-core/        # HostId / Endpoint / Domain events / errors / config
│   ├── relay-ssh/         # SSH config/auth/key verification/session/PTY/SFTP
│   ├── relay-transfer/    # plans, scheduler, streaming, metadata, validation
│   ├── relay-tui/         # app state, event loop, widgets, screens, focus
│   └── relay-cli/         # optional CLI frontend (later)
└── tests/
    └── integration/      # SSH/SFTP + network failures
```

可以先用一个 Workspace 管理上述五个小 crate；首期不引入动态插件、远端守护进程及事件溯源系统。

### 2.1 依赖候选

| 模块 | 技术 | 理由 / 待验证点 |
| --- | --- | --- |
| TUI | Ratatui + Crossterm | TUI 渲染、终端输入与大小变化 |
| Async | Tokio | SSH、文件 IO、任务队列与取消 |
| SSH | russh | 原生 Rust SSH 客户端、PTY Channel |
| SFTP | russh-sftp（候选） | 与 russh 配套；实际 API 在 P0 锁定 |
| Terminal Render | tui-term + VT parser | 将远程 VT 控制流渲染于 Ratatui 面板 |
| Config | ssh2-config / OpenSSH 辅助解析 | 常见 Host 配置，复杂语义需兼容验证 |
| Serialization | Serde + TOML | 应用偏好及独立保存主机条目 |
| Errors | thiserror / anyhow | 结构化底层错误与边界聚合 |
| State persistence | SQLite（Later） | 传输任务与重启后的恢复 |

**依赖版本不要直接按本文照抄。** 需在创建 `Cargo.lock` 时确认 Rust MSRV、SSH 实现兼容性、维护状态及许可证。

## 3. 逻辑关系

```text
                  ┌────────────── Relay TUI ──────────────┐
                  │ Files  Terminals  Transfers  Hosts   │
                  └──────────────┬───────────────────────┘
                                 │ command / event
                     ┌───────────▼───────────┐
                     │ App State / Dispatcher │
                     └────┬───────────┬───────┘
                          │           │
             ┌────────────▼──┐   ┌───▼────────────────┐
             │ SessionManager│   │ TransferScheduler  │
             │ SSH / PTY     │   │ bounded tasks      │
             └───────┬───────┘   └───┬────────────────┘
                     │               │
             ┌───────▼───────────────▼──────┐
             │ FileProvider / Connection    │
             │ LocalFs | RemoteSftpFs        │
             └──────────────────────────────┘
```

### 3.1 SessionManager

按 HostId 管理 SSH 连接状态、认证、PTY Session 和 SFTP 访问。一个 Host 允许多个终端 Tab。SSH 的多个 Channel 可以共享一个底层 Transport，但长传输可能影响交互延迟：初期设计支持按需建立**独立 SFTP 连接或会话池**，不强制终端与传输共用单一物理连接。

连接失效后，不得在 UI 中无提示地将旧远程进程解释为已恢复。重连可以重新建立连接与 Shell，但**不能宣称恢复原 PTY 内存状态**；需要远程 tmux 等机制才可能恢复持久会话。

### 3.2 FileProvider 接口

应区分本地路径与远程 POSIX 路径，避免 Windows `PathBuf` 规则污染 Linux SFTP 路径。

```rust
struct HostId(String);

enum Endpoint {
    Local { path: PathBuf },
    Remote { host: HostId, path: RemotePath },
}

struct TransferSpec {
    source: Endpoint,
    destination: Endpoint,
    conflict: ConflictPolicy,
}
```

文件接口至少需要 `read_dir`、`metadata`、`create_dir`、`rename`、`remove`、`open_read`、`open_write`、`set_metadata`。流式 API 应表达 Seek/offset 能力、写入完成、取消及服务端能力差异，而不是只提供单个“复制文件”函数。

同一 Host 上复制可优先尝试服务端 copy 能力；缺失时使用安全的流式回退，不默认使用任意 shell 命令。

### 3.3 TransferScheduler

任务生命周期：Queued → Preparing → Transferring → Verifying → Completed；异常分为 Cancelled、Interrupted 与 Failed，必要时 Retry。

- 任务队列只保存不可变的 TransferSpec 与任务配置；
- 传输实体独立拥有文件 Reader/Writer，事件通过 Tokio Channel 推送到 UI；
- 传输使用有界缓冲（候选 64–256 KiB/流，最终以压测为准）、背压和受限并发；
- 默认远程 A → 本机**内存中继** → 远程 B，不要求 A 与 B 可直连；
- 取消只能使任务停止后进入 Cancelled；若已写入部分目标，需按临时文件策略清理或显式保留；
- 文件与目录处理分离：先规划目录结构，再调度各文件；符号链接策略明确；
- 成功状态在完成 Writer finalization、必要校验与提交后才发出。

**重要语义**：MVP 的“后台”指 **TUI 仍运行时文件任务与终端并行执行**。若 Relay 进程退出，任务停止并标记为可识别中断；程序退出后继续传输需要另设常驻进程，不纳入 MVP。

### 3.4 文件提交与验证

默认优先向目标同目录的唯一临时文件写入，最终验证后再执行 rename/commit；rename 语义由服务端和文件系统决定，不能笼统承诺跨所有服务器原子提交。

完整性验证分级：
- Basic：确认写入字节数、服务端响应及目标大小（仍不是密码学完整性保证）；
- Enhanced（Later）：双方可靠读取或计算并比较散列；
- Metadata：权限/修改时间尽可能保留，但失败需按策略报告。

覆盖已存在目标文件时，必须预先确认，且不能先将正式目标文件截断。

## 4. SSH 与 OpenSSH 配置

优先从用户 SSH Config 生成连接配置：`Host` / `HostName` / `User` / `Port` / `IdentityFile` 等。`Include`、`Match`、`ProxyJump`、`ProxyCommand`、硬件密钥、证书、代理与特殊 SSH 扩展需要逐项验证。

架构采用 **russh 优先、系统 OpenSSH 后备**。OpenSSH 后备并非简单替换终端进程就让 SFTP 自动兼容：对应的文件浏览/上传下载路径也必须单独接入、测试和校验身份。不得隐式降级主机验证或跳过受保护的复杂配置。

所有 SSH Host Key 都采用可信存储与明确验证策略；绝不使用无条件接受主机密钥的处理器。

## 5. Tailscale 接入

**Tailscale 是网络可达性层，不是新的文件传输协议。**

MVP 允许通过：
- `100.x.y.z` 等 Tailnet IP；
- 用户环境可解析的 MagicDNS 域名；
- 已经配置好的 Subnet Router 允许访问的内部目标。

前提是本机的 Tailscale 客户端和操作系统路由、ACL/grants/SSH 服务端配置已正确设置。Relay 不负责启动 VPN、修改 Tailnet 策略或绕过授权。

**必须区分两种场景**：
- Ordinary SSH over Tailscale：普通 SSH 密钥/Agent/密码认证 + Tailnet 网络。
- Tailscale SSH：Tailscale 管理 SSH 身份与授权，认证行为另有差异；Later 进行单独兼容性验证。

后期可选读取本机 `tailscale status --json` 作为设备发现辅助，但“节点在线”不代表目标端口可连接，也不意味着授权成功。

Windows/WSL2：Windows 路径、WSL 路由和 DNS 需要分别验证；目标是连接兼容，而不是 Relay 自动修复宿主机 VPN。

## 6. TUI 与 PTY 事件通道

```text
keyboard ──> focus router ──> PTY input (Terminal Input)
                      └───> UI command (Relay Control)
SSH channel output ──> VT parser ──> terminal screen state
transfer events ─────> app state ──> UI re-render
window resize ───────> SSH PTY resize + UI layout
```

远端 PTY 字节流可包含控制序列；渲染与剪贴板/系统控制要有防护。每个 TerminalTab 独立维护解析器、滚动区域、尺寸以及会话 ID。外层终端仍可能拦截功能键或鼠标事件，因此需要真实环境验证。

## 7. 设计取舍记录（ADR 摘要）

| 决策 | 当前结论 | 原因 |
| --- | --- | --- |
| ADR-001 | 不 Fork termscp | 它以文件传输 Activity 为中心，后台队列与 PTY 需要大重构 |
| ADR-002 | 只做 SSH / SFTP | 让 MVP 规模可控 |
| ADR-003 | 传输 Core 不依赖 TUI | 长任务和 CLI 可复用 |
| ADR-004 | 远程互传优先内存中继 | 不要求两台远端直接互通、不分发凭证 |
| ADR-005 | Tailscale 作为网络接入 | 无需自行实现 WireGuard 或额外服务 |
| ADR-006 | 主机密钥严格验证 | 避免中间人攻击及不可信连接 |
| ADR-007 | CLI 延至 0.2 | 先验证真实 PTY + 传输并行体验 |
| ADR-008 | F12 候选 PTY 逃逸键 | 避免与远端常用按键冲突，P0 需测试 |

## 8. 关键待决事项

- 何时使用独立连接而非同 SSH Channel 多路复用？
- russh + SFTP 的实际异步 API 与取消机制能否满足要求？
- 复杂 OpenSSH 配置的后备传输层如何封装？
- 不同操作系统的 known_hosts/SSH Agent 行为差异？
- 临时文件命名、异常清理与目标冲突恢复规则？
- 是否需要在 0.2 采用 SQLite 持久化任务元数据？

关联：[产品](PRODUCT.md) · [安全](SECURITY.md) · [测试](QUALITY.md)
