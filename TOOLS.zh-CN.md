# nuphus-mcp 工具参考文档

本文档描述当前 `nuphus-mcp` 构建暴露的全部工具。所有工具与 `tools/list`
返回的 schema 一致，本文档是权威的人类可读参考。

- **工具总数：45** —— 桌面 22 · 浏览器 23
- **协议**：JSON-RPC 2.0 over stdio（换行分隔 JSON）
- **协议版本**：`2024-11-05`
- **支持的方法**：`initialize`、`notifications/initialized`、`ping`、`tools/list`、`tools/call`

---

## 目录

- [安全标注](#安全标注)
- [调用工具](#调用工具)
- [视觉与本地模型](#视觉与本地模型)
- [桌面工具（22）](#桌面工具22)
- [浏览器工具（23）](#浏览器工具23)
- [端到端示例](#端到端示例)

---

## 安全标注

每个工具在 `tools/list` 中都带 `annotations` 字段（MCP 规范）。

- **`destructiveHint: true`**（30 个）——写操作，会改变系统或页面状态。客户端
  在调用前应展示确认 UI。
- **`readOnlyHint: true`**（15 个）——只读操作，可安全自动执行。

只读工具（15 个）：

| 工具 |
|------|
| `desktop_screen_size` |
| `desktop_windows_list` |
| `desktop_window_info` |
| `desktop_vision` |
| `desktop_perceive` |
| `desktop_targets_list` |
| `desktop_semantic_observe` |
| `desktop_semantic_candidate` |
| `desktop_verify_state` |
| `browser_snapshot` |
| `browser_extract` |
| `browser_cookies_get` |
| `browser_list_tabs` |
| `browser_list_downloads` |
| `browser_wait_for` |

其余 30 个工具均标注 `destructiveHint`。注意：`desktop_mouse` 在 schema 层面
保守标注为 destructive（因为其 `action` 可能是 click/scroll 等写操作）；
运行时确认检查只把 `action: "position"` 视为只读。`desktop_vision` /
`desktop_perceive` 是只读工具（读取屏幕）；`desktop_perceive` 首次调用可能
触发 OCR 模型下载这一副作用（见 [视觉与本地模型](#视觉与本地模型)）。

### 严格确认模式

启动时加 `--confirm-write` 参数，或设置环境变量 `NUPHUS_MCP_CONFIRM_WRITE=1`
开启**严格确认模式**。此模式下所有写工具都要求参数显式携带 `"confirm": true`，
否则调用被拒绝（`isError: true`），不产生任何副作用。只读工具不受影响。

```json
{
  "jsonrpc": "2.0",
  "id": 10,
  "method": "tools/call",
  "params": {
    "name": "desktop_input",
    "arguments": { "mode": "type", "hwnd": 123456, "text": "hello", "confirm": true }
  }
}
```

### 路径校验

截图保存路径会校验，拒绝路径穿越（`..`）和系统保护目录；
`browser_upload` 要求文件真实存在；`browser_drag_files` 只接受真实存在的文件或
目录，并在交给 Chrome 前将路径规范化为绝对路径。

---

## 调用工具

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "tools/call",
  "params": {
    "name": "<工具名>",
    "arguments": { "<参数>": <值> }
  }
}
```

响应为标准 MCP `CallToolResult`。语义预期的失败（参数错误、路径被拒、窗口
不存在、浏览器无法启动等）返回 `isError: true`，人类可读消息在
`content[0].text`。协议级错误（未知方法、未 initialize）返回 JSON-RPC `error`。

---

## 视觉与本地模型

### `desktop_vision` — BYOK（自带 Key）

`desktop_vision` 把截图发送到**你自己的**视觉模型，支持两种协议：
- **OpenAI 兼容** Chat Completions（默认，`image_url` 内容）——适用于 OpenAI、
  MiniMax、通义、Ollama、vLLM 等；
- **Anthropic 原生** Messages API——把 `NUPHUS_MCP_VISION_BASE_URL` 指向
  `https://api.anthropic.com/v1` 即可自动识别协议（或显式
  `NUPHUS_MCP_VISION_PROVIDER=anthropic`）。

不调用该工具就不需要任何配置；未配置时工具返回 `isError: true` 并明确点名缺失的环境变量。

| 环境变量 | 必填 | 默认值 | 说明 |
|----------|------|--------|------|
| `NUPHUS_MCP_VISION_API_KEY` | **是** | — | 视觉模型 API Key |
| `NUPHUS_MCP_VISION_BASE_URL` | 否 | `https://api.openai.com/v1` | base URL（Claude 用 `https://api.anthropic.com/v1`） |
| `NUPHUS_MCP_VISION_MODEL` | **是** | — | 模型 ID，如 `gpt-4o-mini`、`qwen-vl-max`、`claude-sonnet-4-5` |
| `NUPHUS_MCP_VISION_PROVIDER` | 否 | `auto` | `auto` \| `openai` \| `anthropic`；`auto` 按 base URL host 自动识别 |
| `NUPHUS_MCP_VISION_MAX_TOKENS` | 否 | `1024` | 最大输出 token 数（智谱 GLM-4V-Flash 上限 1024；文本多时可调大） |

```sh
# OpenAI 兼容（默认）
set NUPHUS_MCP_VISION_API_KEY=sk-...
set NUPHUS_MCP_VISION_MODEL=qwen-vl-max
# 可选：set NUPHUS_MCP_VISION_BASE_URL=https://your-gateway/v1

# Anthropic / Claude —— 从 base URL 自动识别协议
set NUPHUS_MCP_VISION_API_KEY=sk-ant-...
set NUPHUS_MCP_VISION_BASE_URL=https://api.anthropic.com/v1
set NUPHUS_MCP_VISION_MODEL=claude-sonnet-4-5
```

### `desktop_perceive` — 本地 OCR + YOLO 模型

`desktop_perceive` 用 ONNX Runtime 本地运行 PaddleOCR。首次调用自动下载 OCR
模型到 `%APPDATA%\Nuphus\models`（或 `NUPHUS_MODELS_DIR`）。下载源：
`hf-mirror.com/SWHL/RapidOCR`（det/rec ONNX）与 `gitee.com/paddlepaddle/PaddleOCR`
（字符字典）。下载失败时工具返回明确错误并附手动下载指引。

- **YOLO 图标检测**（`icon_detect.onnx`）随 OCR 模型一起自动下载（来源：
  `onnx-community/OmniParser-icon_detect_640x640`，优先 hf-mirror）。它在运行
  时是*可选*增强：下载失败时 `desktop_perceive` 仍返回 OCR 结果并报告
  `yolo_available: false`。设置 `NUPHUS_MCP_YOLO_MODEL_URL` 为 `.onnx` 直链可
  覆盖默认来源（如完整 ~80MB 的 OmniParser 导出或私有镜像）。
- `NUPHUS_MCP_NO_MODEL_DOWNLOAD=1` 跳过自动下载（受限网络/CI 快速失败）。
- 运行需要 `onnxruntime.dll` 可加载（Nuphus 主程序旁已内置；独立运行时请把它
  复制到 `nuphus-mcp.exe` 同目录）。

### 推荐配合：`desktop_vision` + `desktop_perceive`

这两个工具**设计为成对使用**：

1. `desktop_vision` — 用你自己的视觉大模型理解屏幕（布局、文字、图标），
   提供语义理解但**坐标不精确**。
2. `desktop_perceive` — 用本地 OCR + YOLO 定位 UI 元素的精确坐标，
   返回精确的 `center` 坐标但**无语义理解**。
3. 点击时使用 `desktop_perceive` 返回的 `center` 坐标 —— 绝不用
   `desktop_vision` 估算的坐标。

这正是 Nuphus 桌面应用实战验证过的流程：*vision 理解语义，perceive 提供精度*。

---

## 桌面工具（22）

桌面工具控制本机：屏幕、窗口、鼠标、键盘与剪贴板。Windows 上基于 Win32 实现
（`desktop-api` crate：xcap 截屏 + SendInput）；macOS/Linux 上鼠标键盘回退到
`enigo`、剪贴板回退到 `arboard`。macOS 桌面输入需要宿主应用获得辅助功能授权。

### desktop_screen_size

获取屏幕分辨率（宽 × 高）。

| 参数 | 类型 | 必填 | 默认 | 说明 |
|------|------|------|------|------|
| （无） | | | | 无参数 |

**示例**
```json
{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"desktop_screen_size","arguments":{}}}
```
**返回** `{"width":1920,"height":1080}`

---

### desktop_screenshot

全屏截图（或 region 区域截图），保存为 PNG（无损）。未传 `path` 时以 base64 内联返回。

| 参数 | 类型 | 必填 | 默认 | 说明 |
|------|------|------|------|------|
| `path` | string | 否 | - | 保存路径（自动补 `.png`）；不传则返回 base64 |
| `region` | object | 否 | - | 裁剪区域 `{x, y, width, height}`；不传则全屏 |

**示例 — 内联 base64**
```json
{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"desktop_screenshot","arguments":{}}}
```
**返回** `{"format":"png","data":"<base64>","width":1920,"height":1080}`

**示例 — 保存文件**
```json
{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"desktop_screenshot","arguments":{"path":"C:/Users/me/Desktop/shot.png"}}}
```
**返回** `{"path":"C:/Users/me/Desktop/shot.png","format":"png","size":123456,"width":1920,"height":1080}`

---

### desktop_windows_list

列出所有可见操作系统窗口（hwnd / 标题 / 位置）。

| 参数 | 类型 | 必填 | 默认 | 说明 |
|------|------|------|------|------|
| （无） | | | | 无参数 |

**示例**
```json
{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"desktop_windows_list","arguments":{}}}
```
**返回** 可见窗口 JSON 数组（含 `hwnd`、`title`、位置字段）。

---

### desktop_window_activate

通过 `hwnd` 将窗口激活到前台。窗口操作（截图/点击/输入）前必须先激活目标
窗口，否则操作可能作用于错误窗口或失败。

| 参数 | 类型 | 必填 | 默认 | 说明 |
|------|------|------|------|------|
| `hwnd` | integer | **是** | - | 来自 `desktop_windows_list` 的窗口句柄 |

**示例**
```json
{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"desktop_window_activate","arguments":{"hwnd":123456}}}
```

---

### desktop_window_screenshot

截取指定窗口保存为 PNG（通过 `hwnd` 或 `title` 定位，至少提供一个）。
未传 `path` 时以 base64 返回。

| 参数 | 类型 | 必填 | 默认 | 说明 |
|------|------|------|------|------|
| `title` | string | 否* | - | 窗口标题子串 |
| `hwnd` | integer | 否* | - | 来自 `desktop_windows_list` 的窗口句柄 |
| `path` | string | 否 | - | 保存路径（自动补 `.png`）；不传则返回 base64 |

\* `title` / `hwnd` 至少提供一个。

**示例**
```json
{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"desktop_window_screenshot","arguments":{"title":"记事本","path":"C:/Users/me/Desktop/notepad.png"}}}
```

---

### desktop_window_move

移动窗口到指定屏幕坐标（Windows：`SetWindowPos`，保持大小与 Z 序，并显示窗口）。
`hwnd` 从 `desktop_windows_list` 获取。

| 参数 | 类型 | 必填 | 默认 | 说明 |
|------|------|------|------|------|
| `hwnd` | integer | **是** | - | 来自 `desktop_windows_list` 的窗口句柄 |
| `x` | integer | **是** | - | 目标 X 屏幕坐标 |
| `y` | integer | **是** | - | 目标 Y 屏幕坐标 |

**示例**
```json
{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"desktop_window_move","arguments":{"hwnd":123456,"x":100,"y":100}}}
```
**返回** `{"hwnd":123456,"x":100,"y":100,"moved":true}`

---

### desktop_window_resize

缩放窗口到指定 `width` × `height`（Windows：`SetWindowPos`，保持位置与 Z 序，
并显示窗口）。

| 参数 | 类型 | 必填 | 默认 | 说明 |
|------|------|------|------|------|
| `hwnd` | integer | **是** | - | 来自 `desktop_windows_list` 的窗口句柄 |
| `width` | integer | **是** | - | 新窗口宽度（像素，> 0） |
| `height` | integer | **是** | - | 新窗口高度（像素，> 0） |

**示例**
```json
{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"desktop_window_resize","arguments":{"hwnd":123456,"width":800,"height":600}}}
```
**返回** `{"hwnd":123456,"width":800,"height":600,"resized":true}`

---

### desktop_window_info

查询窗口详细信息：标题、可见性、最小化/最大化状态、窗口矩形与客户区矩形
（屏幕坐标）、进程 ID/名与窗口类名。只读。

| 参数 | 类型 | 必填 | 默认 | 说明 |
|------|------|------|------|------|
| `hwnd` | integer | **是** | - | 来自 `desktop_windows_list` 的窗口句柄 |

**示例**
```json
{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"desktop_window_info","arguments":{"hwnd":123456}}}
```
**返回**
```json
{"hwnd":123456,"title":"记事本","visible":true,"minimized":false,"maximized":false,"window":{"x":100,"y":100,"width":800,"height":600},"client":{"x":104,"y":132,"width":792,"height":564},"process_id":1234,"process_name":"notepad.exe","class_name":"Notepad"}
```

---

### desktop_vision

用**你自己的**视觉模型理解截图（BYOK —— OpenAI 兼容或 Anthropic 原生）。需要
`NUPHUS_MCP_VISION_API_KEY` 与 `NUPHUS_MCP_VISION_MODEL`；协议按 base URL
自动识别（或用 `NUPHUS_MCP_VISION_PROVIDER` 指定），见
[视觉与本地模型](#视觉与本地模型)。未传 `path` 时先截全屏。

| 参数 | 类型 | 必填 | 默认 | 说明 |
|------|------|------|------|------|
| `path` | string | 否 | - | 图片文件路径（PNG）；不传则先截全屏 |
| `prompt` | string | 否 | - | 给视觉模型的可选指令 |

**示例**
```json
{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"desktop_vision","arguments":{"prompt":"What is on the screen?"}}}
```
**返回** `{"description":"<模型文本>"}` —— 未配置时：
`isError: true` + `NUPHUS_MCP_VISION_API_KEY required ...`。

---

### desktop_perceive

用**本地 OCR（PaddleOCR）** + 可选 **YOLO 图标检测**定位截图中的 UI 元素。
首次运行自动下载 OCR 模型（见 [视觉与本地模型](#视觉与本地模型)）。未传
`path` 时先截图 —— 默认全屏，传了 `region` 则只截该区域。返回元素含 `id`、
`kind`（text/button/input/icon）、`text`、`rect`、`center`、`confidence` 与
`source`（ocr/yolo/both）。

| 参数 | 类型 | 必填 | 默认 | 说明 |
|------|------|------|------|------|
| `path` | string | 否 | - | 图片文件路径（PNG）；不传则先截图 |
| `region` | object | 否 | - | 要截取并分析的区域 `{x, y, width, height}`；不传则全屏。与 `path` 互斥 |

**坐标语义**：由工具自己截图时，`rect` 与 `center` 均为**屏幕坐标**，可直接点击
`center`。`geometry` 返回被分析图像覆盖的屏幕矩形（`{x, y, width, height}`，其中
`x`/`y` 为图像左上角像素的屏幕位置），`coordinate_space` 为 `"screen"`。起点在屏幕
外的区域会被裁剪到屏幕内，因此 `geometry.x`/`y` 是实际得到的图像原点、而不一定是
请求的原点；超出屏幕边缘的区域会被缩短。传入 `path` 时图像屏幕原点未知：坐标相对
于该 PNG 自身左上角像素，`coordinate_space` 为 `"image"` —— 只有它恰好是 `(0, 0)`
显示器的全屏截图时才可直接点击。`path` 与 `region` 同时传入会被拒绝。

**示例 —— 截取区域并点击返回结果**
```json
{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"desktop_perceive","arguments":{"region":{"x":200,"y":100,"width":800,"height":600}}}}
```
**返回**
```json
{"elements":[{"id":0,"kind":"button","text":"OK","rect":{"x":210,"y":110,"w":40,"h":20},"center":{"x":230,"y":120},"confidence":0.9,"source":"both"}, ...],"count":42,"ocr_count":30,"yolo_count":12,"yolo_available":true,"models_dir":"C:\\Users\\me\\AppData\\Roaming\\Nuphus\\models","coordinate_space":"screen","geometry":{"x":200,"y":100,"width":800,"height":600}}
```
该元素在区域内的位置为 10,20，因此屏幕坐标 = `区域原点 + 图像内位置`。OCR 模型
缺失且下载失败时：`isError: true` + 明确错误与手动下载指引。

---

### desktop_mouse

鼠标操作：`click` / `double_click` / `hover` / `scroll` / `move` 需要
`(x, y)`。`position` 为只读：返回当前光标位置，不移动光标。

| 参数 | 类型 | 必填 | 默认 | 说明 |
|------|------|------|------|------|
| `action` | string | **是** | - | `click`、`double_click`、`hover`、`scroll`、`position`、`move` 之一 |
| `x` | integer | 否 | 0 | X 坐标 |
| `y` | integer | 否 | 0 | Y 坐标 |
| `button` | string | 否 | - | `click` 的按键：`left`、`right`、`middle` |
| `clicks` | integer | 否 | 1 | 点击次数（`click`） |
| `direction` | string | 否 | - | `scroll` 方向：`up`、`down` |
| `amount` | integer | 否 | 3 | `scroll` 滚动格数 |

**示例 — 点击**
```json
{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"desktop_mouse","arguments":{"action":"click","x":100,"y":200,"button":"left"}}}
```

**示例 — 读取光标位置（只读）**
```json
{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"desktop_mouse","arguments":{"action":"position"}}}
```
**返回** `{"x":512,"y":384}`

---

### desktop_mouse_drag

从起点坐标拖拽鼠标到终点坐标（验证码滑块、滑块验证等场景）。

| 参数 | 类型 | 必填 | 默认 | 说明 |
|------|------|------|------|------|
| `start_x` | integer | **是** | - | 起点 X 坐标 |
| `start_y` | integer | **是** | - | 起点 Y 坐标 |
| `end_x` | integer | **是** | - | 终点 X 坐标 |
| `end_y` | integer | **是** | - | 终点 Y 坐标 |

**示例**
```json
{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"desktop_mouse_drag","arguments":{"start_x":50,"start_y":300,"end_x":350,"end_y":300}}}
```

---

### desktop_input

向窗口输入文本（自动 UTF-8 编码），可选附带一个后续按键——原子操作。
普通文本直接输入；超过 500 字符用剪贴板。操作前需先通过
`desktop_window_activate` 激活目标窗口。

| 参数 | 类型 | 必填 | 默认 | 说明 |
|------|------|------|------|------|
| `mode` | string | **是** | - | `type`：输入文本；`hotkey`：仅按键 |
| `hwnd` | integer | **是** | - | 目标窗口句柄（来自 `desktop_windows_list`） |
| `text` | string | 否 | - | 要输入的文本（`mode=type` 必填） |
| `send` | string | 否 | `"enter"` | 输入后发送的按键：`"enter"`（默认）、`"ctrl+enter"`、`"tab"`、或 `"none"` 跳过 |
| `keys` | array<string> | 否 | - | 要按下的组合键（`mode=hotkey` 必填） |

**示例 — 输入文本并回车**
```json
{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"desktop_input","arguments":{"mode":"type","hwnd":123456,"text":"你好世界","send":"enter"}}}
```

**示例 — 快捷键**
```json
{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"desktop_input","arguments":{"mode":"hotkey","hwnd":123456,"keys":["ctrl","s"]}}}
```

---

### desktop_clipboard_clean

清空系统剪贴板。粘贴完敏感内容（密码/Token/验证码）后必须调用，防止残留泄漏。

⚠️ 仅用于清除，不要用于读取剪贴板内容。

| 参数 | 类型 | 必填 | 默认 | 说明 |
|------|------|------|------|------|
| （无） | | | | 无参数 |

**示例**
```json
{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"desktop_clipboard_clean","arguments":{}}}
```
**返回** `{"cleared":true}`

---

### desktop_clipboard_write

写入长文本（>500 字符）到剪贴板用于粘贴。普通文本用 `desktop_input` 直接输入，
无需剪贴板。粘贴后必须调用 `desktop_clipboard_clean` 清除残留。

⚠️ 禁止用于密码/敏感数据。

| 参数 | 类型 | 必填 | 默认 | 说明 |
|------|------|------|------|------|
| `text` | string | **是** | - | 要写入的文本 |

**示例**
```json
{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"desktop_clipboard_write","arguments":{"text":"<长文本>"}}}
```
**返回** `{"written_chars":1234}`

---

### desktop_targets_list

查询本机运行窗口与已登记应用，返回 `app_ref` / `window_ref` 与 `is_self`。请优先
依据用户任务选择目标：提交任务时前台窗口通常是 Nuphus 本身，**不代表**它就是任务
目标。

| 名称 | 类型 | 必填 | 默认值 | 说明 |
|------|------|------|--------|------|
| `query` | string | 否 | - | 可选应用名称或窗口标题过滤 |
| `cursor` | integer | 否 | `0` | 翻页：传入返回的 `next_cursor`，且保持相同 `query` |

**返回**有界目录页，含 `applications[].windows[].window_ref`、`next_cursor`
与 `is_self`。

**示例**
```json
{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"desktop_targets_list","arguments":{"query":"editor"}}}
```

---

### desktop_target_bind

绑定 `desktop_targets_list` 返回的应用/窗口，必要时启动。`auto`/`background`
不会主动激活已有窗口；`foreground` 可还原并激活。多个窗口时返回候选供选择。
**写类工具** —— 严格确认模式下需要 `"confirm": true`。

| 名称 | 类型 | 必填 | 默认值 | 说明 |
|------|------|------|--------|------|
| `app_ref` | string | **是** | - | `desktop_targets_list` 返回的应用引用 |
| `window_ref` | string | 否 | - | 多窗口时选择列表返回的某个引用 |
| `delivery_mode` | string | 否 | `foreground` | `foreground` / `auto` / `background` |
| `confirm` | boolean | 否 | - | 严格确认模式下必须为 `true` |

**返回**供后续 `desktop_semantic_observe` 使用的 `target_token`；应用有多个窗口时
返回候选列表。

**示例**
```json
{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"desktop_target_bind","arguments":{"app_ref":"app:1a2b3c","confirm":true}}}
```

---

### desktop_semantic_observe

读取绑定目标的 UI Automation / Accessibility 元素，返回完整 JSON 候选页。未传
`target_token` 时观察前台窗口。原生句柄、坐标与平台定位器都留在适配器内部，调用方
只看到不透明候选 ID。翻页时传同次 `observation_token` 与 `next_cursor`，不会重新
采集。详情与可保存的 `workflow_step` 请用 `desktop_semantic_candidate` 查询。

| 名称 | 类型 | 必填 | 默认值 | 说明 |
|------|------|------|--------|------|
| `goal` | string | 否 | - | 当前桌面任务；仅用于构造和说明有界候选 |
| `target_token` | string | 否 | - | `desktop_target_bind` 返回的任务目标令牌 |
| `scope` | string | 否 | `window` | `window` / `menu` |
| `subtree_id` | string | 否 | - | 本地观察返回的元素/区域 ID |
| `observation_token` | string | 否 | - | 翻页时传入；此时不改变目标或观察范围 |
| `cursor` | integer | 否 | `0` | 候选/区域分页位置 |
| `view` | string | 否 | `candidates` | `candidates` / `regions` |
| `tree_cursor` | string | 否 | - | `next_tree_cursor` 返回的续读令牌（读取原生树下一批） |
| `delivery_mode` | string | 否 | `foreground` | `foreground` / `auto` / `background` |

**返回**有界候选页：`observation_token`、`candidates[]`、`control_candidates[]`、
`regions[]`、`next_cursor`、`next_tree_cursor`、`tree_incomplete` 与观察身份信息。

**示例**
```json
{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"desktop_semantic_observe","arguments":{"goal":"打开设置"}}}
```

---

### desktop_semantic_candidate

只读查询同一次观察中某个候选的详情与可保存 `workflow_step`。**请保存返回的稳定
步骤，不要保存候选 ID 或 token。**

| 名称 | 类型 | 必填 | 默认值 | 说明 |
|------|------|------|--------|------|
| `observation_token` | string | **是** | - | 当前观察返回的令牌 |
| `candidate_id` | string | **是** | - | 当前观察返回的候选 ID |

**示例**
```json
{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"desktop_semantic_candidate","arguments":{"observation_token":"obs:1f2e...","candidate_id":"candidate-3"}}}
```

---

### desktop_semantic_execute

执行 `desktop_semantic_observe` 最近一次返回的某个 `candidate_id`，必须回传同次
`observation_token`。`SetValue` 候选可附带 `value`，该文本只交给本地执行器，不会
发送给任何模型。执行前会重新读取 UI 并拒绝过期动作。**不得传坐标、选择器或脚本。**
**写类工具** —— 严格确认模式下需要 `"confirm": true`。

| 名称 | 类型 | 必填 | 默认值 | 说明 |
|------|------|------|--------|------|
| `observation_token` | string | **是** | - | 最近一次语义观察返回的不可预测短期令牌 |
| `candidate_id` | string | **是** | - | 最近一次语义观察返回的候选动作 ID |
| `value` | string | 否 | - | 仅用于 `SetValue` 候选的本地文本（保持原始空白） |
| `delivery_mode` | string | 否 | 继承观察 | `foreground` / `auto` / `background` |
| `checked` | boolean | 否 | - | `set_checked` 的目标状态 |
| `direction` | string | 否 | - | `up` / `down` / `left` / `right` |
| `amount` | string | 否 | - | `small` / `page` |
| `confirm` | boolean | 否 | - | 严格确认模式下必须为 `true` |

**返回**`status`、`verification`、`dispatch_state`、`effect`、原生 `receipt`
与可保存的 `workflow_step`。

**示例**
```json
{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"desktop_semantic_execute","arguments":{"observation_token":"obs:1f2e...","candidate_id":"candidate-3","confirm":true}}}
```

---

### desktop_semantic_action

执行已保存工作流中的稳定语义动作。运行时重新绑定目标窗口并解析 `locator`，不依赖
临时 `observation_token`、`candidate_id`、坐标、选择器或脚本；文本与范围值通过
`value` 本地传入。**写类工具** —— 严格确认模式下需要 `"confirm": true`。

| 名称 | 类型 | 必填 | 默认值 | 说明 |
|------|------|------|--------|------|
| `locator` | object | **是** | - | 从 `workflow_step` 原样保存的稳定定位器（`app_id` 必填） |
| `action` | string | **是** | - | `invoke` / `toggle` / `set_checked` / `select` / `expand` / `collapse` / `focus` / `set_value` / `scroll` / `scroll_into_view` / `set_range_value` |
| `value` | string | 否 | - | 仅用于 `set_value` / `set_range_value`，只交给本地 UIA |
| `launch_ref` | string | 否 | - | 本地返回的稳定启动引用；不得自行编造 |
| `delivery_mode` | string | 否 | `foreground` | `foreground` / `auto` / `background` |
| `checked` | boolean | 否 | - | `set_checked` 的目标状态；已满足时不点击 |
| `direction` | string | 否 | - | `up` / `down` / `left` / `right` |
| `amount` | string | 否 | - | `small` / `page` |
| `completion_policy` | string | 否 | `auto` | `auto` / `verified` / `dispatched` |
| `expectation` | object | 否 | - | 后置条件 `{locator, condition, expected, value, timeout_ms, stable_samples}`；始终验证且不重发动作 |
| `confirm` | boolean | 否 | - | 严格确认模式下必须为 `true` |

**返回**`status`、`verification`、`dispatch_state`、`effect`、`receipt` 与
`business_goal_confirmed`。

**示例**
```json
{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"desktop_semantic_action","arguments":{"locator":{"app_id":"app:1a2b3c","role":"button","automation_id":"save"},"action":"invoke","confirm":true}}}
```

---

### desktop_verify_state

只读验证指定窗口/元素的后置条件，可有界等待。返回 `satisfied` / `unsatisfied` /
`unknown`。不会重发原动作、启动应用或抢前台。

| 名称 | 类型 | 必填 | 默认值 | 说明 |
|------|------|------|--------|------|
| `expectation` | object | **是** | - | `{locator, condition, expected, value, timeout_ms, stable_samples}` |
| `expectation.locator` | object | **是** | - | 稳定定位器（`app_id` 必填） |
| `expectation.condition` | string | **是** | - | `exists` / `absent` / `window_exists` / `window_absent` / `focused` / `checked` / `selected` / `expanded` / `value_equals` |
| `expectation.expected` | boolean | 否 | `true` | 布尔类条件的期望状态 |
| `expectation.value` | string | 否 | - | `value_equals` 的期望文本 |
| `expectation.timeout_ms` | integer | 否 | `5000` | 有界等待，上限 `300000` |
| `expectation.stable_samples` | integer | 否 | `2` | 连续稳定采样次数，`1`–`5` |

**返回**`status`（`satisfied` / `unsatisfied` / `unknown`）、`condition`、
`reason` 与观察到的版本号。

**示例**
```json
{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"desktop_verify_state","arguments":{"expectation":{"locator":{"app_id":"app:1a2b3c","role":"check_box","accessible_name":"启用同步"},"condition":"checked","expected":true}}}}
```

---

## 浏览器工具（23）

浏览器工具通过 CDP（`chromiumoxide`）操作 Chrome 实例。首次浏览器调用会启动
一个可见的 Chrome 窗口；`browser_close` 关闭并释放资源。CDP 操作有 15 秒超时
保护（`navigate`/`back`/`forward` 为 30 秒；`wait_for` 为 timeout + 5 秒）。

### browser_navigate

在浏览器中打开 URL。

| 参数 | 类型 | 必填 | 默认 | 说明 |
|------|------|------|------|------|
| `url` | string | **是** | - | 要导航到的 URL |

**示例**
```json
{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"browser_navigate","arguments":{"url":"https://example.com"}}}
```
**返回** 导航确认，随后带 `── Page state ──` 页面快照。

---

### browser_snapshot

使用 Chrome 无障碍树获取可见可交互元素的文本快照，输出
`@N [role] "name"` 格式（如 `@1 [button] "Submit"`）。AX 树不可用时回退到
DOM 遍历。`@N` 引用可用于 `browser_click` / `browser_type`。

| 参数 | 类型 | 必填 | 默认 | 说明 |
|------|------|------|------|------|
| `full` | boolean | 否 | `false` | 是否包含隐藏元素 |
| `selector` | string | 否 | - | 限定快照范围的 CSS 选择器（如 `'#quiz'`、`'.main-content'`）；只对该子树内元素编号 |

**示例**
```json
{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"browser_snapshot","arguments":{"selector":"#main"}}}
```

---

### browser_exec

在一次 CDP 往返中执行多步批量脚本。适合表单填写、多步点击工作流。
脚本使用 `window.__nuphus` 助手（别名为 `h`）：

- `h.click('@N' | 'selector')`
- `h.fill('@N' | 'selector', text)`
- `h.scroll(px)`
- `h.wait(ms)`
- `h.extract('selector')`
- `h.snapshot()`

| 参数 | 类型 | 必填 | 默认 | 说明 |
|------|------|------|------|------|
| `script` | string | **是** | - | 使用 `window.__nuphus` 助手的 JS 脚本（别名为 `h`） |

**示例**
```json
{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"browser_exec","arguments":{"script":"h.fill('@2', 'alice'); h.click('@5'); h.wait(800); h.snapshot();"}}}
```
**返回** 每步 `[{op, ref, success, detail}]`。

---

### browser_click

通过 CSS 选择器或快照中的引用 ID（如 `@1`、`@e0`、`'button'`）点击元素。
CSS 选择器路径会在点击前自动等待元素出现并可见（最多 5 秒）。

默认左键点击是 JS 合成事件（可靠、可穿透遮挡层），但**不产生**用户激活
（user activation）。传入 `trusted: true` 可改为在元素中心派发真实 CDP
鼠标事件（`isTrusted=true`）。右键和中键始终使用可信 CDP 事件。
右键和中键默认不生成点击后快照，避免瞬态上下文菜单在下一步前消失。

| 参数 | 类型 | 必填 | 默认 | 说明 |
|------|------|------|------|------|
| `selector` | string | `selector` 或 `ref` | - | CSS 选择器或引用 ID（如 `@1`、`@e0`、`'button'`） |
| `ref` | string | `selector` 或 `ref` | - | 快照引用 ID；`selector` 的别名 |
| `trusted` | boolean | 否 | `false` | 派发真实可信 CDP 鼠标事件（产生用户激活）替代 JS 点击。用于自动播放受限的媒体播放等手势受限场景。 |
| `button` | string | 否 | `left` | 鼠标键：`left`、`right` 或 `middle`；右键和中键始终可信。 |
| `snapshot` | boolean | 否 | 随按键而定 | 是否附带点击后快照；左键默认 `true`，右键/中键默认 `false`。 |

**示例**
```json
{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"browser_click","arguments":{"selector":"@1"}}}
```
```json
{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"browser_click","arguments":{"selector":"button.play","trusted":true}}}
```
```json
{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"browser_click","arguments":{"selector":".file-row","button":"right"}}}
```
**返回** 点击确认；启用 `snapshot` 时，随后附带 `── Page state ──` 页面快照。

---

### browser_type

通过 CSS 选择器或快照中的引用 ID 向输入框输入文本。CSS 选择器路径会在输入前
自动等待元素出现并可见（最多 5 秒）。

| 参数 | 类型 | 必填 | 默认 | 说明 |
|------|------|------|------|------|
| `selector` | string | `selector` 或 `ref` | - | 输入框的 CSS 选择器或引用 ID（如 `@1`、`@e0`） |
| `ref` | string | `selector` 或 `ref` | - | 快照引用 ID；`selector` 的别名 |
| `text` | string | **是** | - | 要输入的文本 |

**示例**
```json
{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"browser_type","arguments":{"selector":"@2","text":"alice@example.com"}}}
```

---

### browser_press

向当前聚焦的页面元素发送可信物理按键或组合键。先用 `browser_click` 或
`browser_type` 聚焦目标。它是终端和 canvas 应用的原生按键通道；与
`browser_evaluate` 不同，产生的键盘事件满足 `isTrusted: true`。

| 参数 | 类型 | 必填 | 默认 | 说明 |
|------|------|------|------|------|
| `key` | string | **是** | - | 命名键、单个美式键盘字符或组合键，如 `Enter`、`ArrowUp`、`Control+c`、`Shift+Tab`、`Meta+ArrowLeft` |
| `snapshot` | boolean | 否 | `false` | 是否附带按键后的页面快照 |

**示例——提交已输入浏览器终端的命令**
```json
{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"browser_press","arguments":{"key":"Enter"}}}
```

---

### browser_scroll

按 N 像素向上/向下滚动页面。

| 参数 | 类型 | 必填 | 默认 | 说明 |
|------|------|------|------|------|
| `direction` | string | **是** | - | 滚动方向：`up`、`down` |
| `amount` | integer | 否 | `500` | 滚动像素数 |

**示例**
```json
{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"browser_scroll","arguments":{"direction":"down","amount":800}}}
```

---

### browser_extract

提取当前页面可读文本（去除导航/广告）。

| 参数 | 类型 | 必填 | 默认 | 说明 |
|------|------|------|------|------|
| `max_chars` | integer | 否 | `8000` | 最大提取字符数 |

**示例**
```json
{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"browser_extract","arguments":{"max_chars":4000}}}
```

---

### browser_screenshot

截取当前浏览器页面。

| 参数 | 类型 | 必填 | 默认 | 说明 |
|------|------|------|------|------|
| `path` | string | **是** | - | 父目录已存在的 PNG 保存路径 |

**示例**
```json
{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"browser_screenshot","arguments":{"path":"page.png"}}}
```
**返回** 保存路径和 PNG 字节数。

---

### browser_close

关闭浏览器并释放资源。

| 参数 | 类型 | 必填 | 默认 | 说明 |
|------|------|------|------|------|
| （无） | | | | 无参数 |

**示例**
```json
{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"browser_close","arguments":{}}}
```
**返回** `Browser closed`

---

### browser_evaluate

在当前页面执行任意 JavaScript。

| 参数 | 类型 | 必填 | 默认 | 说明 |
|------|------|------|------|------|
| `script` | string | **是** | - | JavaScript 代码 |

**示例**
```json
{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"browser_evaluate","arguments":{"script":"document.title"}}}
```

---

### browser_back

后退到浏览器历史上一页。

| 参数 | 类型 | 必填 | 默认 | 说明 |
|------|------|------|------|------|
| （无） | | | | 无参数 |

**示例**
```json
{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"browser_back","arguments":{}}}
```

---

### browser_forward

前进到浏览器历史上下一页。

| 参数 | 类型 | 必填 | 默认 | 说明 |
|------|------|------|------|------|
| （无） | | | | 无参数 |

**示例**
```json
{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"browser_forward","arguments":{}}}
```

---

### browser_wait_for

等待 CSS 选择器达到指定状态（最长 timeout）。注意：`browser_click` /
`browser_type` 的 CSS 路径已自动等待（出现+可见，5 秒），因此显式等待通常只在
需要自定义状态或更长延迟时使用。

| 参数 | 类型 | 必填 | 默认 | 说明 |
|------|------|------|------|------|
| `selector` | string | **是** | - | 要等待的 CSS 选择器 |
| `timeout_ms` | integer | 否 | `5000` | 最大等待毫秒数 |
| `state` | string | 否 | `"attached"` | 目标状态：`attached`、`visible`、`hidden` |

**示例**
```json
{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"browser_wait_for","arguments":{"selector":".result","state":"visible","timeout_ms":10000}}}
```

---

### browser_cookies_get

获取当前页面的全部 Cookie。

| 参数 | 类型 | 必填 | 默认 | 说明 |
|------|------|------|------|------|
| （无） | | | | 无参数 |

**示例**
```json
{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"browser_cookies_get","arguments":{}}}
```

---

### browser_cookies_set

为当前域名设置 Cookie。

| 参数 | 类型 | 必填 | 默认 | 说明 |
|------|------|------|------|------|
| `name` | string | **是** | - | Cookie 名称 |
| `value` | string | **是** | - | Cookie 值 |
| `domain` | string | 否 | 当前域名 | Cookie 域名 |
| `path` | string | 否 | - | Cookie 路径 |

**示例**
```json
{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"browser_cookies_set","arguments":{"name":"theme","value":"dark"}}}
```

---

### browser_import_cookies

从用户 Chrome 配置导入 Cookie 到当前浏览器会话。需要宿主环境（Nuphus）注册
Cookie 数据源；裸装 `nuphus-mcp` 时可能不可用，会返回说明性错误。

| 参数 | 类型 | 必填 | 默认 | 说明 |
|------|------|------|------|------|
| `domain` | string | 否 | - | 可选域名过滤 |

**示例**
```json
{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"browser_import_cookies","arguments":{"domain":"example.com"}}}
```

---

### browser_upload

使用 DataTransfer 技巧向文件输入元素上传文件。文件必须真实存在于磁盘。

| 参数 | 类型 | 必填 | 默认 | 说明 |
|------|------|------|------|------|
| `selector` | string | **是** | - | 文件输入的 CSS 选择器或 `@N` 引用 |
| `file_path` | string | **是** | - | 要上传文件的绝对路径 |

**示例**
```json
{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"browser_upload","arguments":{"selector":"input[type=file]","file_path":"C:/Users/me/Desktop/report.pdf"}}}
```

---

### browser_drag_files

通过 Chrome DevTools 原生拖放事件，把一个或多个本地文件或目录拖到任意浏览器
元素上。不要求页面存在 `input[type=file]`，不对文件内容做 Base64 编码，因此
不受 MCP 请求行大小限制。

| 参数 | 类型 | 必填 | 默认 | 说明 |
|------|------|------|------|------|
| `selector` | string | `selector` 或 `ref` | - | 放置目标的 CSS 选择器或快照引用 |
| `ref` | string | `selector` 或 `ref` | - | 快照引用 ID；`selector` 的别名 |
| `file_paths` | string[] | **是** | - | 真实存在的本地文件或目录绝对路径 |

**示例**
```json
{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"browser_drag_files","arguments":{"selector":".explorer-viewlet","file_paths":["/Users/me/report.pdf","/Users/me/assets"]}}}
```

---

### browser_list_downloads

列出浏览器下载目录中的文件。

| 参数 | 类型 | 必填 | 默认 | 说明 |
|------|------|------|------|------|
| （无） | | | | 无参数 |

**示例**
```json
{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"browser_list_downloads","arguments":{}}}
```

---

### browser_new_tab

打开新的浏览器标签页。

| 参数 | 类型 | 必填 | 默认 | 说明 |
|------|------|------|------|------|
| `url` | string | 否 | - | 新标签页要打开的 URL |

**示例**
```json
{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"browser_new_tab","arguments":{"url":"https://example.com"}}}
```

---

### browser_list_tabs

列出所有打开的标签页（索引、URL、标题）。索引反映当前标签页顺序，新增或关闭
标签页后可能变化。

| 参数 | 类型 | 必填 | 默认 | 说明 |
|------|------|------|------|------|
| （无） | | | | 无参数 |

**示例**
```json
{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"browser_list_tabs","arguments":{}}}
```

---

### browser_switch_tab

按索引切换标签页，并在可见的 Chrome 窗口中将其真正置前。

| 参数 | 类型 | 必填 | 默认 | 说明 |
|------|------|------|------|------|
| `index` | integer | **是** | - | 来自 `browser_list_tabs` 的标签页索引 |

**示例**
```json
{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"browser_switch_tab","arguments":{"index":1}}}
```

---

## 端到端示例

一个完整的 stdio 会话：initialize → tools/list → 屏幕截图 → 打开页面 → 填写并提交表单。

```
→ {"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"my-agent","version":"1.0.0"}}}
← {"jsonrpc":"2.0","id":0,"result":{"protocolVersion":"2024-11-05","capabilities":{"tools":{"listChanged":false}},"serverInfo":{"name":"nuphus-mcp","version":"0.1.0"},...}}

→ {"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}
← {"jsonrpc":"2.0","id":1,"result":{"tools":[ ... 38 个工具 ... ]}}

→ {"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"desktop_screenshot","arguments":{}}}
← {"jsonrpc":"2.0","id":2,"result":{"content":[{"type":"text","text":"{\"format\":\"png\",\"data\":\"...\",\"width\":1920,\"height\":1080}"}]}}

→ {"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"browser_navigate","arguments":{"url":"https://example.com"}}}
← {"jsonrpc":"2.0","id":3,"result":{"content":[{"type":"text","text":"Navigated to: https://example.com\n\n── Page state ──\n@1 [link] \"More information\"..."}]}}

→ {"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"browser_exec","arguments":{"script":"h.click('@1'); h.wait(500); h.snapshot();"}}}
← {"jsonrpc":"2.0","id":4,"result":{"content":[{"type":"text","text":"[{\"op\":\"click\",\"ref\":\"@1\",\"success\":true,\"detail\":\"...\"},...]"}]}}
```

---

*依据当前 `nuphus-mcp` 构建的 `tools/list` schema 生成。本版本仅暴露以上
38 个工具。*
