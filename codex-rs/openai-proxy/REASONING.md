# OpenAI API Reasoning 支持分析

## 概述

OpenAI API 在两个 API 中支持 reasoning（推理）功能：
1. **Responses API** - 完整支持 reasoning 流式传输
2. **Chat Completions API** - 仅在 usage 中报告 reasoning_tokens

## Responses API 中的 Reasoning

### Reasoning 配置

```json
{
  "model": "o3-mini",
  "input": "问题",
  "reasoning": {
    "effort": "high",      // low, medium, high
    "summary": "detailed"  // auto, concise, detailed
  }
}
```

### 流式事件

Responses API 通过以下 SSE 事件传递 reasoning 内容：

#### 1. `response.reasoning_summary_part.added`
开始一个新的 reasoning summary 部分
```json
{
  "type": "response.reasoning_summary_part.added",
  "item_id": "rs_xxx",
  "output_index": 0,
  "summary_index": 0,
  "part": {
    "type": "summary_text",
    "text": ""
  }
}
```

#### 2. `response.reasoning_summary_text.delta`
流式传递 reasoning 文本
```json
{
  "type": "response.reasoning_summary_text.delta",
  "item_id": "rs_xxx",
  "output_index": 0,
  "summary_index": 0,
  "delta": "**Respond"
}
```

#### 3. `response.reasoning_summary_text.done`
Reasoning 文本完成
```json
{
  "type": "response.reasoning_summary_text.done",
  "item_id": "rs_xxx",
  "output_index": 0,
  "summary_index": 0,
  "text": "完整的推理过程..."
}
```

### ReasoningItem 结构

```json
{
  "type": "reasoning",
  "id": "rs_xxx",
  "summary": [
    {
      "type": "summary_text",
      "text": "推理摘要内容..."
    }
  ]
}
```

## Chat Completions API 中的 Reasoning

### 限制

Chat Completions API **不支持流式传输 reasoning 内容**，仅在以下位置包含 reasoning 信息：

1. **Usage 统计**
```json
{
  "usage": {
    "prompt_tokens": 82,
    "completion_tokens": 17,
    "total_tokens": 99,
    "completion_tokens_details": {
      "reasoning_tokens": 832,  // ⚠️ 仅统计，无内容
      "audio_tokens": 0
    }
  }
}
```

2. **流式响应中的 delta**
标准的 `ChatCompletionStreamResponseDelta` **不包含 reasoning 字段**：
```typescript
{
  content?: string;
  tool_calls?: ToolCallChunk[];
  role?: "assistant";
  refusal?: string;
  // ❌ 无 reasoning 字段
}
```

## Cursor 的情况

### 当前状态
- Cursor 使用 **Chat Completions API**，不是 Responses API
- Chat Completions API **不支持**在流式响应中传递 reasoning 内容
- 只能在 `usage.completion_tokens_details.reasoning_tokens` 中看到 token 统计

### 可能的解决方案

#### 方案 1：使用 Responses API（推荐）
如果 Cursor 支持 Responses API，可以完整获取 reasoning：
- ✅ 完整的 reasoning 流式传输
- ✅ 支持 reasoning effort 配置
- ✅ 支持 reasoning summary 设置
- ❌ 需要 Cursor 支持 Responses API 格式

#### 方案 2：在 Chat Completions API 中模拟
在我们的代理中将 Codex 的 reasoning 添加到响应：

**选项 A：添加到 content**
```json
{
  "delta": {
    "content": "[Reasoning]: 推理过程...\n\n实际回答..."
  }
}
```
- ✅ Cursor 可以显示
- ❌ 混合在正常回答中，不易区分
- ❌ 不符合 OpenAI 规范

**选项 B：使用非标准字段**
```json
{
  "delta": {
    "content": "实际回答...",
    "reasoning": "推理过程..."  // ⚠️ 非标准字段
  }
}
```
- ✅ 内容分离
- ❌ Cursor 可能忽略此字段
- ❌ 不符合 OpenAI 规范

**选项 C：仅在 usage 中报告**
```json
{
  "usage": {
    "completion_tokens_details": {
      "reasoning_tokens": 832
    }
  }
}
```
- ✅ 符合 OpenAI 规范
- ❌ 无法看到实际 reasoning 内容

## Codex 的 Reasoning 支持

### Codex 配置
```toml
model_reasoning_effort = "high"
model_verbosity = "medium"
```

### Codex 输出
Codex 可能会输出 reasoning content，但需要检查：
1. 是否在 `ResponseItem` 中包含 reasoning
2. 是否在 usage 中报告 reasoning_tokens

## 建议

### 短期方案
1. **检查 Cursor 支持**
   - 测试 Cursor 是否识别 Responses API 格式
   - 测试 Cursor 是否显示非标准 `reasoning` 字段

2. **添加 reasoning_tokens 统计**
   - 在 `usage.completion_tokens_details` 中添加 `reasoning_tokens`
   - 从 Codex 的 usage 信息中获取

### 长期方案
1. **同时支持两个 API**
   - 保持 `/v1/chat/completions` 用于 Chat Completions API
   - 添加 `/v1/responses` 用于 Responses API
   - 根据请求路由到不同的处理逻辑

2. **配置选项**
   - 添加配置选择是否在 content 中包含 reasoning
   - 允许用户选择 reasoning 显示方式

## 测试计划

1. **检查 Codex 输出**
   ```bash
   # 使用 gpt-5.2-codex 测试
   # 查看 ResponseItem 是否包含 ReasoningItem
   ```

2. **测试 Cursor 兼容性**
   ```bash
   # 发送带 reasoning 字段的响应
   # 观察 Cursor 是否显示
   ```

3. **比较 API 格式**
   ```bash
   # Chat Completions vs Responses API
   # 流式响应格式差异
   ```

## 参考链接

- OpenAI Reasoning Guide: https://platform.openai.com/docs/guides/reasoning
- Responses API: https://platform.openai.com/docs/api-reference/responses
- OpenAPI Spec: openai-openapi/openapi.yaml
