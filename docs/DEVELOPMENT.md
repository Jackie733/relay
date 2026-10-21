# Relay 开发指南

## 当前交付范围

当前已完成工程基线及 P0-01 的 Linux SSH 连接原型，完整 P0 Gate 尚未通过。

- Rust 2024 Cargo workspace；开发工具链固定为 1.97.1，MSRV 为 1.89。
- `relay` 启动后默认进入 Hosts；`1/2/3/4` 切换四个工作空间。
- Files 两栏独立浏览本机目录，支持进入目录、返回上级、刷新、首尾跳转。
- 目录读取在独立服务任务中执行，通过有界消息队列更新界面。
- 读取失败保留原路径；旧请求的迟到结果不会覆盖新请求。
- 小于 100 列只显示聚焦栏；小于 60×16 显示扩窗提示。
- `?` 帮助不会遮挡模式状态栏；正常退出、返回错误及 panic 有终端恢复路径。
- 本地名称保留 `OsString`，远端路径保留原始字节；显示时转义控制字符。
- 已提供三台 OpenSSH 测试主机的 Docker 配置和临时测试密钥生成脚本。
- `ssh` crate 支持主机信任校验、显式首次确认、加密/未加密 OpenSSH 私钥认证；提供独立连接示例。
- `ssh -G` 与不执行命令的后备配置解析器已对照测试；连接能力尚未接入 Hosts UI。

尚未实现：TUI 的主机连接工作流、SSH Agent/密码/keyboard-interactive、PTY、SFTP、文件写入/复制/删除、
传输调度、日志持久化、用户配置及自定义快捷键。界面不会展示虚构的已连接主机或运行任务。
已锁定 russh 0.64.1 + ring 0.17.14，禁用 russh 默认特性与 RSA；终端仿真及按键逃逸方案待 P0-02 验证。

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

本次变更的 Windows/macOS/WSL2 和远端 CI 尚待实际执行。

仓库尚未选择代码许可证，所有 crate 禁止发布。
`cargo-deny` 只对这些私有 workspace crate 跳过许可证要求，第三方依赖仍进行许可证与漏洞检查。
