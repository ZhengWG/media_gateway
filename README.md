# media_gateway

面向 vLLM / SGLang 的多模态前处理独立模块（HTTP-first）。

## 1. 当前目标与状态

本项目当前处于“优先打通 HTTP 全链路”的阶段：

- 对外提供 OpenAI 兼容入口（重点：`/v1/chat/completions`）。
- 在网关层完成媒体加载与轻量前处理。
- 处理后仍回写 OpenAI 兼容形状（`image_url/video_url/audio_url` 的 data URL）。
- 在 proxy 模式下转发到 SGLang `/v1/chat/completions`。

## 2. 与 SGLang `/v1/chat/completions` 的兼容策略

### 2.1 已适配原则

- **字段透明透传**：除多模态媒体 `url` 被替换为 data URL 外，其余请求字段保持原样转发到 SGLang。
- **多模态 shape 兼容**：
  - `type=text`
  - `type=image_url` + `image_url.url`
  - `type=video_url` + `video_url.url`
  - `type=audio_url` + `audio_url.url`（兼容 SGLang）
  - 同时兼容历史 `audio` + `audio.url` 形状
- **skip 协议**：追加 `x-mm-preprocessed: 1`，并在 body 写入 `mm_preprocessed=true`。

### 2.2 仍需注意

- 当前网关聚焦 `chat/completions`，并不声明覆盖 SGLang 的所有其它 API（如 embeddings/rerank 等）。
- “完全一致”还依赖你部署侧 SGLang 版本与具体模型对多模态/processor_output 的支持。

## 3. 媒体 load 与前处理能力

### 3.1 支持的媒体来源

- `data:*;base64,...`
- `http(s)://...`
- 本地文件路径：
  - `file:///path/to/file`
  - `/abs/path`
  - `./relative/path`
  - `../relative/path`

### 3.2 轻量前处理（当前）

- 图片：按模型 profile 的目标边长缩放，并编码为 `image/jpeg`。
- 视频/音频：首期透传（只做加载与统一回写，不做重解码采样）。

## 4. HF Processor 接入模式

当前提供了 **HF Sidecar 预留接入位**（`hf_sidecar` 模块）：

- 用于将重前处理逻辑迁移到 Python（AutoProcessor）侧实现。
- 网关保持 Rust 控制面与协议编排。
- 当 sidecar 可用时，网关可将 payload 发送给 sidecar，接收处理后的 payload 回写。

> 说明：仓库内已放置接口与配置位；若要达到“完全 HF AutoProcessor 语义对齐”，需要你提供/落地 sidecar 脚本实现（模型加载、processor 调用、输出契约）。

## 5. 错误码策略（已按你要求）

- **400 Bad Request**：媒体 load 阶段问题
  - load 超时
  - 本地文件不存在/不可读
  - URL 非法或不可拉取
  - base64/data URL 异常
  - load 数据异常（大小/格式等）
- **500 Internal Server Error**：非 load 阶段前处理故障
  - 例如图像 decode/encode 等内部处理异常

## 6. 运行模式

- `RUN_MODE=preprocess_only`
  - 仅返回处理后的 payload，不转发上游。
- `RUN_MODE=proxy`
  - 处理后转发到 `${UPSTREAM_URL}/v1/chat/completions`。

## 7. 快速启动

```bash
cargo run
```

默认监听 `0.0.0.0:8080`。

## 8. 环境变量

### 8.1 基础

- `BIND_ADDR`：监听地址，默认 `0.0.0.0:8080`
- `RUN_MODE`：`auto | proxy | preprocess_only`，默认 `auto`
- `UPSTREAM_URL`：上游地址（`RUN_MODE=proxy` 必填）
- `REQUEST_TIMEOUT_MS`：请求超时，默认 `30000`
- `FETCH_TIMEOUT_MS`：媒体加载超时，默认 `15000`
- `MAX_REQUEST_BYTES`：请求体上限，默认 `16777216`
- `MAX_INFLIGHT`：并发上限，默认 `64`

### 8.2 安全

- `ALLOW_PRIVATE_NETWORK`：是否允许私网地址，默认 `false`
- `ALLOWED_HOSTS`：host 白名单（逗号分隔）

### 8.3 模型 profile

- `DEFAULT_TARGET_IMAGE_EDGE`：默认图片目标边长，默认 `1024`
- `DEFAULT_MAX_MEDIA_BYTES`：默认单媒体大小上限，默认 `20971520`
- `MODEL_PROFILES_JSON`：按 `model_id` 覆盖 profile

```json
{
  "qwen2-vl": {
    "target_image_edge": 1344,
    "max_media_bytes": 31457280
  }
}
```

### 8.4 HF 处理模式（预留）

- `HF_PROCESSOR_MODE`：`disabled | python_sidecar`，默认 `disabled`
- `HF_PYTHON_BIN`：Python 可执行文件，默认 `python3`
- `HF_SIDECAR_SCRIPT`：sidecar 脚本路径，默认 `scripts/hf_processor_sidecar.py`

## 9. API

### POST /v1/preprocess

仅执行前处理，返回处理后 payload。

### POST /v1/chat/completions

- preprocess_only：返回处理后 payload
- proxy：转发到上游 SGLang chat completions

转发时附加：

- `x-mm-preprocessed: 1`

## 10. 请求样例（可直接复制）

以下示例默认本服务地址为 `http://127.0.0.1:8080`。

### 10.1 preprocess_only + 本地文件（file://）

```bash
curl -sS "http://127.0.0.1:8080/v1/preprocess" \
  -H "content-type: application/json" \
  -d '{
    "model": "qwen2-vl",
    "messages": [{
      "role": "user",
      "content": [
        {"type": "text", "text": "describe image"},
        {"type": "image_url", "image_url": {"url": "file:///tmp/demo.png"}}
      ]
    }]
  }'
```

### 10.2 preprocess_only + 远程 URL

```bash
curl -sS "http://127.0.0.1:8080/v1/preprocess" \
  -H "content-type: application/json" \
  -d '{
    "model": "qwen2-vl",
    "messages": [{
      "role": "user",
      "content": [
        {"type": "text", "text": "what is in this image?"},
        {"type": "image_url", "image_url": {"url": "https://example.com/demo.jpg"}}
      ]
    }]
  }'
```

### 10.3 preprocess_only + base64(data URL)

```bash
IMG_B64="$(base64 -w0 /tmp/demo.png)"
curl -sS "http://127.0.0.1:8080/v1/preprocess" \
  -H "content-type: application/json" \
  -d "{
    \"model\": \"qwen2-vl\",
    \"messages\": [{
      \"role\": \"user\",
      \"content\": [
        {\"type\": \"text\", \"text\": \"analyze this\"},
        {\"type\": \"image_url\", \"image_url\": {\"url\": \"data:image/png;base64,${IMG_B64}\"}}
      ]
    }]
  }"
```

### 10.4 proxy 模式（转发到 SGLang）

先启动服务（示例）：

```bash
export RUN_MODE=proxy
export UPSTREAM_URL="http://127.0.0.1:30000"
cargo run
```

请求：

```bash
curl -sS "http://127.0.0.1:8080/v1/chat/completions" \
  -H "content-type: application/json" \
  -d '{
    "model": "qwen2-vl",
    "stream": false,
    "messages": [{
      "role": "user",
      "content": [
        {"type": "text", "text": "summarize"},
        {"type": "video_url", "video_url": {"url": "https://example.com/demo.mp4"}},
        {"type": "audio_url", "audio_url": {"url": "https://example.com/demo.mp3"}}
      ]
    }]
  }'
```

### 10.5 错误样例（应返回 400）

本地文件不存在：

```bash
curl -i "http://127.0.0.1:8080/v1/preprocess" \
  -H "content-type: application/json" \
  -d '{
    "model": "qwen2-vl",
    "messages": [{
      "role": "user",
      "content": [
        {"type": "image_url", "image_url": {"url": "file:///tmp/not-found.png"}}
      ]
    }]
  }'
```

## 11. 测试

```bash
cargo test
```

当前已覆盖：

- data URL 解析
- 本地文件不存在 -> 400
- base64 异常 -> 400
- 前处理异常 -> 500
- proxy 模式下 skip header 转发
- `audio_url` 形状兼容
