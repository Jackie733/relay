# Relay TUI 交互设计

> 版本：0.1-draft · 2026-10-20 · 以下布局为规划，不是已实现功能。

## 1. 交互目标

让用户通过少量、可发现的按键，完成 **连接主机 → 使用 Shell → 浏览文件 → 跨主机复制 → 继续使用 Shell**。以焦点清晰、操作可撤销/可确认、进度永远可见为优先级，不追求信息密度最大化。

## 2. 三个工作空间

### 2.1 Files — 双栏文件管理

```text
┌─ Relay ─ Files ─ Terminals ─ Transfers ─────────────────────────┐
│ Hosts:  prod ●  backup ●  local                                 │
├────────────────────────────┬───────────────────────────────────┤
│ [prod ▾] /var/www          │ [backup ▾] /srv/backup            │
│ > dist/                    │   daily/                          │
│   logs/                    │   archive/                        │
│   data.tar.zst             │   README                          │
│   config.json              │                                   │
├────────────────────────────┴───────────────────────────────────┤
│ Task #12 prod → backup  data.tar.zst  58%  18.4 MB/s            │
│ Tab focus · Space mark · F5 copy · / filter · ? help            │
└────────────────────────────────────────────────────────────────┘
```

两栏对等，均有独立 HostId、当前路径、排序、过滤、选中项及滚动位置。可使用本机 ↔ 远程、远程 A ↔ B，或同一远程主机两个目录。

- 主机选择控件显示名称、连接状态和连接错误；
- 被聚焦面板边框与高亮明显，另一栏降低强调；
- 复制前弹窗显示来源、目标、条目数与冲突策略；
- 后台传输只有一个全局任务来源，不依赖当前面板是否还打开；
- 窄窗口降级为单栏浏览 + 通过 Tab 切换目标；最小终端尺寸不足时提示，而非 panic。

### 2.2 Terminals — 真实 SSH PTY

```text
┌─ Relay / Terminals ────────────────────────────────────────────┐
│ [prod ●] [backup ●] [+]                                        │
├────────────────────────────────────────────────────────────────┤
│ ubuntu@prod:~$ htop                                            │
│ ...（持续 PTY 数据流，不是一次性 exec 输出）...                │
│                                                                │
├────────────────────────────────────────────────────────────────┤
│ F12: Relay 控制模式  │  当前：Terminal Input                    │
└────────────────────────────────────────────────────────────────┘
```

PTY 聚焦时，原则上所有可表达的键盘事件都发送给远程应用。禁止把 `Esc`、`Ctrl+C`、方向键等当成 TUI 全局操作而破坏 `vim`、`tmux`、`htop`。初步选择 **F12 作为从远程输入切换到 Relay 控制模式的唯一保留键**，须经过 Linux/Windows Terminal/WSL2/macOS 兼容性验证，并提供可配置替代。

- Terminal Input：按键发给 PTY；窗口尺寸改变时通知 SSH 服务端；
- Relay Control：允许切换 Tab、工作空间及面板；显式返回 Terminal Input；
- SSH 断开：保留必要的滚动历史和原因展示，不假装远程进程仍然存活；
- 可以在 Files 中选择主机后快捷打开对应终端；同一主机可有多个独立 Shell；
- 对终端 escape sequences、剪贴板控制、超链接等进行安全策略限制。

### 2.3 Transfers — 独立任务视图

显示任务 ID、来源、目标、状态、文件进度、总体进度、传输速度、累计字节与错误详情。功能包括查看、取消、对可重试错误重新运行；暂停/断点续传按路线图分期引入。

任务状态至少区分：Queued、Preparing、Transferring、Verifying、Completed、Failed、Cancelled、Interrupted。UI 必须准确区分“部分写入”和“已提交完成”。

## 3. 初步快捷键（MVP 候选）

| 场景 | 按键 | 行为 |
| --- | --- | --- |
| Relay Control | `Tab` / `Shift+Tab` | 在当前可聚焦区域间切换 |
| Relay Control | `Ctrl+P` | 命令面板、搜索操作与主机 |
| Relay Control | `1` / `2` / `3` | Files / Terminals / Transfers |
| Files | `j/k`、上下键 | 移动高亮 |
| Files | `Enter` | 进入目录 / 预览条目（依类型） |
| Files | `Backspace` | 返回上级目录 |
| Files | `Space` | 标记/取消标记文件 |
| Files | `F5` | 从当前面板复制到另一面板 |
| Files | `F2` | 重命名 |
| Files | `Delete` | 删除，必须确认 |
| Files | `/` | 过滤当前目录 |
| Relay Control | `?` | 快捷键和帮助 |
| Terminal Input | `F12`（候选） | 进入 Relay Control |
| Relay Control | `F12`（候选） | 返回 Terminal Input（当有终端） |

注意：`Ctrl+P` 等组合键 **仅在 Relay Control 模式** 生效；PTY 模式须保留完整远程输入。终端外层可能拦截功能键，P0 必须验证并设计替代路径。

## 4. 主机与连接管理

- 入口为独立 Hosts 选择器，展示手动保存主机、SSH Config 别名与最近连接；
- 连接卡片包含：别名、目标/用户名、认证方式概要、连接状态与错误原因；
- 连接状态区分 Unknown / Connecting / Connected / AuthFailed / HostKeyMismatch / NetworkError；
- 默认不自动接受首次出现的 Host Key；显示算法及指纹并要求确认；
- Tailscale 主机初期属于普通 SSH Host，其 IP/MagicDNS 与常规域名一样管理；不把“Tailnet 在线”误写为“SSH 可用”。

## 5. 文件复制确认与破坏性操作

确认框至少显示以下信息：

```text
Copy 1 item
From: prod:/var/www/data.tar.zst
To:   backup:/srv/backup/data.tar.zst
Strategy: Relay via local memory
Conflict: Ask before overwrite
[Cancel] [Copy]
```

- 目标已存在时先检查并明确执行冲突策略；
- 误选同一路径时拒绝复制，防止截断源文件；
- 不可保证原子覆盖时须显式提示；在 MVP 中倾向拒绝危险覆盖；
- 删除目录需显示递归影响并有确认；不可在后台无提示地删除；
- 错误消息给出“发生了什么、影响哪个路径、接下来能做什么”。

## 6. 文件拖入（非 MVP，增强功能）

纯 TUI 通常接收到的是外层终端转成文本的文件路径，而不是操作系统原生 Drag & Drop 事件。因此：

- 只在显式的“选择本地文件/粘贴路径”输入上下文尝试识别拖入路径；
- 普通 Terminal Input 模式不劫持文本，也不自动运行粘贴的命令；
- 识别到本地路径后预览将上传的路径、大小和目标 Host；
- Windows Terminal + WSL2 等环境可能需要将 Windows 路径映射到运行 Relay 的环境；
- 任何路径都视为不可信输入；不得执行路径字符串作为 shell 命令。

## 7. 视觉规范

优先暗色主题，清晰的焦点边框、选中行、状态图标和分层文本。避免闪烁、频繁全屏重绘和过度彩色装饰。颜色不应是表达连接状态或失败状态的唯一手段；保持 ASCII/Unicode 退化能力。布局必须在 80 列、120 列等尺寸下测试。

## 8. 交互验收

1. 首次使用者只靠 `?` 和命令面板可完成连接、上传、远程互传。
2. `vim` 的 `Esc`、`Ctrl+C`、`Ctrl+P` 等在 Terminal Input 时可按远程应用预期工作。
3. 大文件传输中可以继续切换工作空间、使用 Shell、浏览其他目录。
4. 任何破坏性操作都能识别作用主机与完整目标路径。
5. 不支持拖拽的终端仍有等价的键盘路径。

相关：[PRD](PRODUCT.md) · [架构](ARCHITECTURE.md) · [测试](QUALITY.md)
