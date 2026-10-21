# Relay 开发指南

## 当前交付范围

当前已完成工程基线、P0-01 SSH 连接原型、P0-02 交互式 PTY 终端与 P0-03 SFTP 文件接口，完整 P0 Gate 尚未通过。

- Rust 2024 Cargo workspace；开发工具链固定为 1.97.1，MSRV 为 1.89。
- `relay` 启动后默认进入 Hosts；`1/2/3/4` 切换四个工作空间。
- Files 两栏可独立绑定本机（`LocalFs`）或远端 SFTP（`RemoteFs`）目录，支持进入目录、返回上级、刷新、首尾跳转（使用 `--ssh` 或 `--ssh-config` 启动时，左栏为 Local，右栏自动挂载远端 SFTP 主目录）。
- 目录读取在独立服务任务中通过 `FsResolver` 执行，经有界消息队列更新界面。
- 读取失败保留原路径；旧请求的迟到结果不会覆盖新请求。
- 小于 100 列只显示聚焦栏；小于 60×16 显示扩窗提示。
- `?` 帮助不会遮挡模式状态栏；正常退出、返回错误及 panic 有终端恢复路径。
- 本地名称保留 `OsString`，远端路径保留原始字节（`RemotePath` / `raw_name`）；显示时转义控制字符，含 `U+FFFD` 替换字符的名称及 Windows 非法名禁止修改操作。
- 已提供三台 OpenSSH 测试主机的 Docker 配置和临时测试密钥生成脚本。
- `ssh` crate 支持主机信任校验、显式首次确认、加密/未加密 OpenSSH 私钥认证、长期交互式 PTY Channel（`xterm-256color`、窗口大小同步、退出状态回报），以及自研精简 SFTP v3 客户端（`SftpClient` / `RemoteFs`，支持列目录、`stat`/`lstat`、`canonicalize`、创建/删除目录与文件、有界流水线流式读写、`NoReplace` 与 `posix-rename@openssh.com` `AtomicReplace` 重命名、`fsync@openssh.com`、`statvfs@openssh.com`、`limits@openssh.com` 能力探测）。
- `ssh -G` 与不执行命令的后备配置解析器已对照测试；可通过 `relay --ssh ...` 或 `relay --ssh-config ...` 打开交互式 SSH 终端与远端 SFTP 浏览（连接前在控制终端完成主机密钥或私钥口令确认）。
- `tui` 集成 `vt100 0.18.1`、设备查询应答器（DA1/DA2/DSR/CPR/XTWINOPS 18）、安全策略（OSC 52 拦截、OSC 0/2 标题清洗）、`Input Encoder`（DECCKM、xterm 修饰键、Alt 前缀、防注入括号粘贴）与 `Terminal Input` / `Control` 模式切换（默认同时启用 `F12` 与 `Ctrl+]`，连按两次透传，支持 `--escape-key`）。

尚未实现：Hosts UI 的主机列表与模态框工作流、SSH Agent/密码/keyboard-interactive、跨主机传输调度（P0-04）、
日志持久化、用户配置文件落盘。界面不会展示虚构的已连接主机或运行任务。
已锁定 russh 0.64.1 + ring 0.17.14，禁用 russh 默认特性与 RSA；终端仿真已定案使用 `vt100 0.18.1`（ADR-018）；SFTP 客户端已定案采用自研精简 SFTP v3（ADR-010）。

初始 `FileSystem` 仅有只读 `read_dir`；它暂时一次返回最多 10,000 项，超过时明确报错。
目前串行处理目录请求，不支持读取中的取消。分批目录流、取消和写入能力随 P0-FS 引入，
不应把当前简化接口视为最终设计。符号链接只展示，不进入其指向的目录。

## 代码结构

```text
crates/
  common/    路径、端点、文件类型、只读 FileSystem 与 LocalFs
  ssh/       配置解析、主机信任、Prompt Broker、连接与私钥认证
  transfer/  传输请求与任务状态类型，尚无调度实现
  tui/       relay 程序、输入、状态、绘制及 services 编排
scripts/          真实终端交互检查
tests/integration/  可控 OpenSSH A/B/C 测试环境
.github/workflows/  跨平台构建、质量检查、依赖检查、测试环境验证
```

依赖方向遵循架构文档：TUI → SSH / Transfer → Common；Transfer 不依赖 SSH；
Common 不依赖 UI。`tui::services` 不引用 Ratatui。
其他设计文档仍描述目标架构；本节与代码描述实际支持范围。

## 运行和检查

```sh
cargo run --locked
cargo run --locked -- /path/to/directory
cargo run --locked -- --help

cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo build --release --locked

rustup toolchain install 1.89.0 --profile minimal
cargo +1.89.0 check --workspace --all-targets --locked

cargo install cargo-deny --version 0.20.2 --locked
cargo deny check
```

二进制位于 `target/debug/relay` 或 `target/release/relay`（Windows 为 `.exe`）。
交互模式需要真实终端；重定向输入/输出会明确报错，帮助与版本信息不受此限制。
`--` 后的参数按路径处理，可用于以 `-` 开头的目录名。

Unix 上还可以用虚拟终端驱动真实程序，并通过终端模拟器检查最终可见内容：

```sh
python3 -m venv /tmp/relay-ui-tests
/tmp/relay-ui-tests/bin/pip install -r scripts/requirements-test.txt
/tmp/relay-ui-tests/bin/python scripts/smoke_tui.py target/release/relay
```

检查覆盖帮助与错误输出、四个视图、左右栏导航、进入/返回目录、帮助窗口、
窗口缩放、退出码和退出后的终端设置恢复。Python 及 pyte 仅用于检查，不是运行 Relay 的依赖。
Docker 环境的命令见 [集成测试说明](../tests/integration/README.md)。

## 自动检查与验证边界

CI 已配置 Linux、macOS、原生 Windows 的格式、Clippy、测试及发布构建；
Linux/macOS 额外运行真实终端交互检查；Linux 检查 MSRV、cargo-deny 和 Docker 测试环境。
配置已落地，远端 CI 的运行结果以 GitHub 实际执行为准。

初始化时在 macOS ARM64 本机验证，未在该次验证中运行 Linux、WSL2 或原生 Windows。
已通过格式检查、Clippy、10 项单元测试（含 40 种界面尺寸/视图/帮助组合）、
发布构建、Rust 1.89 最低版本检查、cargo-deny，以及真实终端交互检查。
测试环境的 YAML 结构、shell 脚本语法和测试密钥重复准备也已检查。
该环境没有 Docker，容器检查的后续结果见下方记录。

### 工程基线复验（2026-10-21）

Linux x86_64，Rust 1.97.1，Docker Engine 29.2.1，Compose 5.0.2：

- `cargo fmt --all -- --check`、Clippy（warnings 视为错误）、11 项单元测试及发布构建通过。
- `cargo +1.89.0 check --workspace --all-targets --locked` 通过。
- cargo-deny 0.20.2 的漏洞、许可证、来源与依赖规则检查通过；有 `syn` 2/3 重复版本警告。
- 原有真实终端交互脚本通过，含 resize、退出码及终端状态恢复。受限执行沙箱内的
  resize 信号测试失败，沙箱外对照运行通过；此项应在允许终端信号的环境运行。
- 三台 OpenSSH 容器构建、启动及健康检查通过；密钥认证与六个方向的 DNS / TCP 22 隔离通过。
- 额外模拟挂载公钥 UID 为 12345、容器用户 UID 为 1000，确认启动时复制并修正所有权后
  密钥登录成功，OpenSSH StrictModes 保持启用。

已核对初始化提交 `32d622e` 的 [GitHub CI 记录](https://github.com/Jackie733/relay/actions/runs/39514401734)：
Linux/macOS/Windows Rust 检查、MSRV 和依赖检查通过；SSH 环境检查因公钥认证失败。
本次修复了挂载公钥所有权不匹配及并行构建共用镜像标签的问题，并在本机完成复验。
本次变更的远端 CI 尚待提交后执行；WSL2 尚未实测。

基线复验时尚未引入 SSH 依赖；P0-01 的后续结果见下方，后端跨平台验证仍未完成。
以上结果不满足整个 P0 Gate，也不代表 Relay 的 SSH、传输及完整跨平台验收已通过。

### P0-01 SSH 原型（2026-10-21）

在上述 Linux 环境通过 24 项常规测试及真实 OpenSSH 集成测试，另通过格式检查、Clippy、
发布构建、Rust 1.89 检查与 cargo-deny。依赖扫描保留重复版本警告，未忽略安全公告。

集成测试验证：首次未知密钥的拒绝/接受、严格模式拒绝未知主机、保存后重连、远端命令回显、
主机密钥变化、全局撤销、错误用户、OpenSSH 生成的哈希 known_hosts、通配符、不同算法警告、
加密私钥的正确/错误口令、确认超时、连接取消及网络失败。所有写入发生在临时信任库中。
真实终端中独立核对了容器主机指纹、输入 `yes` 完成首次确认，并确认认证成功及正常退出。
`ssh -G` 和后备模式均通过同一测试配置完成真实登录；原有 TUI 真实终端回归亦通过。

启动测试容器后，可运行：

```sh
cargo test -p ssh --locked --test openssh -- --ignored
cargo run --locked -p ssh --example connect -- \
  127.0.0.1 2221 relay tests/integration/.fixtures/id_ed25519 /tmp/relay-p0-known-hosts
# 使用显式配置文件；仅在主动连接时调用 ssh -G，可能执行配置中的 Match exec
cargo run --locked -p ssh --example connect -- --config ALIAS /path/to/ssh_config
# 不执行配置中的命令，复杂配置会明确拒绝
cargo run --locked -p ssh --example connect -- --config ALIAS /path/to/ssh_config --fallback
```

原型边界：

- 支持 Ed25519/ECDSA 的 OpenSSH 私钥；真实认证测试使用 Ed25519。RSA 暂禁用（ADR-017）。
- Prompt 由有界队列与独立回复通道关联主机；未回复、超时和取消均不视为同意。
  口令及私钥读取缓冲使用 secrecy；解密在阻塞工作线程执行。
- 信任读取合并用户/全局文件与 Relay 文件；冲突及撤销优先拒绝。写入前使用文件锁重新检查，
  只追加，不修改已有条目；Unix 新文件为 0600，拒绝写入符号链接或权限过宽的 Relay 文件。
  Windows ACL 尚未实测。信任文件上限 16 MiB，单个私钥文件上限 64 KiB；畸形来源会阻止连接。
- 支持端点、IdentityFile、HostKeyAlias、信任来源、StrictHostKeyChecking、ConnectTimeout、
  HostKeyAlgorithms 与 keepalive。未知主机确认独立限时，握手总预算为网络超时加确认超时。
- 解析器接收显式配置文件，等同 `ssh -F FILE`；`--fallback` 需明确选择，支持简单 Host 块，
  拒绝 Include / Match、重复信任指令、未解析的路径 token，以及代理/证书等不支持功能。
  `ssh -G` 的空格路径输出有歧义时拒绝读取该信任来源。后备模式不是完整 OpenSSH 兼容层。
- 目前没有完整的会话管理器、TUI 模态框和终端屏幕；下一步为 P0-02 PTY。

### P0-02 交互式 PTY 终端（2026-10-21）

在上述 Linux 环境通过 36 项常规单元测试、3 项真实 OpenSSH 集成测试（`ssh` + `tui`），以及真实 Unix PTY 端到端交互冒烟检查，另通过格式检查、Clippy、发布构建、Rust 1.89 MSRV 检查与 cargo-deny。

实测验证：

- `ssh::Connection::open_pty` 请求 `xterm-256color`（设置 `VERASE=0x7f` 与 `IUTF8=1`）并拆分并发读写半部；支持持续收发、50 ms 防抖 `window-change` 及远端退出状态回报。
- `tui::terminal` 实现 `Input Encoder`（DECCKM 应用光标、xterm 修饰参数、Alt 前缀、Ctrl 控制字符、剔除嵌入 `ESC[201~`/`ESC[200~` 的括号粘贴）与 `Emulator`（`vt100 0.18.1` + 10,000 行回滚缓冲 + 仅由本地状态生成的 DA1/DA2/DSR/CPR/XTWINOPS 18 应答，单批应答限长 4 KiB，拒绝 OSC 52 剪贴板读写并清洗 OSC 0/2 标题）。
- 针对真实 OpenSSH 容器端到端运行并渲染 `vim`（含备用屏幕、DA/CPR 应答、DECCKM 方向键、`Esc`、CJK 编辑保存）、`htop`（彩色进程界面与退出）、`less`（DECCKM 翻页与退出）、`tmux`（状态栏、24-bit 真彩色与 CJK 宽字符）、readline 括号粘贴防提前执行注入，以及窗口 resize（`stty size` 校验）。
- 验证 `F12` 与后备键 `Ctrl+]`（含传统 `0x1d`/`Ctrl+5` 解码）从 `Terminal Input` 安全返回 `Relay Control`、连按两次向远端透传，以及会话退出自动回到 `Control`。同时修复了 `scripts/smoke_tui.py` 未开启独立会话（`start_new_session=True`）导致子进程继承父 shell `/dev/tty` 窗口大小的问题。

启动测试容器后，可运行：

```sh
cargo test --workspace --locked -- --ignored
/tmp/relay-ui-tests/bin/python scripts/smoke_tui.py target/release/relay --ssh-fixtures
cargo run --locked -- --ssh \
  127.0.0.1 2221 relay tests/integration/.fixtures/id_ed25519 /tmp/relay-p0-known-hosts
cargo run --locked -- --ssh-config ALIAS /path/to/ssh_config [--fallback] [--escape-key ctrl+g]
```

原型边界：目前通过命令行参数在启动 TUI 前建立单条 P0 SSH PTY 会话，尚未接入 Hosts 工作空间与 TUI 模态框；下一步为 P0-03 SFTP 文件接口。

### P0-03 SFTP 文件接口（2026-10-21）

在上述 Linux 环境通过 51 项常规测试、4 项真实 OpenSSH 集成测试（`ssh` + `tui`）以及 Release 程序的真实 Unix PTY 端到端交互冒烟检查（含 SFTP 远端面板目录浏览），另通过格式检查、Clippy、发布构建、Rust 1.89 MSRV 检查与 cargo-deny 离线检查。

实测验证：

- `common::fs` 与 `common::path`：扩展完整 `FileSystem` trait（`read_dir`、`lstat`、`stat`、`canonicalize`、`create_dir`、`remove_file`、`remove_dir`、`rename`、`open_read`、`open_write`、`set_metadata`、`free_space`）、`Capabilities`、`Metadata`（自动剥离 setuid/setgid/sticky 位）、`RenameMode::{NoReplace, AtomicReplace}`、`FsResolver`，以及 Windows 保留名/非法字符/大小写折叠冲突校验与 `U+FFFD` 有损名称修改拦截。
- `ssh::sftp`：在 `russh` 会话通道上实现自研精简 SFTP v3 客户端（`SftpClient` / `RemoteFs`），单帧上限 1 MiB，读写流水线各最多 8 个未完成请求，块大小按服务端 `limits@openssh.com` 与 256 KiB 上限协商；读取器/写入器 `Drop` 及目录读取取消时自动异步发送 `SSH_FXP_CLOSE`。
- 审核修复新增 10 项回归测试：读取取消后重试保持完整字节序列，动态缩小读取上限及零长度读取不丢失预读数据，短读无重复/遗漏；目录遍历取消及 `OPENDIR` 句柄晚到均关闭句柄；每客户端最多 64 个排队/待回复请求，超时（含外部取消后的请求）终止 SFTP 通道并拒绝后续请求；小于 4 KiB 的服务端读写限制与完整包长上限均被遵守，不可用限制被拒绝；本地文件/目录时间戳正确修改并保留未指定字段。取消、超时和资源上限的约定见架构 §6.3。
- 可编程 `Quirky-SFTP` 异常服务端测试：覆盖不广播扩展时 `AtomicReplace` 返回 `FsError::Unsupported`、`fsync` 返回 `SyncOutcome::Unsupported`、`statvfs` 返回 `None`；过滤 `.`、`..`、空名、含 `/` 或 NUL 的恶意目录项；超 10,000 项大目录报错并关闭句柄；超长帧头、未知 `request_id`、中途断线与慢响应取消。
- 真实 OpenSSH 容器集成测试（`sftp_filesystem_and_1gib_streaming_against_real_openssh`）：验证能力探测、目录与文件增删改查、权限拒绝（`/root` 及 `0500` 目录）与路径不存在、非 UTF-8 文件名（`b"non-utf8-\xff-\xfe-\x80.bin"`）原样字节往返、`NoReplace` 与 `posix-rename` `AtomicReplace` 覆盖，以及 1 GiB 流式读写（Release 模式下写入耗时 4.39 s / 233.2 MiB/s，读取耗时 2.98 s / 343.6 MiB/s，峰值 RSS 增量 804 KiB；Debug 模式下峰值 RSS 增量 1292 KiB）。
- 下一步为 P0-04 跨主机文件传输（Remote A → Relay 有界缓冲 → Remote B 及同连接/独立连接终端延迟测量）。

本次变更的 Windows/macOS/WSL2 和远端 CI 尚待实际执行。

仓库尚未选择代码许可证，所有 crate 禁止发布。
`cargo-deny` 只对这些私有 workspace crate 跳过许可证要求，第三方依赖仍进行许可证与漏洞检查。
