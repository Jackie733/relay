# Relay 开发路线与实施计划

> 版本：0.2-draft · 2026-10-20。开发阶段与工程量是规划假设，不构成发布承诺。开发优先级以风险和验收结果调整。

## 1. 里程碑总览

| 阶段 | 目标 | 核心交付 | 退出条件 |
| --- | --- | --- | --- |
| P0 / Spike | 证明核心技术链路 | SSH PTY、SFTP、跨主机流式复制、Host Key | 4 项技术门槛全部通过 |
| 0.1 / MVP | 首个自用完整工作流 | 主机管理、文件双栏、终端 Tab、后台传输、Tailnet 地址访问 | 一条完整工作流和故障测试通过 |
| 0.2 / Reliability | 适合持续日常使用 | CLI、文件校验、断点续传、任务恢复及 SSH 兼容扩展 | 数据可靠性/兼容回归通过 |
| 1.0 / Productization | 稳定发布 | 兼容性、文档、分发、扩展功能 | 完整跨平台矩阵与发布检查通过 |

## 2. P0：技术可行性验证（最高优先级）

建议将 P0 控制为一个单独最小原型，首先解决“做得成、做得安全”而不是 UI 风格。P0 代码直接按 [架构 §3](ARCHITECTURE.md#3-模块结构) 的 crate 边界组织（不含 relay-cli），以免原型代码无法演进。

### P0-00 工程骨架

- [ ] Cargo workspace、Rust 2024、MSRV 与 `rust-toolchain.toml`
- [ ] CI：fmt、clippy、test、cargo-deny
- [ ] Docker Compose 测试拓扑（OpenSSH A / B / C，A 与 B 网络隔离）
- [ ] 加密后端（aws-lc-rs / ring）在 Linux、WSL2、Windows 构建验证并锁定

### P0-01 SSH 连接和 Host Key

- [ ] 通过 russh 连接可控 OpenSSH 容器
- [ ] SSH 密钥、Agent 至少一种认证方式通过端到端测试；Prompt Broker 原型（主机密钥确认、口令）
- [ ] 自研 known_hosts：哈希、通配符、`[host]:port`、`@revoked`、全局文件、算法变化
- [ ] 首次 Host Key 必须显式确认；已知密钥变化必须拒绝
- [ ] 失败可区分网络、认证、主机验证
- [ ] `ssh -G` 解析与后备解析器的结果对比

### P0-02 交互式 PTY

- [ ] 打开 PTY 并持续收发数据
- [ ] 在 Ratatui 区域渲染远端 `vim` / `htop`，支持颜色和光标
- [ ] 窗口 resize 正确传给远端 PTY
- [ ] Input Encoder：DECCKM、修饰键、Alt 前缀、括号粘贴（含注入剔除）
- [ ] DA / DSR 查询应答；vim、htop、tmux、less 不因缺少应答而卡顿
- [ ] 终端内 `Esc`、`Ctrl+C`、方向键和粘贴工作；CJK 宽字符与真彩色渲染正确
- [ ] 证明可以安全地从 Terminal Input 返回 Relay Control；记录逃逸键在各终端模拟器中的兼容矩阵，确定默认键与后备键
- [ ] 结论：vt100 保真度是否足够（否则切换 alacritty_terminal）

### P0-03 SFTP 文件接口

- [ ] 列目录、stat、创建/读取/写入文件
- [ ] 区分 Windows 本地路径和远程 POSIX 路径；远程路径以字节保存
- [ ] 使用可控测试主机验证权限错误和目录不存在
- [ ] **SFTP 客户端验证**：优先自研精简 SFTP v3（仅 MVP 必需操作及扩展，复用 russh SSH 层）；验证非 UTF-8 文件名往返、1 GiB 吞吐/峰值内存、posix-rename 覆盖，以及取消、断线、异常响应、原子覆盖失败；P0 Gate 后定案并更新 ADR-010，未通过则重新评估改造 russh-sftp
- [ ] 能力探测：posix-rename、fsync、statvfs、limits

### P0-04 跨主机文件传输

- [ ] 同时连接两台独立 SSH 测试服务器
- [ ] A → Relay 有界缓冲 → B，至少大于缓冲区几百倍的文件正常复制
- [ ] 验证取消、网络中断时状态正确
- [ ] 传输运行期间交互式 Shell 保持响应：测量**同一连接**与**独立连接**两种模式下的终端回显延迟，写入 ADR-011
- [ ] 临时文件 + rename 提交；覆盖走原子替换，不支持时拒绝
- [ ] 记录峰值内存、正确性验证方式与耗时（不预设无根据的性能数值）

**P0 Gate**：上面关键场景不通过，就先调整架构/依赖，不继续堆 UI。P0 结束时形成：依赖版本锁定、ADR-008/010/011 的结论、风险清单与简短性能基线（含并发、chunk 默认值）。

## 3. 0.1：可用的 SSH 工作台

### 3.1 功能拆解

**Hosts Workspace**
- [ ] 列出 `~/.ssh/config` 别名（含 Include），连接时用 `ssh -G` 解析
- [ ] 手动增加/编辑/删除保存的主机
- [ ] 主机列表搜索、最近使用、连接状态、不支持指令提示
- [ ] SSH Agent、密钥文件（含口令）、keyboard-interactive 与密码登录
- [ ] Tailscale IP / MagicDNS 作为普通 SSH HostName（验证即可）

**Terminal Workspace**
- [ ] 多 SSH PTY Tab，新建、切换、关闭
- [ ] VT 控制流渲染与终端大小同步
- [ ] Terminal Input / Relay Control 焦点模式
- [ ] 可解释的连接失败/断开状态

**Files Workspace**
- [ ] 两个对等面板，每栏可以绑定任意 Local/Remote Host
- [ ] 目录浏览、过滤、选择多个文件
- [ ] 创建目录、重命名、删除（确认）、复制
- [ ] 文件/目录冲突检查、安全的目标提交
- [ ] 异步分批目录加载、不安全名称标记、Windows 非法名拒绝

**Transfer Manager**
- [ ] 从 Files 创建任务并入队
- [ ] 后台执行期间终端可继续交互
- [ ] 任务进度、累计传输、取消和失败提示
- [ ] 受控并发；相同目标写入冲突处理
- [ ] 手动重试失败任务（不能静默重试破坏性操作）
- [ ] Planning 阶段冲突决策（含“应用到全部”）、条目级结果报告
- [ ] 退出确认、Interrupted 标记与临时文件清理

**基础设施**
- [ ] `config.toml`：快捷键（含逃逸键）、并发、OSC 52 策略
- [ ] 脱敏结构化日志；panic 时恢复终端

### 3.2 0.1 非目标

不会为了“看起来完整”临时追加：FTP/S3/SMB、团队登录、完整远端直接传输、rsync 同步、原生拖放、运行期持久化守护进程、所有 SSH Config 指令完全支持、自动发现 Tailnet 节点。

### 3.3 验收场景

从一个新的本地环境启动 Relay，通过 SSH 配置或添加 Host 连上 prod 和 backup；在 prod Shell 里运行 `htop`，切换 Files，将 prod 文件复制到 backup，同时切回终端；传输完成后在 backup 检查内容。以失败注入重试，不得对旧文件造成静默破坏。

## 4. 0.2：可靠性与 CLI

- [ ] `relay ssh prod`、`relay cp local:/... prod:/...` 等一致的 CLI（语法在实际实现前定稿）
- [ ] 传输任务元数据持久化；中断后明确提示恢复能力
- [ ] 断点续传：源/目标一致性校验、偏移验证、范围读写能力检测
- [ ] 强度更高的文件校验策略和可解释的完整性级别
- [ ] ProxyJump（0.2 优先级最高）；仅当自用核心场景或首批用户的日常连接被阻断时评估提前单跳到 0.1，多跳和 ProxyCommand 仍延后
- [ ] ProxyCommand、证书、硬件密钥评估
- [ ] 远端鼠标转发
- [ ] 更稳健的文件权限/符号链接/元数据保留
- [ ] 终端与文件视图之间可选的工作目录跟随（需协议/OSC 行为验证）

注意：进程退出后任务继续执行 **不等于** 恢复任务元数据。若要进程退出后持续上传，需要独立服务进程与生命周期设计，暂不计划。

## 5. 1.0 候选（基于实际需求选择）

- [ ] Tailnet 设备发现和连接诊断
- [ ] Tailscale SSH 身份模式适配
- [ ] 远端直传（A 主动连接 B）并明确凭证与安全模型
- [ ] 目录差异 / 增量同步
- [ ] 可选文件拖入路径识别
- [ ] 原生 Windows/macOS 打包、说明和兼容性完善

## 6. Issue 拆分建议

每个 GitHub Issue 应包含：
- 背景与用户场景；
- 本次唯一交付结果及不做什么；
- 输入/输出接口或界面状态；
- 验收用例（至少一个成功、一个失败）；
- 依赖项、风险及测试证据。

推荐拆分顺序：工程骨架与测试拓扑 → Host Key 校验 → SSH PTY 与 Input Encoder → SFTP 选型与 FileSystem → Remote→Remote 流与提交协议 → Task Engine → UI 四个工作空间 → 配置管理与集成测试。

## 7. Definition of Done (DoD)

任务标记完成前必须满足：
1. 行为、失败模式与产品文档一致；
2. 单元或集成测试覆盖关键安全不变量；
3. 错误消息可操作、日志无敏感信息；
4. Linux/WSL2 目标环境中至少有一份可复现的手工验证记录；
5. 涉及架构或用户交互的变化同步更新文档。

关联：[产品](PRODUCT.md) · [架构](ARCHITECTURE.md) · [测试](QUALITY.md)
