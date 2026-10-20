# SSH integration fixtures

三个独立 Docker bridge 网络分别放置 OpenSSH A/B/C，端口只绑定本机
`127.0.0.1:2221/2222/2223`。A/B 不共享容器网络；Relay 从本机分别访问。
此处仅为后续 SSH/SFTP 测试准备环境，不代表 Relay 已能连接 SSH。

要求 Docker Compose v2（支持 `up --wait`）、本机 OpenSSH 客户端及 POSIX shell。

在仓库根目录运行：

```sh
sh tests/integration/prepare.sh
docker compose -f tests/integration/compose.yml up --build --wait
sh tests/integration/check.sh
docker compose -f tests/integration/compose.yml down --volumes
```

客户端测试密钥在本机生成于被忽略的 `.fixtures/`，私钥不挂载到容器、不写入镜像。
每台服务器首次启动生成独立主机密钥并保存在命名卷；`down --volumes` 后重建会变更密钥，
可供后续 Host Key 变化测试使用。`check.sh` 只对这些临时测试主机扫描公钥，
写入临时信任文件后启用严格检查；该机制不能用作产品的主机信任实现。

当前检查验证三台服务器的密钥认证与跨网络 DNS 隔离；Relay 的主机验证、PTY、
SFTP 读写及传输故障测试随对应功能添加。不要在这里使用真实主机配置或生产密钥。
