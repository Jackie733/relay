# Relay

> A multi-host SSH workspace for your terminal.

Relay（暂定产品名）是一款计划使用 Rust 构建的、以键盘操作为核心的 SSH TUI 工作台。目标是在一个应用中管理 SSH 主机、运行真实的交互式终端、浏览本地与远程文件，并在任意两台可连接的主机之间安全地传输文件。

**当前状态：工程脚手架 + P0-01 SSH 连接原型 + P0-02 交互式 PTY 终端 + P0-03 SFTP 文件接口。** 已有可运行的终端界面、四个工作空间切换和本地/远程双栏目录浏览。`ssh`、`common` 与 `tui` 已实现主机信任、密钥认证、交互式 SSH PTY、VT 终端仿真（`vt100` + 设备查询应答）、按键编码、`Terminal Input` / `Control` 焦点切换，以及自研精简 SFTP v3 客户端与统一 `FileSystem` 接口（可通过 `relay --ssh ...` 启动），尚未接入 Hosts UI 与跨主机传输队列（P0-04）。下文产品目标及设计文档不代表全部已交付功能。

## 开始开发

安装 [Rustup](https://rustup.rs/) 后，在仓库根目录运行：

```sh
cargo run --locked
# 或指定初始目录
cargo run --locked -- /path/to/directory
```

启动后按 `2` 浏览本地文件，`Tab` 切换左右栏，`j/k` 或方向键移动，`Enter` 进入目录，
`Backspace` 返回上级，`?` 查看帮助，`q` 退出。小于 100 列时显示当前聚焦的一栏。
界面提示暂用英文。当前仅浏览目录，不跟随符号链接；单目录最多加载 10,000 项。

项目固定开发工具链为 Rust 1.97.1，使用 Rust 2024，最低支持 Rust 1.89。
依赖版本由 `Cargo.lock` 锁定。开发检查、模块边界和当前实现范围见
[开发指南](docs/DEVELOPMENT.md)，可控 SSH 环境见 [集成测试说明](tests/integration/README.md)。

## 为什么开发 Relay？

常见工作流需要同时使用 SSH 客户端、SFTP 文件管理器和复制命令。Relay 希望统一这些操作：

- **终端**：多主机、真正的 SSH PTY，会话及焦点管理。
- **文件**：双栏浏览器；任一侧可以是本机或已连接的远程主机。
- **传输**：本地 ↔ 远程、远程 A ↔ 远程 B，后台任务、进度和失败反馈。
- **连接**：兼容常见 OpenSSH 配置；支持通过 Tailscale IP / MagicDNS 到达的 SSH 主机。
- **脚本**：逐步提供与 TUI 共用传输核心的 CLI。

### 产品原则

1. **SSH-first**：首先做可靠的 SSH / SFTP，不追求一开始支持所有存储协议。
2. **TUI-first**：完整键盘工作流；拖拽上传只是终端支持时的可选增强。
3. **Safe by default**：校验服务器主机密钥；破坏性文件操作需明确确认；失败不得静默损坏已有文件。
4. **Task-first**：传输引擎独立于 UI，长时间传输时仍能使用终端和文件管理器。
5. **Local-first / Agentless**：不要求账号、Relay 云服务或服务器端专用守护进程。

## 预期体验

```text
┌─ Relay ───────────────────────────────────────────────────────┐
│ Hosts  [Files]  Terminals   Transfers                        │
├─────────────────────────────┬────────────────────────────────┤
│ prod /var/www               │ backup /srv/backup             │
│  ▸ dist/                    │    daily/                      │
│    logs/                    │    archive/                    │
│    config.json              │    manifest.json              │
├─────────────────────────────┴────────────────────────────────┤
│ prod → backup  data.tar.zst   [########------] 58%           │
└──────────────────────────────────────────────────────────────┘
```

以上为概念示意图，不是当前可运行界面的截图。

## 文档导航

| 文档 | 内容 |
| --- | --- |
| [开发指南](docs/DEVELOPMENT.md) | 启动、检查命令、当前代码结构与验证范围 |
| [产品需求与边界](docs/PRODUCT.md) | 用户、场景、功能优先级、范围及验收 |
| [交互设计](docs/UX.md) | 布局、焦点、键盘、终端模式、拖入文件 |
| [技术架构](docs/ARCHITECTURE.md) | 术语、模块与运行时、SSH/主机信任、SFTP/文件系统、传输与提交协议、终端仿真、ADR |
| [安全设计](docs/SECURITY.md) | 主机校验、身份凭据、路径及覆盖保护 |
| [开发路线](docs/ROADMAP.md) | P0 原型、MVP、后续迭代及完成定义 |
| [测试与验收](docs/QUALITY.md) | 集成环境、功能矩阵、故障与性能要求 |
| [竞品与技术调研](docs/RESEARCH.md) | termscp、sshm-rs 及差异化依据 |

## 技术方向（SSH / SFTP 等核心链路待 P0 验证）

已使用 Rust 2024、Ratatui、Crossterm 和 Tokio；P0-01 已引入锁定的 russh 0.64.1 与 ring 后端，支持 Ed25519/ECDSA 密钥，RSA 暂禁用。后续接入 SFTP 客户端（优先自研精简 SFTP v3，P0 验证后定案，改造 russh-sftp 为备选）及 VT 终端仿真组件。设计采用可共享的应用核心、独立 SSH 会话管理器、自研主机信任模块、统一文件系统接口和后台任务引擎。加密后端的 Windows/WSL2 验证仍待完成。

## 目标平台

优先验证 Linux、Windows + WSL2；随后验证原生 Windows 与 macOS。Tailscale 作为网络接入方式，不要求在 Relay 内部实现 VPN。

## 贡献与许可

尚未选择项目代码的开源许可证；在许可证正式加入仓库前，不应假设项目代码已经对外授权。欢迎通过 Issues 交流产品需求和技术实现问题。

---

文档版本：**0.2-draft** · 更新：**2026-10-21**。重要技术取舍以代码原型和测试验证为准。
