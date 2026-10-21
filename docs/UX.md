# Relay TUI 交互设计

> 版本：0.2-draft · 2026-10-20 · 以下布局为规划，不是已实现功能。

## 1. 交互目标

让用户通过少量、可发现的按键，完成 **连接主机 → 使用 Shell → 浏览文件 → 跨主机复制 → 继续使用 Shell**。以焦点清晰、操作可撤销/可确认、进度永远可见为优先级，不追求信息密度最大化。

## 2. 四个工作空间

顶部标签栏固定为 `Hosts · Files · Terminals · Transfers`，任何工作空间底部都有一行全局任务摘要和当前输入模式提示。启动时进入 Hosts。

### 2.1 Hosts — 主机与连接

```text
┌─ Relay ─ [Hosts] Files Terminals Transfers ─────────────────────┐
│ / 搜索主机                                                       │
├──────────────────────────────┬──────────────────────────────────┤
│ ● prod        ssh:prod       │ prod                             │
│ ● backup      ssh:backup     │ ubuntu@10.0.0.12:22              │
│ ○ nas         saved          │ 认证：Agent → ~/.ssh/id_ed25519   │
│ ✗ old-box     ssh:old-box    │ 状态：Connected                  │
│                              │ 不支持的配置：ProxyCommand        │
├──────────────────────────────┴──────────────────────────────────┤
│ Enter 连接/打开终端 · f 在文件中打开 · a 新增 · e 编辑 · ? 帮助   │
└─────────────────────────────────────────────────────────────────┘
```

- 列表合并 SSH 配置别名（`ssh:`，只读）、Relay 保存的主机（`saved`）与最近连接；来源始终标注。
- 详情显示：目标/用户/端口、认证方式概要、连接状态、错误原因、**不支持或被忽略的配置指令**。
- 连接状态：Unknown / Connecting / Connected / AuthFailed / HostKeyUnknown / HostKeyMismatch / NetworkError / Disconnected。状态同时用图标和文字表达，不只靠颜色。
- 首次出现的 Host Key 不自动接受：模态框显示主机名、地址、端口、算法、SHA-256 指纹；如存在其他算法的已知密钥，用醒目文案提示风险。
- Tailscale 主机初期属于普通 SSH Host，其 IP/MagicDNS 与常规域名一样管理；不把“Tailnet 在线”误写为“SSH 可用”。

### 2.2 Files — 双栏文件管理

```text
┌─ Relay ─ Hosts [Files] Terminals Transfers ─────────────────────┐
├────────────────────────────┬───────────────────────────────────┤
│ [prod ▾] /var/www          │ [backup ▾] /srv/backup            │
│ > dist/                    │   daily/                          │
│   logs/                    │   archive/                        │
│   data.tar.zst             │   README                          │
│   config.json              │                                   │
├────────────────────────────┴───────────────────────────────────┤
│ #12 prod → backup  data.tar.zst  58%  18.4 MB/s     [Control]   │
│ Tab 切换栏 · Space 标记 · c 复制 · / 过滤 · ? 帮助              │
└────────────────────────────────────────────────────────────────┘
```

两栏对等，均有独立 Location、当前路径、排序、过滤、选中项及滚动位置。可使用本机 ↔ 远程、远程 A ↔ B，或同一远程主机两个目录。

- 主机选择器（`o`）列出本机与所有主机；选择未连接的主机会触发连接流程；
- 被聚焦面板边框与高亮明显，另一栏降低强调；
- 目录加载为异步分批：显示“加载中 n 项…”，可用 `Esc` 取消，不冻结界面；
- 无法安全表示的文件名（如非 UTF-8）以转义形式显示并带 `⚠` 标记，只允许查看属性；
- 复制前弹窗显示来源、目标、条目数与冲突策略（见 §5）；
- 后台传输只有一个全局任务来源，不依赖当前面板是否还打开；
- 窄窗口（< 100 列）降级为单栏浏览 + `Tab` 切换两栏；小于最小尺寸（候选 60×16）时显示提示，而非 panic。

### 2.3 Terminals — 真实 SSH PTY

```text
┌─ Relay ─ Hosts Files [Terminals] Transfers ────────────────────┐
│ [prod ●] [prod#2 ●] [backup ✗] [+]                             │
├────────────────────────────────────────────────────────────────┤
│ ubuntu@prod:~$ htop                                            │
│ ...（持续 PTY 数据流，不是一次性 exec 输出）...                │
│                                                                │
├────────────────────────────────────────────────────────────────┤
│ [TERMINAL] F12 → Relay 控制      #12 58%                       │
└────────────────────────────────────────────────────────────────┘
```

PTY 聚焦时，所有可表达的键盘事件都发送给远程应用。禁止把 `Esc`、`Ctrl+C`、方向键等当成 TUI 全局操作而破坏 `vim`、`tmux`、`htop`。

- 进入 Terminals 且存在活动 Tab 时，默认进入 Terminal Input 模式（可配置）；
- 同一主机可有多个独立 Shell（`prod#2`）；从 Hosts / Files 可快捷打开对应主机的新终端；
- SSH 断开：屏幕冻结并保留滚动历史，顶部横幅显示原因与“`r` 重新连接（新 Shell）”，不假装远程进程仍然存活；
- 远端设置的窗口标题经清洗后显示在 Tab 上；响铃在非当前 Tab 上显示标记；
- 远端请求写剪贴板（OSC 52）默认被拒绝，并在状态栏提示一次。

### 2.4 Transfers — 独立任务视图

显示任务 ID、来源、目标、状态、文件进度（n/m）、字节进度、速度、错误详情；选中任务可展开每个条目的结果（完成/跳过/失败及原因、残留临时文件）。

操作：取消（`x`，需确认）、重试失败或中断的任务（`r`）、清除已结束任务。暂停/断点续传按路线图分期引入。

任务状态：Queued、Planning、AwaitingDecision、Running、Completed、CompletedWithIssues、Failed、Cancelled、Interrupted。UI 必须准确区分“部分写入”和“已提交完成”，并显示实际执行的完整性级别（Basic）。

## 3. 输入模式与快捷键

### 3.1 输入模式

| 模式 | 进入方式 | 按键去向 | 状态栏 |
| --- | --- | --- | --- |
| Control | 默认；Terminal Input 中按逃逸键 | Relay 快捷键 | `[CONTROL]` |
| Terminal Input | 在 Terminals 中按 `Enter` 或逃逸键 | 除逃逸键外全部发给远端 PTY | `[TERMINAL]` |
| Text Input | `/` 过滤、重命名、表单字段 | 文本框；`Enter` 确认，`Esc` 取消 | `[INPUT]` |
| Modal | 确认框、主机密钥、密码等提示 | 只响应框内按键 | 框内说明 |

模式指示**始终可见**。`Ctrl+P`、数字键等全局快捷键只在 Control 模式生效，因此在过滤框中输入 `1` 或在 vim 中按 `Ctrl+P` 都不会被 Relay 截获。

### 3.2 逃逸键

- 默认同时启用主键 `F12` 与后备键 `Ctrl+]`（可通过 `--escape-key` 或后续 `config.toml` 更换）。
- 连续按两次逃逸键：向远端发送一次该键本身（远端应用确实需要它时），并保持在 `Terminal Input` 模式。
- 首次进入 `Terminal Input` 时在状态栏提示当前生效的逃逸键；远端会话退出或断开时自动退回 `Control` 模式，防止焦点被困在已结束的会话中。

各终端模拟器逃逸键兼容矩阵（P0-02 梳理与验证）：

| 外层终端 / 环境 | `F12`（默认主键） | `Ctrl+]`（默认后备键，ASCII `0x1d`） | 备注 |
| --- | --- | --- | --- |
| xterm / VTE（GNOME Terminal、xfce4-terminal） | 发送 `\x1b[24~`，可用 | 发送 `0x1d`（crossterm 传统模式解析为 `Ctrl+5`），可用 | `KeyBinding::matches` 按控制字节等价匹配 `Ctrl+]` 与 `Ctrl+5` |
| Alacritty / Kitty / WezTerm / foot | 发送 `\x1b[24~`，可用 | 传统模式为 `0x1d`，kitty 协议下为 `Ctrl+]`，均可用 | 同时覆盖两种解码路径 |
| KDE + Yakuake / GNOME + Guake | **常被全局下拉热键拦截** | 可用 | 下拉式终端用户可直接用 `Ctrl+]` 返回 Control |
| Windows Terminal（原生 / WSL2） | 发送 `\x1b[24~`，可用 | 发送 `0x1d`，可用 | 不与默认复制/粘贴冲突 |
| macOS Terminal.app / iTerm2 | 需配合 `Fn` 或开启标准功能键，否则触发音量键 | 发送 `0x1d`，可用 | 笔记本键盘免 `Fn` 直接使用 `Ctrl+]` |
| 外层 tmux / GNU screen | 透传 `\x1b[24~`，可用 | 透传 `0x1d`（不与 `Ctrl+B` / `Ctrl+A` 冲突），可用 | 若远端在 vim tag 跳转需 `Ctrl+]`，连按两次即可发送给远端 |

### 3.3 快捷键（MVP 候选）

主键使用字母，功能键作为别名（功能键可能被外层终端拦截，笔记本键盘也未必方便）。

| 场景 | 按键 | 行为 |
| --- | --- | --- |
| Control | `1` / `2` / `3` / `4` | Hosts / Files / Terminals / Transfers |
| Control | `Tab` / `Shift+Tab` | 在当前工作空间的可聚焦区域间切换 |
| Control | `Ctrl+P` | 命令面板：搜索操作与主机 |
| Control | `?` | 快捷键与帮助（按当前工作空间过滤） |
| Control | `q` | 退出（有活动任务时确认） |
| 列表通用 | `j/k`、上下键、`g/G` | 移动高亮 / 首尾 |
| 列表通用 | `/` | 过滤 |
| Files | `Enter` / `Backspace` | 进入目录 / 返回上级 |
| Files | `Space` | 标记/取消标记 |
| Files | `o` | 切换当前栏的 Location |
| Files | `i` | 查看属性 |
| Files | `c` 或 `F5` | 从当前栏复制到另一栏 |
| Files | `n` 或 `F7` | 新建目录 |
| Files | `r` 或 `F2` | 重命名 |
| Files | `d`、`Delete` 或 `F8` | 删除（必须确认） |
| Files | `t` | 在当前主机打开新终端并进入当前目录（发送经 shell 转义的 `cd`；路径含控制字符时不提供） |
| Terminals | `Enter` | 进入 Terminal Input |
| Terminals | `[` / `]` | 上一个 / 下一个 Tab |
| Terminals | `n` / `w` | 新建 Tab / 关闭 Tab（确认） |
| Terminal Input | 逃逸键 | 返回 Control |

所有按键可在 `config.toml` 中重新绑定；冲突在启动时报告。

## 4. 鼠标

MVP 中鼠标只用于 Relay 自身（点击标签、选择列表行、滚动），不转发给远端 PTY；Terminal Input 模式下滚轮浏览本地滚动缓冲。远端鼠标转发留到 0.2。鼠标始终是可选增强，任何操作都有键盘路径。

## 5. 文件复制确认与破坏性操作

确认框至少显示以下信息：

```text
Copy 3 items (1 directory, 2 files, 1.2 GiB)
From: prod:/var/www/{dist/, data.tar.zst, config.json}
To:   backup:/srv/backup/
Strategy: Relay via local memory
Conflicts: 1 file exists → [Overwrite] [Skip] [Cancel]   ☐ Apply to all
Skipped:   2 symlinks (will be listed in the report)
[Cancel] [Copy]
```

- 冲突在此处一次性决定；目标服务器不支持原子覆盖时，“覆盖”选项不可用并说明原因；
- 误选同一路径或把目录复制到自身内部时拒绝复制，防止截断或无限递归；
- 删除目录需显示递归影响（条目数量）并确认；不可在后台无提示地删除；
- 错误消息给出“发生了什么、影响哪个主机和路径、接下来能做什么”。

## 6. 文件拖入（非 MVP，增强功能）

纯 TUI 通常接收到的是外层终端转成文本的文件路径，而不是操作系统原生 Drag & Drop 事件。因此：

- 只在显式的“选择本地文件/粘贴路径”输入上下文尝试识别拖入路径；
- 普通 Terminal Input 模式不劫持文本，也不自动运行粘贴的命令；
- 识别到本地路径后预览将上传的路径、大小和目标 Host；
- Windows Terminal + WSL2 等环境可能需要将 Windows 路径映射到运行 Relay 的环境；
- 任何路径都视为不可信输入；不得执行路径字符串作为 shell 命令。

## 7. 视觉规范

优先暗色主题，清晰的焦点边框、选中行、状态图标和分层文本。避免闪烁、频繁全屏重绘和过度彩色装饰。颜色不应是表达连接状态或失败状态的唯一手段；保持 ASCII/Unicode 退化能力（`●○✗⚠` 均有 ASCII 替代）。CJK 等宽字符需正确计算列宽。布局必须在 80×24、120×40 等尺寸下测试。

## 8. 交互验收

1. 首次使用者只靠 `?` 和命令面板可完成连接、上传、远程互传。
2. `vim` 的 `Esc`、`Ctrl+C`、`Ctrl+P` 等在 Terminal Input 时可按远程应用预期工作。
3. 大文件传输中可以继续切换工作空间、使用 Shell、浏览其他目录。
4. 任何破坏性操作都能识别作用主机与完整目标路径。
5. 不支持拖拽、功能键被拦截的终端仍有等价的键盘路径。
6. 当前输入模式在任何时刻都可从屏幕上读出。

相关：[PRD](PRODUCT.md) · [架构](ARCHITECTURE.md) · [测试](QUALITY.md)
