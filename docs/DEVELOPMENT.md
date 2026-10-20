# Relay 开发指南

## 当前交付范围

这是 P0 之前的工程起点，不是 SSH/SFTP 原型已经通过验收。

- Rust 2024 Cargo workspace；开发工具链固定为 1.97.1，MSRV 为 1.89。
- `relay` 启动后默认进入 Hosts；`1/2/3/4` 切换四个工作空间。
- Files 两栏独立浏览本机目录，支持进入目录、返回上级、刷新、首尾跳转。
- 目录读取在独立服务任务中执行，通过有界消息队列更新界面。
- 读取失败保留原路径；旧请求的迟到结果不会覆盖新请求。
- 小于 100 列只显示聚焦栏；小于 60×16 显示扩窗提示。
- `?` 帮助不会遮挡模式状态栏；正常退出、返回错误及 panic 有终端恢复路径。
- 本地名称保留 `OsString`，远端路径保留原始字节；显示时转义控制字符。
- 已提供三台 OpenSSH 测试主机的 Docker 配置和临时测试密钥生成脚本。

尚未实现：SSH 配置读取/连接/认证/主机信任、PTY、SFTP、文件写入/复制/删除、
传输调度、日志持久化、用户配置及自定义快捷键。界面不会展示虚构的已连接主机或运行任务。
尚未引入 russh 或终端仿真依赖；加密后端与终端按键逃逸方案仍待对应 P0 实测。

初始 `FileSystem` 仅有只读 `read_dir`；它暂时一次返回最多 10,000 项，超过时明确报错。
目前串行处理目录请求，不支持读取中的取消。分批目录流、取消和写入能力随 P0-FS 引入，
不应把当前简化接口视为最终设计。符号链接只展示，不进入其指向的目录。

## 代码结构

```text
crates/
  common/    路径、端点、文件类型、只读 FileSystem 与 LocalFs
  ssh/       主机配置与连接状态类型，尚无网络实现
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

本次初始化在 macOS ARM64 本机验证，未在本机运行 Linux、WSL2 或原生 Windows。
已通过格式检查、Clippy、10 项单元测试（含 40 种界面尺寸/视图/帮助组合）、
发布构建、Rust 1.89 最低版本检查、cargo-deny，以及真实终端交互检查。
测试环境的 YAML 结构、shell 脚本语法和测试密钥重复准备也已检查。
本机没有 Docker，因此容器启动、SSH 认证与网络隔离检查留待具备 Docker 的环境或 CI 验证。
这不满足整个 P0 Gate，也不代表架构文档中的 SSH、传输及跨平台验收已通过。

仓库尚未选择代码许可证，所有 crate 禁止发布。
`cargo-deny` 只对这些私有 workspace crate 跳过许可证要求，第三方依赖仍进行许可证与漏洞检查。
