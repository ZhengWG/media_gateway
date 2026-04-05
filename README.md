# media_gateway

面向 vLLM / SGLang 的多模态前处理独立模块（首期 HTTP 版本）。

## 功能概览

当前实现已覆盖架构设计中的首期关键闭环（HTTP 优先）：

- OpenAI 兼容请求解析（`/v1/chat/completions`、`/v1/preprocess`）。
- 多模态内容识别（`messages[].content[]` 中的 `image_url` / `video_url` / `audio`）。
- 媒体获取与规范化：
  - 支持 `data:*;base64,...` 直接解析。
  - 支持 `http(s)` 拉取远程媒体。
  - 支持本地文件路径（绝对路径、`./`、`../`、`file://`）。
  - 图片轻量前处理（按模型 profile 边长缩放并编码为 JPEG）。
  - 视频/音频首期透传（仍封装为 data URL，便于后续 Sidecar 增强）。
- SSRF 基础防护（host allowlist、私网地址限制）。
- 处理后回写为 data URL，并附加 `mm_preprocessed=true` + `x-mm-preprocessed: 1` 语义标记。
- 运行模式：
  - `preprocess_only`：仅返回处理后的 payload。
  - `proxy`：处理后转发到上游 OpenAI 兼容接口。
- 可观测性接口：
  - `GET /live`
  - `GET /ready`
  - `GET /health`
  - `GET /metrics`（Prometheus）

## 本地运行

```bash
cargo run
```

默认监听 `0.0.0.0:8080`。

## 环境变量

### 基础配置

- `BIND_ADDR`：监听地址，默认 `0.0.0.0:8080`
- `RUN_MODE`：`auto | proxy | preprocess_only`，默认 `auto`
- `UPSTREAM_URL`：上游引擎地址（`RUN_MODE=proxy` 必填）
- `REQUEST_TIMEOUT_MS`：请求级超时，默认 `30000`
- `FETCH_TIMEOUT_MS`：媒体拉取超时，默认 `15000`
- `MAX_REQUEST_BYTES`：请求体大小上限，默认 `16777216`
- `MAX_INFLIGHT`：并发槽位（预留参数），默认 `64`

### 安全策略

- `ALLOW_PRIVATE_NETWORK`：是否允许内网地址，默认 `false`
- `ALLOWED_HOSTS`：允许的 host 白名单，逗号分隔；为空时不启用 host 白名单

### 模型配置

- `DEFAULT_TARGET_IMAGE_EDGE`：默认图片目标边长，默认 `1024`
- `DEFAULT_MAX_MEDIA_BYTES`：默认单媒体字节上限，默认 `20971520`
- `MODEL_PROFILES_JSON`：按 `model_id` 覆盖 profile 的 JSON，例如：

```json
{
  "qwen2-vl": {
    "target_image_edge": 1344,
    "max_media_bytes": 31457280
  }
}
```

## API

### POST /v1/preprocess

仅执行前处理并返回处理后的 JSON。

### POST /v1/chat/completions

- `RUN_MODE=preprocess_only`：返回处理后的 payload。
- `RUN_MODE=proxy`：转发到 `${UPSTREAM_URL}/v1/chat/completions`。

转发时会附加头：

- `x-mm-preprocessed: 1`

## 错误码策略

- 媒体 load 阶段错误统一返回 **400**：
  - base64 格式错误/解码失败
  - 本地文件不存在或不可读
  - URL 不合法或超时
  - load 数据异常（格式/大小等）
- 非 load 阶段的前处理错误返回 **500**（`Internal Server Error`）：
  - 例如图像解码/重编码等处理流程异常

## 指标（示例）

- `gateway_requests_total`
- `gateway_inflight`
- `gateway_request_seconds`
- `media_fetch_total`
- `media_fetch_duration_seconds`
- `media_preprocess_total`
- `media_preprocess_duration_seconds`
- `media_gateway_media_processed_total`

## 下一步（与设计文档对齐）

- 接入 Python Sidecar 池（F-05/F-06），补齐 HF AutoProcessor 语义对齐。
- 增加视频采帧/音频解码轻量策略（F-07 扩展）。
- 补充有界队列与背压（F-10）。
- 增加 Sidecar 监督/重启与可重试策略（F-12）。
- 引擎协同冻结 skip 契约与 golden 对比（F-30/F-31）。
