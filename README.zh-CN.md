# nuphus-mcp

[![npm version](https://img.shields.io/npm/v/@nuphus/nuphus-mcp.svg)](https://www.npmjs.com/package/@nuphus/nuphus-mcp) [![npm downloads](https://img.shields.io/npm/dm/@nuphus/nuphus-mcp.svg)](https://www.npmjs.com/package/@nuphus/nuphus-mcp) [![GitHub stars](https://img.shields.io/github/stars/mrpulor-gh/nuphus-mcp.svg)](https://github.com/mrpulor-gh/nuphus-mcp) [![License: MIT](https://img.shields.io/github/license/mrpulor-gh/nuphus-mcp.svg)](LICENSE) [![CI](https://github.com/mrpulor-gh/nuphus-mcp/actions/workflows/ci.yml/badge.svg)](https://github.com/mrpulor-gh/nuphus-mcp/actions/workflows/ci.yml)

**桌面自动化 MCP Server —— 为任意 AI 智能体提供"计算机使用"能力。看屏幕、控制窗口/键鼠、驱动 Chrome，经 Model Context Protocol（stdio）接入。**

`nuphus-mcp` 是一个轻量、跨平台的**桌面自动化 MCP Server**，把桌面与浏览器自动化能力
封装为标准 MCP 工具。它通过 stdio 走 JSON-RPC 2.0 —— 无守护进程、无网络服务、
单二进制。Claude Desktop、Cursor、VS Code、Copilot、任意 MCP 客户端乃至 Nuphus
自身都能即连即用，控制屏幕、窗口、键鼠与 Chrome —— **为任意 AI 智能体提供
"计算机使用"能力：桌面与浏览器自动化无需 API Key；内置本地 OCR；视觉能力
支持接入你自己的视觉大模型（OpenAI 兼容协议，BYOK）**。

> **国内镜像**：本仓库同时镜像到 [Gitee](https://gitee.com/nuphus/nuphus-mcp)，
> 国内访问更快、默认显示中文文档。GitHub 打不开时用 Gitee。
>
> **Nuphus 生态组件**：[Nuphus](https://github.com/mrpulor-gh/nuphus) —— 本地优先的 AI 助手，真实桌面执行力 + 手机第二块屏幕。nuphus-mcp 将同一套桌面/浏览器自动化能力开放给任意 MCP 客户端。
>
> **使用 DeepSeek Harness (DSH)？** nuphus-mcp 本体是标准 stdio MCP Server，**不是** DSH/Cordis 插件。请安装专用 [dsh-nuphus-mcp](https://github.com/mrpulor-gh/dsh-nuphus-mcp) 插件（见 [DeepSeek Harness (DSH)](#deepseek-harness-dsh)）。

```
┌──────────────────┐   stdio JSON-RPC   ┌──────────────────────┐
│  任意 MCP 客户端  │  ───────────────►  │      nuphus-mcp      │
│  (Claude/Cursor/ │  ◄───────────────  │  desktop-api crate   │──► 屏幕/窗口/键鼠
│   Nuphus 自身)    │    单行 JSON       │  nuphus-browser crate│──► Chrome (CDP)
└──────────────────┘                    └──────────────────────┘
```

## 特性

- **38 个 MCP 工具**（桌面 15 + 浏览器 23）—— 完整参考见
  [TOOLS.md](TOOLS.md) / [TOOLS.zh-CN.md](TOOLS.zh-CN.md)。
- **桌面自动化**：屏幕分辨率、截图（PNG/base64）、窗口列表、窗口激活/截图/移动/缩放/信息查询、
  鼠标点击/拖拽/滚轮/定位、键盘输入/快捷键、剪贴板写入/清空 —— 基于
  `desktop-api` crate（xcap + Win32），不依赖 Tauri。
- **计算机视觉双件套**：`desktop_vision`（BYOK —— 截图发送到你自己的视觉模型，OpenAI
  兼容 API）+ `desktop_perceive`（本地 OCR，PaddleOCR，首次运行自动下载模型；
  可选 YOLO 图标检测）。二者配合让 AI 智能体**同时获得语义理解与像素级精确坐标**
  —— 这是 Nuphus 桌面应用实战验证过的 vision→perceive 流程。BYOK 环境变量、
  模型配置与推荐配合见 [TOOLS.md](TOOLS.md)。
- **浏览器自动化**：导航、快照（无障碍树 `@N` 引用）、点击、输入、批量脚本、
  滚动、正文提取、截图、JS 执行、前进/后退、等待、Cookie 读写/导入、文件上传/拖放、
  标签页、下载目录 —— 基于 `nuphus-browser`（chromiumoxide CDP）。
- **零成本 stdio**：无 HTTP 服务、无常驻进程。进程从 stdin 读单行 JSON，向
  stdout 写响应。
- **安全优先**：破坏性工具按 MCP 规范标注；可选严格确认模式；截图、上传和文件拖放路径校验。

## 仓库结构

```
nuphus-mcp/
├── Cargo.toml                  # workspace 根
├── TOOLS.md / TOOLS.zh-CN.md   # 38 工具参考文档
├── crates/
│   ├── nuphus-mcp/             # MCP Server（本仓库产品）
│   ├── nuphus-browser/         # 浏览器自动化核心（CDP）
│   └── desktop-api/            # 桌面控制核心（vendored）
└── ...
```

## 环境依赖

- **Rust 工具链（stable）** —— 通过 Cargo 从源码构建。
- **Chrome 或 Edge** —— browser 工具必需。server 自动查找本机已安装的浏览器；
  找不到时 `browser_*` 工具返回明确错误。
- **Windows 优先推荐** —— 桌面控制完整支持，见下方平台支持。

## 平台支持

| 平台 | 浏览器工具 | 桌面工具 |
|------|-----------|---------|
| Windows | 全量 | 全量（Win32 API） |
| macOS | 全量 | 桌面输入需在「系统设置 → 隐私与安全性 → 辅助功能」中授权 |
| Linux | 可用 | 部分支持——窗口/输入能力受限 |

> **执行 HUD**：全平台非侵入执行反馈——Windows 屏幕浮条（开始 ▶ / 完成 ✓ 实时显示），macOS/Linux 完成后发**系统通知**（`NUPHUS_MCP_HUD=off` 关闭）。绝不以激活窗口作为可见性手段。

## API Key 与本地模型

### 视觉理解（可选，BYOK）

`desktop_vision` 使用**你自己的**视觉模型（OpenAI 兼容 Chat Completions）。
不调用该工具就不需要任何配置；未配置时工具返回明确错误，不会静默失败。

| 环境变量 | 必填 | 默认值 | 说明 |
|----------|------|--------|------|
| `NUPHUS_MCP_VISION_API_KEY` | ✅ | — | 视觉模型 API Key |
| `NUPHUS_MCP_VISION_BASE_URL` | — | `https://api.openai.com/v1` | OpenAI 兼容 base URL |
| `NUPHUS_MCP_VISION_MODEL` | ✅ | — | 模型 ID，如 `gpt-4o-mini`、`qwen-vl-max` |

### 对接外部浏览器（反检测 / 指纹浏览器）

默认情况下 `browser_*` 工具启动并管理自己的 Chrome 实例。若要改为驱动
**外部浏览器**——例如反检测 / 指纹浏览器——用调试端口启动它，并把地址
告诉 server：

| 环境变量 | 必填 | 默认值 | 说明 |
|----------|------|--------|------|
| `NUPHUS_MCP_BROWSER_CDP_URL` | — | — | 外部 CDP 端点，如 `http://127.0.0.1:9222` |

```sh
# 示例：用调试端口启动你的指纹浏览器
chrome --remote-debugging-port=9222 --user-data-dir=...
```

```jsonc
// MCP 客户端配置
"env": { "NUPHUS_MCP_BROWSER_CDP_URL": "http://127.0.0.1:9222" }
```

配置后，`browser_*` 工具将 attach 到该端点，不再启动托管 Chrome；attach
失败会返回明确错误（不会静默回退到错误的浏览器）。外部浏览器归你所有——
server 退出时不会杀掉它。

### 保留登录态：让 `browser_*` 操作带扩展 / 登录态的原生浏览器

想让 `browser_*` 工具操作带你自己扩展 / 书签 / 登录态的浏览器？先了解一个
**Chrome 136+ 的硬性限制**：Chrome 官方安全变更后，`--remote-debugging-port`
与 `--remote-debugging-pipe` 对**默认用户数据目录直接失效**，必须搭配
`--user-data-dir` 指向非默认目录。这是防窃密木马（infostealer）通过本地调试
端口偷取真实 Cookie 的刻意设计——**不是 nuphus-mcp 的缺陷**，也没有任何 flag /
注册表策略能绕过（`RemoteDebuggingAllowed` 策略只能"允许/禁止"这些开关，
不能解除默认目录限制）。

因此"真实默认 profile + 可被 CDP 操控"在 136+ 上互斥。按以下优先级选一种：

**方案 A（推荐，最简单）：在 nuphus 专用 profile 登录一次**

nuphus-mcp 默认管理自己的 Chrome 实例（独立 `--user-data-dir`，天然可调试）。
打开它，手动登录需要保持会话的站点一次，登录态即持久化到该 profile，之后
`browser_*` 调用全部自带这些会话。零配置、零复制。

**方案 B：复制真实 profile 生成可调试副本（保留扩展 / 书签 / 登录态）**

需要原浏览器的扩展与登录态时，复制真实 profile 再启动：

1. 先**完全退出**正在运行的真实 Chrome / Edge（profile 锁冲突）
2. 复制 profile 到非默认目录：
   - Windows：`copy "%LOCALAPPDATA%\Google\Chrome\User Data\Default" <副本>\Default`
   - macOS：`cp -R ~/Library/"Application Support"/Google/Chrome/Default <副本>/Default`
   - Linux：`cp -R ~/.config/google-chrome/Default <副本>/Default`
3. 用副本启动并开调试端口：`chrome --remote-debugging-port=9222 --user-data-dir=<副本>`
4. 配置外部 attach：`NUPHUS_MCP_BROWSER_CDP_URL=http://127.0.0.1:9222`
   （配合 `NUPHUS_BROWSER_EXE_PATH` / `NUPHUS_BROWSER_USER_DATA_DIR` 可自动自愈端口）

注意：Windows 下 Cookie / 密码为 DPAPI 用户级加密，副本在同用户下可直接解密，
登录态基本完整；macOS 部分凭据在钥匙串，个别站点需重新登录。副本可能数百 MB
到 GB 级，且不能与真实浏览器同时运行。

**方案 C：桌面视觉自动化：操控正在运行的真实浏览器窗口**

如果目标是操作用户**当前运行中**的真实浏览器（DOM 级做不到这一点），那属于
桌面 OCR + 键鼠链路（`desktop_perceive` / `desktop_*` 工具），以视觉方式点击、
输入、读取屏幕，不依赖 CDP——但没有 DOM 访问与登录态注入能力。

### perceive 模型（本地，自动下载）

`desktop_perceive` 用 ONNX Runtime 本地运行 PaddleOCR 和 YOLO 图标检测。首次
调用把 OCR 模型和 `icon_detect.onnx` **一起**自动下载到
`%APPDATA%\Nuphus\models`（或 `NUPHUS_MODELS_DIR`）。下载失败时返回明确错误
并附手动指引。YOLO 在运行时是可选增强：下载失败时 perceive 仍返回 OCR 结果
并报告 `yolo_available: false`（可通过 `NUPHUS_MCP_YOLO_MODEL_URL` 指定自定
义来源）。详见 [TOOLS.zh-CN.md → 视觉与本地模型](TOOLS.zh-CN.md#视觉与本地模型)。

其余工具无需任何 API key。

## 安装与运行

**npm 安装（推荐 —— 全平台，预编译二进制）：**

```sh
npm install -g @nuphus/nuphus-mcp
```

`nuphus-mcp` meta 包会自动安装当前平台对应的预编译二进制（Windows
x64/arm64、macOS arm64、Linux x64/arm64），并把 `nuphus-mcp` 命令加入 PATH。
无需 Rust 工具链：

```sh
nuphus-mcp   # stdio MCP server
```

**源码构建**（需要 Rust 工具链）：

```sh
cargo build --release -p nuphus-mcp
# 二进制在 target/release/nuphus-mcp(.exe)
```

server 从 stdin 读换行分隔的 JSON，向 stdout 写 JSON-RPC 响应；日志一律走
stderr。

```sh
# 快速冒烟
echo '{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test"}}}' | nuphus-mcp
```

### 🔒 推荐：开启严格确认（strict confirmation）

该 server 能物理控制它所在的机器。默认情况下写工具**不要求确认**即执行；我们
**强烈建议**开启严格确认，使破坏性操作必须由客户端显式携带 `"confirm": true`
参数（否则工具以 `isError` 拒绝）。

**以下任一方式即可开启：**

```sh
# 命令行参数
nuphus-mcp --confirm-write

# 环境变量（推荐 —— 对所有客户端生效，配置最简单）
export NUPHUS_MCP_CONFIRM_WRITE=1      # macOS / Linux
setx NUPHUS_MCP_CONFIRM_WRITE 1        # Windows（持久生效，新开的 shell）

# MCP 客户端 args
"args": ["--confirm-write"]
```

**Claude Desktop** —— 推荐的 `claude_desktop_config.json`：

```json
{
  "mcpServers": {
    "nuphus-mcp": {
      "command": "nuphus-mcp",
      "args": ["--confirm-write"]
    }
  }
}
```

> 优先使用环境变量：一条设置对本机所有 MCP 客户端生效。完整威胁模型见
> [SECURITY.md](SECURITY.md) 与 [TOOLS.md 的安全标注章节](TOOLS.md#safety-annotations)。

## MCP 客户端配置

把任意 MCP 客户端指向 `nuphus-mcp` 即可。`npm install -g
@nuphus/nuphus-mcp` 之后命令已在 PATH 上；否则使用二进制的绝对路径
（`nuphus-mcp` / `nuphus-mcp.exe`）。

**Claude Desktop** — `claude_desktop_config.json`：

```json
{
  "mcpServers": {
    "nuphus-mcp": {
      "command": "nuphus-mcp",
      "args": []
    }
  }
}
```

**任意 MCP 客户端**（通用 mcpServers JSON）：

```json
{
  "mcpServers": {
    "nuphus-mcp": {
      "command": "nuphus-mcp",
      "args": [],
      "env": {}
    }
  }
}
```

支持的方法：`initialize`、`notifications/initialized`、`ping`、`tools/list`、`tools/call`。

## 接入 DeepSeek Harness（DSH）

**推荐：安装官方
[dsh-nuphus-mcp](https://github.com/mrpulor-gh/dsh-nuphus-mcp) 插件**（Gitee
镜像：[gitee.com/nuphus/dsh-nuphus-mcp](https://gitee.com/nuphus/dsh-nuphus-mcp)），
即可把 nuphus-mcp 以原生工具的形式挂载进 DSH，零配置、默认开启
`--confirm-write`、无需改动代码：

```sh
npx -p @deepseek-ai/dsh dsh plugin --profile web add github:mrpulor-gh/dsh-nuphus-mcp
```

**手动接入方式**：`nuphus-mcp` 是纯 stdio MCP server，可直接经 DSH 内置的
MCP 客户端（`@deepseek-ai/dsh-mcp-client`）接入，无需改动代码。挂载到 DSH 的
`cordis.yml` / patch：

```yaml
- id: nuphus-mcp
  name: '@deepseek-ai/dsh-mcp-client'
  config:
    serverName: nuphus-mcp
    transport: stdio
    command: nuphus-mcp
    args: ["--confirm-write"]
    toolCallTimeoutMs: 120000   # DSH 默认 60000，截图/OCR 会超时
```

工具以 `mcp__nuphus-mcp__*` 注册（如 `mcp__nuphus-mcp__desktop_click`）。DSH
需运行在被控机器的桌面会话里。

## Demo

自包含 stdio 客户端，完整走 `initialize → tools/list → tools/call`：

```sh
cargo build -p nuphus-mcp
cargo run -p nuphus-mcp --example demo
```

## 测试

```sh
cargo check --workspace
cargo test -p nuphus-mcp          # 协议 + 安全 + 视觉 + 模型测试（28）
```

## 安全

本 server 能物理控制所在机器。部署前请阅读 [SECURITY.md](SECURITY.md) 和
[TOOLS.zh-CN.md 的安全标注章节](TOOLS.zh-CN.md#安全标注)。建议以
`--confirm-write`（或 `NUPHUS_MCP_CONFIRM_WRITE=1`）运行，使写工具要求参数显式
携带 `"confirm": true`。

## License

MIT