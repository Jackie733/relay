# SSH integration fixtures

三个独立 Docker bridge 网络分别放置 OpenSSH A/B/C，端口只绑定本机
`127.0.0.1:2221/2222/2223`。A/B 不共享容器网络；Relay 从本机分别访问。
此处提供 P0-01 至 P0-04 的 SSH、交互式 PTY、SFTP 及跨主机传输验证环境（含 `vim`、`htop`、`tmux`、`less` 与 `C.UTF-8` locale）。

要求 Docker Compose v2（支持 `up --wait`）、本机 OpenSSH 客户端及 POSIX shell。

在仓库根目录运行：

```sh
sh tests/integration/prepare.sh
docker compose -f tests/integration/compose.yml up --build --wait
sh tests/integration/check.sh
cargo test --workspace --locked -- --ignored
docker compose -f tests/integration/compose.yml down --volumes
```

客户端测试密钥在本机生成于被忽略的 `.fixtures/`，私钥不挂载到容器、不写入镜像。
启动时将只读挂载的公钥复制到容器用户的 `~/.ssh/authorized_keys` 并设置所有权和权限，
避免宿主机 UID 与容器用户不同导致 OpenSSH StrictModes 拒绝认证。
每台服务器首次启动生成独立主机密钥并保存在命名卷；`down --volumes` 后重建会变更密钥，
可供后续 Host Key 变化测试使用。`check.sh` 只对这些临时测试主机扫描公钥，
写入临时信任文件后启用严格检查；该机制不能用作产品的主机信任实现。

当前检查验证三台服务器的密钥认证，以及所有六个方向的跨网络 DNS 与容器 IP 的 TCP 22 隔离。
TCP 探测先连接各容器自身的 SSH 监听端口，避免探测工具失效被误判为隔离成功。
Compose 为每个服务生成独立镜像标签，避免并行构建写入同一标签的冲突。
`crates/ssh/tests/openssh.rs` 另行验证 Relay 自己的主机信任、认证、口令与取消，
仅连接本机端口并使用临时信任库；常规 `cargo test` 默认跳过，CI 的 ssh-fixtures 任务显式执行。
`crates/ssh/tests/openssh.rs` 覆盖 PTY、SFTP 与 1 GiB 流式读写；`crates/tui/src/session.rs` 覆盖终端程序、128 MiB A → Relay → B 中继、同连接/独立连接的终端延迟、取消和传输连接断开。后者使用独立临时目录与两端 SHA-256 校验，性能复验命令及解释见开发指南 P0-04。不要在这里使用真实主机配置或生产密钥。
