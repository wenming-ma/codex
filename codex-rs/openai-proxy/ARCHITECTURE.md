# Codex OpenAI Proxy 架构文档

## 📋 核心需求（CRITICAL - 必须理解）

**我们的真正需求是什么？**

```
Cursor IDE
    ↓ 发送 OpenAI API 请求
codex-openai-proxy（我们的代理）
    ↓ 转换格式 + 使用 Codex 登录的 ChatGPT Plus 账号
Codex CLI（纯转发器，不执行任何工具）
    ↓ 使用已登录的账号
OpenAI 官方 API Server
    ↓ 返回 LLM 响应（可能包含 tool_calls）
Codex CLI
    ↓ 转发原始响应
codex-openai-proxy
    ↓ 适配成 OpenAI API 格式
Cursor IDE
    ↓ 在本地执行工具（如果有 tool_calls）
    ↓ 将工具结果作为下一条消息发送回代理
```

## ⚠️ 关键理解（之前搞错的地方）

### ❌ 错误理解（导致无限循环）

之前我们误以为需要 Codex **作为 Agent 执行工具**：

```rust
// ❌ 错误的方式
sandbox_policy: SandboxPolicy::Unrestricted,  // 允许 Codex 执行工具
approval_policy: AskForApproval::Never,       // 自动执行

// 结果：
// Codex 自己执行工具 → 工具失败 → Codex 重试 → 无限循环
// 20+ 次 tool_calls，从不返回最终答案
```

### ✅ 正确理解（纯转发）

我们实际需要的是 Codex **纯转发 API**：

```rust
// ✅ 正确的方式
sandbox_policy: SandboxPolicy::ReadOnly,  // Codex 不执行工具，只转发
approval_policy: AskForApproval::Never,   // 不需要审批

// 结果：
// Codex 只做转发 → OpenAI 返回 tool_calls → 转发给 Cursor
// Cursor 本地执行工具 → 将结果发回
```

## 🏗️ 架构设计

### 1. 为什么使用 Codex？

**Codex 的唯一作用：使用已登录的 ChatGPT Plus 账号访问 OpenAI API**

- Codex CLI 通过 `codex login` 已经登录了 ChatGPT Plus
- 它可以使用这个登录状态访问 OpenAI API
- 我们的代理利用 Codex 的认证，避免直接管理 API keys

### 2. 为什么使用 ThreadManager 而不是 ModelClient？

**最初尝试了 ModelClient，但遇到编译问题：**

```rust
// 尝试使用 ModelClient（失败）
use codex_core::ModelClient;
use codex_core::client_common::Prompt;  // ❌ private module
use codex_otel::OtelManager;            // ❌ unresolved import
use codex_core::model_provider_info::ModelProviderInfo;  // ❌ private module
```

**改用 ThreadManager（成功）：**

```rust
// ThreadManager 是公开 API
use codex_core::ThreadManager;
use codex_core::CodexThread;
```

### 3. ReadOnly 的作用

```rust
sandbox_policy: SandboxPolicy::ReadOnly,
```

**ReadOnly 确保：**
- Codex 不会在服务器端执行工具
- 只转发 OpenAI API 的原始响应
- 如果 API 返回 tool_calls，直接转发给 Cursor
- Cursor 在本地（用户的机器）执行工具

**对比：**
- `Unrestricted` - Codex 自己执行工具（导致无限循环）
- `ReadOnly` - Codex 只转发，不执行

## 📝 核心代码说明

### 关键配置

```rust
// 在 get_or_create_thread 和 Submission 中都要设置
let overrides = vec![
    ("model".to_string(), toml::Value::String(map_model(model))),
    ("approval_policy".to_string(), toml::Value::String("never".to_string())),
    ("sandbox_mode".to_string(), toml::Value::String("read-only".to_string())),  // ⚠️ 关键
];

let submission = Submission {
    op: Op::UserTurn {
        approval_policy: AskForApproval::Never,
        sandbox_policy: SandboxPolicy::ReadOnly,  // ⚠️ 关键
        // ...
    },
};
```

### 模型名称反转

```rust
fn map_model(model: &str) -> String {
    // Cursor 使用反转的模型名
    // 例如：Cursor 发送 "2.5-tpg"
    // 我们反转成 "gpt-5.2" 发送给 Codex
    model.chars().rev().collect()
}
```

**为什么反转？**
- Cursor 允许自定义模型名
- 用户在 Cursor 中输入反转的名字（如 "xedoc-2.5-tpg"）
- 代理反转回正常名字（"gpt-5.2-codex"）发给 Codex

### 响应格式要点

```rust
let resp = ChatCompletionResponse {
    model: original_model.clone(),  // ⚠️ 使用原始模型名，不要用反转后的
    choices: vec![ChatChoice {
        message: ChatMessageResponse {
            content: final_text,
            tool_calls: if tool_calls.is_empty() { None } else { Some(tool_calls) },
        },
        finish_reason: if !tool_calls.is_empty() {
            "tool_calls".to_string()  // ⚠️ 有工具调用时必须是 "tool_calls"
        } else {
            "stop".to_string()
        },
    }],
};
```

## 🐛 常见错误及解决方案

### 错误 1: 内容重复发送

**问题：**
```rust
EventMsg::TurnComplete(done) => {
    if let Some(msg) = done.last_agent_message {
        let chunk = stream_chunk(Some(&msg), ...);  // ❌ 重复发送
        tx.send(Ok(chunk)).await;
    }
}
```

**原因：**
- 内容已经通过 `AgentMessageDelta` 发送过了
- 在 TurnComplete 再次发送导致 Cursor 检测到 "looping"

**解决：**
```rust
EventMsg::TurnComplete(_done) => {
    // ⚠️ 不要再发送 last_agent_message
    // 只发送 finish_reason
    let chunk = stream_chunk_with_finish(None, None, finish_reason, model);
    tx.send(Ok(chunk)).await;
}
```

### 错误 2: 缺少 model 字段

**问题：**
```
Cursor 显示 "Connection Error" 但返回 200
```

**原因：**
响应的 chunk 中缺少 `model` 字段

**解决：**
```rust
serde_json::json!({
    "model": model,  // ⚠️ 每个 chunk 都必须包含
    "choices": [...]
})
```

### 错误 3: 无限工具调用循环

**问题：**
```
Codex 连续 20+ 次调用工具，从不返回最终答案
```

**原因：**
```rust
sandbox_policy: SandboxPolicy::Unrestricted  // ❌ 错误
```

**解决：**
```rust
sandbox_policy: SandboxPolicy::ReadOnly  // ✅ 正确
```

## 📊 积累的经验教训

### ✅ 必须保留的特性

1. **双路由支持** - `/v1/*` 和 `/*` 都要支持（Cursor 兼容性）
2. **模型名称反转** - `map_model()` 函数
3. **保留原始模型名** - 响应中使用 `original_model`，不用反转后的
4. **不重复发送内容** - TurnComplete 时只发送 `finish_reason`
5. **model 字段必须包含** - 每个 streaming chunk 都要有
6. **CORS 完整支持** - `allow_origin(Any)`
7. **日志系统** - `LOG_CHANNEL` + `/logs` 端点
8. **Reasoning 检测** - 记录 reasoning items
9. **conversation_id** - 支持持久化对话
10. **ReadOnly 沙箱** - 确保 Codex 不执行工具

### ❌ 常见陷阱

1. **不要使用 Unrestricted** - 会导致工具执行循环
2. **不要重复发送内容** - 会被 Cursor 检测为 looping
3. **不要忘记 model 字段** - 会导致连接错误
4. **不要混淆模型名** - 响应要用原始名，不要用反转名
5. **不要尝试使用 ModelClient** - API 不够稳定，用 ThreadManager

## 🧪 测试要点

### 1. 基本测试
```bash
# 启动代理
cargo run --release

# 在 Cursor 中配置
# Base URL: https://codex.wenming-dev.org
# Model: xedoc-2.5-tpg
```

### 2. 检查要点

**日志检查（http://127.0.0.1:11435/logs）：**
```json
{"type": "incoming_request", "model": "xedoc-2.5-tpg"}
{"type": "codex_forward", "mapped_model": "gpt-5.2-codex"}
{"type": "reasoning_detected", "summary_count": 1}
{"type": "cursor_response", "finish_reason": "stop"}
```

**不应该看到：**
```json
{"type": "tool_call_forwarded"}  // 除非 LLM 真的返回了 tool_calls
{"type": "stream_codex_error"}   // 不应该有错误
```

### 3. 工具调用测试

**正确的流程：**
```
1. Cursor 发送请求 "列出当前目录的文件"
2. Proxy 转发给 Codex → OpenAI
3. OpenAI 返回 tool_calls: [{"name": "list_dir", ...}]
4. Proxy 转发给 Cursor
5. Cursor 本地执行 list_dir
6. Cursor 将结果发送回 Proxy
7. 循环直到 LLM 返回最终答案
```

**不应该看到：**
```
- 20+ 次连续 tool_calls
- Codex 自己执行工具
- 工具执行失败但一直重试
```

## 📂 文件结构

```
codex-rs/openai-proxy/
├── src/
│   ├── main.rs                      # 主代理逻辑（⚠️ 使用 ReadOnly）
│   ├── main_threadmanager_backup.rs # 旧版本备份
│   └── main_modelclient.rs          # ModelClient 尝试（编译失败）
├── static/
│   ├── logs.html                    # 日志查看器
│   ├── logs.css
│   └── logs.js
├── Cargo.toml
├── STATUS.md                        # 状态文档
├── REASONING.md                     # Reasoning 支持分析
└── ARCHITECTURE.md                  # 本文件
```

## 🚀 部署配置

### Cloudflare Tunnel 配置

**C:\Users\wenming\.cloudflared\config.yaml:**
```yaml
tunnel: <tunnel-id>
credentials-file: C:\Users\wenming\.cloudflared\<tunnel-id>.json

ingress:
  - hostname: codex.wenming-dev.org
    service: http://localhost:11435
    originRequest:
      protocol: http2  # ⚠️ 强制 HTTP/2，不用 QUIC（网络限制）
  - service: http_status:404
```

### 环境变量

```bash
# 可选：自定义监听地址
export CODEX_OPENAI_PROXY_ADDR=127.0.0.1:11435
```

### Codex 配置

**C:\Users\wenming\.codex\config.toml:**
```toml
model = "gpt-5.2-codex"
model_reasoning_effort = "high"
model_verbosity = "medium"  # ⚠️ 必须设置，默认 "low" 不支持
```

## 🔄 工作流程总结

### 请求流程
```
1. Cursor 发送 ChatCompletionRequest
   - model: "xedoc-2.5-tpg"
   - messages: [{"role": "user", "content": "..."}]

2. Proxy 处理
   - 反转模型名: "xedoc-2.5-tpg" → "gpt-5.2-codex"
   - 创建 Submission (ReadOnly)
   - 提交给 CodexThread

3. Codex 转发
   - 使用登录的 ChatGPT Plus 账号
   - 调用 OpenAI API
   - 获取响应流

4. Proxy 适配
   - 收集 ResponseItems
   - 转换成 OpenAI 格式
   - 检测 tool_calls
   - 转发给 Cursor

5. Cursor 处理
   - 如果有 tool_calls，在本地执行
   - 将结果作为下一条消息发送
   - 如果没有 tool_calls，显示最终答案
```

## 📞 联系与维护

**重要提醒：**
- 这个代理是**纯转发**模式
- Codex 不执行任何工具
- 所有工具都在 Cursor 本地执行
- 如果看到工具执行循环，检查 `SandboxPolicy`

**关键配置必须是：**
```rust
sandbox_policy: SandboxPolicy::ReadOnly,
sandbox_mode: "read-only",
```

**绝对不要改成：**
```rust
sandbox_policy: SandboxPolicy::Unrestricted,  // ❌ 会导致循环
```
