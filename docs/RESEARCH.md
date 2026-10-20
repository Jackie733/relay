# Relay 竞品与源码调研记录

> 版本：0.1-draft · 2026-10-20 · 基于项目公开资料与 2026-10 对 termscp main 分支的源码阅读。此文用于解释产品决策，不是完整功能排行榜。

## 1. 核心结论

市场已有 **Rust + TUI + SSH/SFTP** 工具；实现双栏文件管理、跨主机传输或交互式 PTY 本身都不是独有卖点。

Relay 选择的组合重点是：**对等多主机工作空间 + 真正的 SSH PTY + 独立后台传输队列 + 严格文件写入与身份安全**。首个版本必须优先验证 PTY 与传输能够并行。

## 2. termscp

仓库：https://github.com/veeso/termscp

在本次阅读时，`Cargo.toml` 标记版本为 1.2.1，2026-10-19 发布。

### 已验证的源码路径

| 模块 | 源码 | 发现 |
| --- | --- | --- |
| FS 抽象 | [host/bridge.rs](https://github.com/veeso/termscp/blob/main/src/host/bridge.rs) | `HostBridge` 封装本地和远端文件操作 |
| 双栏模型 | [filetransfer/lib/browser.rs](https://github.com/veeso/termscp/blob/main/src/ui/activities/filetransfer/lib/browser.rs) | `Browser` 包含 `local: Pane` 与 `remote: Pane` |
| 远程桥接 | [host/remote_bridged.rs](https://github.com/veeso/termscp/blob/main/src/host/remote_bridged.rs) | 依据 STREAM_READ/WRITE 能力选择直接流或本地临时文件回退 |
| 传输循环 | [session/transfer.rs](https://github.com/veeso/termscp/blob/main/src/ui/activities/filetransfer/session/transfer.rs) | 约 64 KiB 缓冲区（65535 字节）的同步读写与进度更新 |
| 命令终端 | [actions/exec.rs](https://github.com/veeso/termscp/blob/main/src/ui/activities/filetransfer/actions/exec.rs) | 调用当前文件系统 `exec()` 并打印命令输出，不等于长期交互式 SSH PTY |
| VT 渲染 | [terminal/component.rs](https://github.com/veeso/termscp/blob/main/src/ui/activities/filetransfer/components/terminal/component.rs) | 使用 tui-term/VT 解析器呈现终端字符 |
| SSH 客户端 | [remotefs_builder.rs](https://github.com/veeso/termscp/blob/main/src/filetransfer/remotefs_builder.rs) | RusshSftpFs/RusshScpFs 使用 `NoCheckServerKey`，此路径不校验实际主机身份 |
| 新版变更 | [CHANGELOG.md](https://github.com/veeso/termscp/blob/main/CHANGELOG.md) | 1.2.1 修复多项目标路径与覆盖检查问题 |

### 最值得学习的部分

- 按文件系统接口抽象远端实现，不让 UI 感知协议差异；
- 远程 A / 远程 B 都可通过 HostBridge 被纳入复制流程；
- 文件传输尽可能采用流式 IO，并保留能力不足时的回退路径；
- 文件队列、目录预扫描和进度可测试的工作列表。

### 不能直接继承的限制

- 当前传输代码与 FileTransferActivity、状态更新和 UI 重绘相互耦合；
- 嵌入式“终端”使用 `exec()` 而不是持续保持的 SSH PTY 会话；
- 上述 SSH Builder 明确使用 `NoCheckServerKey`，Relay 必须采用不同的安全默认值；
- [Background Queue Issue #326](https://github.com/veeso/termscp/issues/326) 中作者表示架构阻碍后台模式，重构成本较高；
- [UX Issue #452](https://github.com/veeso/termscp/issues/452) 说明有用户希望键位可发现性、焦点与视觉层级更清晰。

**决策**：不 Fork termscp；学习设计与测试边界，在独立的 SSH Session Manager 与 Transfer Engine 上实现产品。

## 3. sshm-rs

仓库：https://github.com/bit5hift/sshm-rs

公开 README 描述其具备：SSH 主机管理、交互式 PTY、SFTP 文件浏览、上传下载、OS Keyring、多主机命令，以及终端文件工作目录跟随。这意味着 Relay 不能只靠“SSH + SFTP 结合”作为差异化。

**研究重点**：终端与侧边 SFTP 面板的焦点切换、远程目录跟随、连接导入、命令与传输的使用成本。

**尚未在本次调查中验证的内容**：其各条功能在当前实际二进制上的完整质量表现、不同服务器兼容性和任何未在文档中明确的跨主机后台队列能力。

## 4. 其他参考

| 项目 | 链接 | 参考价值 |
| --- | --- | --- |
| Midnight Commander | https://midnight-commander.org/ | 双栏文件管理与键盘操作 |
| PuTTY | https://www.putty.org/ | 传统 SSH 终端需求 |
| WinSCP | https://winscp.net/ | 文件传输与冲突处理 |
| trzsz | https://github.com/trzsz/trzsz | 终端传文件、拖入文件等跨终端工作流 |
| Tailscale | https://tailscale.com/kb | Tailnet 地址访问与 Tailscale SSH 的区别 |
| Ratatui | https://ratatui.rs/ | Rust TUI 渲染框架 |
| russh | https://github.com/Eugeny/russh | 原生 Rust SSH 客户端实现 |

## 5. 对 Relay 的明确指导

1. P0 必须验证真实 SSH PTY 而不是复用执行一次命令的终端组件。
2. 第一天就将传输执行移出 UI，避免重蹈后台队列难以解耦的架构困境。
3. 左右文件面板绑定任意 Host，而非写死 Local/Remote。
4. 首次连接和主机密钥变化要由安全策略处理，绝不能用无校验 Handler 换取“连得上”。
5. 先精通 SSH/SFTP，不因 termscp 已支持 FTP/S3/SMB 就盲目复制广度。
6. Tailscale 是现有 SSH 的网络可达性路径，先复用系统已建立的 Tailnet 连接。

### 研究局限

这是源码与文档层面的静态研究，**没有**在本仓库运行上述第三方应用、完成同环境性能比较或做审计级安全评估。依赖 API 与具体兼容性均应由 [P0 技术验证](ROADMAP.md) 再确认。

相关：[产品](PRODUCT.md) · [架构](ARCHITECTURE.md) · [安全](SECURITY.md)
