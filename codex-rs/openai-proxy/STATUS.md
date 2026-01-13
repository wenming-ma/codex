# Codex OpenAI Proxy - Status

## 概述

OpenAI API 兼容的代理服务，用于将 Cursor IDE 的请求转发到 Codex。

## 架构

```
Cursor IDE (OpenAI API 客户端)
    ↓ HTTPS (通过 Cloudflare Tunnel)
codex-openai-proxy (Rust/Axum)
    ↓ 模型名反转 + 格式转换
Codex ThreadManager/API
    ↓ 实际 API 调用
OpenAI API
```

## 网络配置

### Cloudflare Tunnel

**配置文件：** `C:\Users\wenming\.cloudflared\config.yaml`

```yaml
tunnel: 728005cd-a2d5-40a7-9fde-d58f97f81eb6
credentials-file: "C:\\Users\\wenming\\.cloudflared\\728005cd-a2d5-40a7-9fde-d58f97f81eb6.json"
protocol: http2  # 强制使用 TCP/443 而非 QUIC/UDP

ingress:
  - hostname: codex.wenming-dev.org
    service: http://127.0.0.1:11435
  - service: http_status:404
```

**启动命令：**
```powershell
cloudflared tunnel run codex
```

**公网 URL：** `https://codex.wenming-dev.org`

**为什么使用 HTTP/2：**
- 某些网络环境（如公司网络）会阻止 QUIC (UDP/443)
- HTTP/2 over TCP/443 兼容性更好

## 模型映射规则

### 反转映射机制

为避免 Cursor 模型名冲突，使用**字符串反转**作为映射规则：

| Cursor 中使用的名称 | 反转后发送给 Codex | Codex 实际模型 |
|---|---|---|
| `xedoc-2.5-tpg` | `gpt-5.2-codex` | GPT-5.2 Codex (默认) |
| `xam-xedoc-1.5-tpg` | `gpt-5.1-codex-max` | GPT-5.1 Codex Max |
| `inim-xedoc-1.5-tpg` | `gpt-5.1-codex-mini` | GPT-5.1 Codex Mini |
| `2.5-tpg` | `gpt-5.2` | GPT-5.2 通用模型 |

**代码实现：**
```rust
fn map_model(model: &str) -> String {
    // Reverse the model name string
    // Cursor uses reversed model names (e.g., "2.5-tpg" -> "gpt-5.2")
    model.chars().rev().collect()
}
```

## API 端点

### 支持的端点

#### 1. `/models` 和 `/v1/models`
**方法：** GET

**响应：**
```json
{
  "object": "list",
  "data": [
    {"id": "xedoc-2.5-tpg", "object": "model", "owned_by": "codex"},
    {"id": "xam-xedoc-1.5-tpg", "object": "model", "owned_by": "codex"},
    {"id": "inim-xedoc-1.5-tpg", "object": "model", "owned_by": "codex"},
    {"id": "2.5-tpg", "object": "model", "owned_by": "codex"}
  ]
}
```

#### 2. `/chat/completions` 和 `/v1/chat/completions`
**方法：** POST

**请求格式：** OpenAI Chat Completion API
**响应格式：** OpenAI Chat Completion API

**关键特性：**
- ✅ 流式和非流式响应
- ✅ CORS 支持
- ✅ 返回原始请求的模型名（而非内部转换后的名称）
- ✅ 必需的 `usage` 字段（包含 token 统计）

**响应示例：**
```json
{
  "id": "chatcmpl-codex-{uuid}",
  "object": "chat.completion",
  "created": 1768247311,
  "model": "2.5-tpg",
  "choices": [{
    "index": 0,
    "message": {
      "role": "assistant",
      "content": "...",
      "tool_calls": null
    },
    "finish_reason": "stop"
  }],
  "usage": {
    "prompt_tokens": 0,
    "completion_tokens": 0,
    "total_tokens": 0
  }
}
```

#### 3. `/responses` 和 `/v1/responses`
**方法：** POST

**用途：** Codex Responses API 格式（用于 Codex 原生集成）

## CORS 配置

**策略：** 允许所有来源

```rust
let cors = CorsLayer::new()
    .allow_origin(Any)
    .allow_methods(Any)
    .allow_headers(Any);
```

**为什么需要 CORS：**
- Cursor IDE 通过 Web 技术实现，需要 CORS 预检请求
- OPTIONS 请求必须返回正确的 CORS 头

## Cursor IDE 配置

### 设置步骤

1. **打开 Cursor 设置**
2. **找到 "Override OpenAI Base URL"**
3. **设置为：** `https://codex.wenming-dev.org`
   - ⚠️ 注意：不要加 `/v1` 后缀
   - ⚠️ 注意：不要有尾部斜杠 `/`
4. **API Key：** 可以填任意值（如 `sk-test`）
   - 我们的代理不验证 API Key
   - Codex 使用已登录的用户凭证

### 使用模型

在 Cursor 中选择模型时，使用反转后的名称：
- `xedoc-2.5-tpg` - 默认推荐
- `2.5-tpg` - 快速简单任务

## 运行服务

### 本地开发

```powershell
# 编译并运行
cargo run -p codex-openai-proxy

# 监听地址
# 默认: 0.0.0.0:11435
# 可通过环境变量覆盖: CODEX_OPENAI_PROXY_ADDR
```

### 生产环境

```powershell
# 1. 启动代理服务
cargo run -p codex-openai-proxy --release

# 2. 启动 Cloudflare Tunnel (另一个终端)
cloudflared tunnel run codex
```

## 已知限制

1. **OpenAI 配额限制**
   - Codex 使用用户的 OpenAI 账户
   - ChatGPT Plus 有每日使用限制
   - 429 错误表示配额耗尽，需等待重置或升级到 Pro

2. **PowerShell Shell Snapshot**
   - Codex 暂不支持 PowerShell 的 shell snapshot
   - 不影响核心功能

3. **Token 统计**
   - 当前 `usage` 字段返回 0
   - 实际 token 统计由 Codex 内部处理

## 故障排查

### Cursor 连接失败

**症状：** "Connection failed. If the problem persists, please check your internet connection or VPN"

**检查清单：**
1. ✅ Cloudflare Tunnel 是否运行？
   ```powershell
   # 检查日志中是否有 "Registered tunnel connection"
   ```

2. ✅ 代理服务是否运行？
   ```powershell
   netstat -ano | findstr 11435
   ```

3. ✅ 从外网测试连接：
   ```powershell
   curl https://codex.wenming-dev.org/models
   ```

4. ✅ Cursor Base URL 配置是否正确？
   - 应该是：`https://codex.wenming-dev.org`
   - 不是：`https://codex.wenming-dev.org/v1`

### 502 Bad Gateway

**原因：** Tunnel 正常但代理服务未运行

**解决：** 启动 `cargo run -p codex-openai-proxy`

### 429 Too Many Requests

**原因：** OpenAI 账户配额用尽

**解决：**
- 等待配额重置（通常每天重置）
- 升级到 ChatGPT Pro
- 购买更多积分

## 技术栈

- **语言：** Rust
- **Web 框架：** Axum 0.8
- **异步运行时：** Tokio
- **HTTP 客户端：** Reqwest
- **CORS：** tower-http
- **隧道：** Cloudflare Tunnel (cloudflared)

## 开发历史

### 完成的功能

- ✅ OpenAI Chat Completions API 兼容
- ✅ Cloudflare Tunnel 集成（HTTP/2）
- ✅ CORS 支持
- ✅ 流式和非流式响应
- ✅ 模型名反转映射
- ✅ 双路由（`/v1/*` 和 `/*`）
- ✅ 符合 OpenAI API 规范的响应格式

### 参考项目

在实现过程中参考了以下开源项目：
- [cursor-deepseek](https://github.com/danilofalcao/cursor-deepseek) - Go 实现的 DeepSeek 代理
- [cursor_ollama_proxy](https://github.com/punnerud/cursor_ollama_proxy) - Python 实现的 Ollama 代理

## 版本信息

- **Codex OpenAI Proxy：** v0.0.0
- **Codex CLI：** v0.77.0
- **Cloudflare Tunnel：** cloudflared 2025.8.1

---

**最后更新：** 2026-01-13
**维护者：** @wenming
