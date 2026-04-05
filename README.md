# media_gateway

面向 vLLM / SGLang 的多模态前处理独立模块（HTTP-first）。

## 1. 当前目标与状态

本项目当前处于“优先打通 HTTP 全链路”的阶段：

- 对外提供 OpenAI 兼容入口（重点：`/v1/chat/completions`）。
- 在网关层完成媒体加载与轻量前处理。
- 处理后仍回写 OpenAI 兼容形状（`image_url/video_url/audio_url` 的 data URL）。
- 在 proxy 模式下转发到 SGLang `/v1/chat/completions`（上游地址从请求 JSON 中读取）。

### 1.1 当前支持模型范围（已收敛）

- 仅支持 **Qwen 系列** 与 **Kimi 系列** 模型。
- `model` 字段若不属于这两类，将返回 `400 Bad Request`。

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

- 图片：按模型 profile 的目标边长缩放后，输出 `pixel_values`（`float32`、`NCHW`）并编码为 base64 data URL，回写到 `image_url.url`。
- 视频/音频：首期透传（只做加载与统一回写，不做重解码采样）。

## 4. HF Processor 接入模式

当前提供了 **HF Sidecar 预留接入位**（`hf_sidecar` 模块）：

- 用于将重前处理逻辑迁移到 Python（AutoProcessor）侧实现。
- 网关保持 Rust 控制面与协议编排。
- 当 sidecar 可用时，网关可将 payload 发送给 sidecar，接收处理后的 payload 回写。

实现说明（已落地）：

- 仓库包含可运行示例：`scripts/hf_processor_sidecar.py`
- sidecar 内部对媒体加载逻辑做了抽象（本地/URL/base64）
- 对多模态处理优先走 `AutoProcessor`（`processor(text=..., images=.../videos=.../audio=...)`）
- 对不同模型能力自动探测并降级：
  - 如果 processor 不支持对应模态，退回到安全默认（例如视频/音频保持原始 data URL）
  - 并在 stderr 输出能力探测信息，便于运维定位模型差异

> 不同模型在视频抽帧、音频采样、图像 resize 细节上存在差异，建议由对应模型的 `AutoProcessor` 作为语义真值来源。

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
  - 处理后转发到请求 body 的 `upstream_url` 指定地址（`${upstream_url}/v1/chat/completions`）。

## 7. 快速启动

```bash
cargo run
```

默认监听 `0.0.0.0:8080`。

## 8. 环境变量

### 8.1 基础

- `BIND_ADDR`：监听地址，默认 `0.0.0.0:8080`
- `RUN_MODE`：`auto | proxy | preprocess_only`，默认 `auto`
  - `proxy` 模式下，目标上游地址不再读取环境变量，而是从请求 JSON body 的 `upstream_url` 字段读取
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
- `HF_SIDECAR_COMMAND_TEMPLATE`：sidecar 命令模板，默认 `"{python_bin} {script_path}"`
  - 支持占位符：
    - `{python_bin}` -> `HF_PYTHON_BIN`
    - `{script_path}` -> `HF_SIDECAR_SCRIPT`
  - 适合需要先 `source` 虚拟环境、导出环境变量、再执行脚本的场景
  - 支持第三方库导入（取决于你命令里激活的 Python 环境）
  - 示例：
    - `source /opt/venv/bin/activate && export HF_HOME=/data/hf && {python_bin} {script_path}`
- `HF_SIDECAR_TIMEOUT_MS`：sidecar 超时，默认跟 `REQUEST_TIMEOUT_MS` 一致
- `INJECT_PROCESSOR_OUTPUT`：是否将 sidecar 返回的结构化 `processor_output` 注入到 `image_url/video_url/audio_url` 对象中，默认 `false`
  - 打开后会做按模态字段白名单过滤，仅保留稳定键（避免把无关字段透传给引擎）

### 8.5 启动参数组合样例（可直接复制）

> 说明：以下样例均假设在仓库根目录执行 `cargo run`。  
> 当前仅支持 Qwen/Kimi 系列模型，请确保请求里的 `model` 对应这两类。

#### 样例 A：最小化本地启动（preprocess_only）

用途：本地联调，只做前处理不转发。

```bash
export RUN_MODE=preprocess_only
export BIND_ADDR=0.0.0.0:8080
export MAX_INFLIGHT=32
cargo run
```

#### 样例 B：proxy 模式接 SGLang（常用）

用途：网关负责前处理并转发到上游（`upstream_url` 从请求体传入）。

```bash
export RUN_MODE=proxy
export BIND_ADDR=0.0.0.0:8080
export REQUEST_TIMEOUT_MS=60000
export FETCH_TIMEOUT_MS=20000
export MAX_INFLIGHT=64
cargo run
```

#### 样例 C：严格安全策略（禁私网 + host 白名单）

用途：生产环境限制外部拉流目标，降低 SSRF 风险。

```bash
export RUN_MODE=proxy
export ALLOW_PRIVATE_NETWORK=false
export ALLOWED_HOSTS=cdn.example.com,media.example.com
export MAX_REQUEST_BYTES=16777216
cargo run
```

#### 样例 D：启用 HF sidecar（独立 Python 环境）

用途：把多模态前处理语义交给 HF AutoProcessor。

```bash
export RUN_MODE=proxy
export HF_PROCESSOR_MODE=python_sidecar
export HF_PYTHON_BIN=/opt/venv/bin/python
export HF_SIDECAR_SCRIPT=/workspace/scripts/hf_processor_sidecar.py
export HF_SIDECAR_COMMAND_TEMPLATE='source /opt/venv/bin/activate && export HF_HOME=/data/hf && {python_bin} {script_path}'
export HF_SIDECAR_TIMEOUT_MS=90000
cargo run
```

#### 样例 E：启用 processor_output 注入（SGLang 对齐路径）

用途：在 data URL 回写之外，同时注入结构化 `processor_output`（白名单过滤后）。

```bash
export RUN_MODE=proxy
export HF_PROCESSOR_MODE=python_sidecar
export INJECT_PROCESSOR_OUTPUT=true
export HF_PYTHON_BIN=/opt/venv/bin/python
export HF_SIDECAR_SCRIPT=/workspace/scripts/hf_processor_sidecar.py
cargo run
```

#### 样例 F：按模型覆盖 profile（Qwen/Kimi）

用途：对不同模型设置不同图片目标边长与媒体大小上限。

```bash
export RUN_MODE=proxy
export DEFAULT_TARGET_IMAGE_EDGE=1024
export DEFAULT_MAX_MEDIA_BYTES=20971520
export MODEL_PROFILES_JSON='{
  "Qwen/Qwen2.5-VL-3B-Instruct": {"target_image_edge": 1344, "max_media_bytes": 31457280},
  "moonshotai/Kimi-VL-A3B-Instruct": {"target_image_edge": 1120, "max_media_bytes": 20971520}
}'
cargo run
```

#### 样例 G：高并发压测参数（示例）

用途：压测或大流量验证（需按机器 CPU/内存实际调优）。

```bash
export RUN_MODE=proxy
export MAX_INFLIGHT=256
export REQUEST_TIMEOUT_MS=120000
export FETCH_TIMEOUT_MS=30000
export MAX_REQUEST_BYTES=33554432
cargo run
```

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
cargo run
```

请求：

```bash
curl -sS "http://127.0.0.1:8080/v1/chat/completions" \
  -H "content-type: application/json" \
  -d '{
    "upstream_url": "http://127.0.0.1:30000",
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

### 10.6 HF sidecar 命令配置示例（含第三方库环境）

方式 A：使用默认脚本路径组合

```bash
export HF_PROCESSOR_MODE=python_sidecar
export HF_PYTHON_BIN="/opt/venv/bin/python"
export HF_SIDECAR_SCRIPT="/workspace/scripts/hf_processor_sidecar.py"
```

方式 B：自定义命令模板（推荐复杂环境）

```bash
export HF_PROCESSOR_MODE=python_sidecar
export HF_SIDECAR_COMMAND_TEMPLATE='source /opt/venv/bin/activate && export HF_HOME=/data/hf && {python_bin} {script_path}'
export HF_SIDECAR_TIMEOUT_MS=60000
```

如果 sidecar 需要 `transformers/torch/opencv` 等第三方库，确保上述 Python 环境已安装对应依赖。

### 10.7 HF sidecar 实际可用脚本（AutoProcessor）

仓库提供了一个可直接运行的示例脚本：`scripts/hf_processor_sidecar.py`。  
脚本协议与网关一致：从 stdin 读取一行 JSON，输出一行 JSON。

#### 10.7.1 依赖安装

```bash
python3 -m pip install -U transformers pillow requests torch
```

#### 10.7.2 启动参数（环境变量）

- `HF_MODEL_ID`：默认模型（例如 `Qwen/Qwen2.5-VL-3B-Instruct`）
- `HF_DEVICE`：`cpu` 或 `cuda`（默认 `cpu`）
- `HF_TRUST_REMOTE_CODE`：`0/1`（默认 `0`）
- `HF_PROCESSOR_OUTPUT_MODE`：`passthrough | processed_data_url`
  - `passthrough`：保持原 URL（最安全）
  - `processed_data_url`：对 image 走 `AutoProcessor(..., return_tensors="pt")` 后，将 pixel_values 的首张张量近似回编码为 data URL

#### 10.7.3 与网关联动

```bash
export HF_PROCESSOR_MODE=python_sidecar
export HF_PYTHON_BIN="/opt/venv/bin/python"
export HF_SIDECAR_SCRIPT="/workspace/scripts/hf_processor_sidecar.py"
export HF_SIDECAR_COMMAND_TEMPLATE='source /opt/venv/bin/activate && export HF_MODEL_ID=Qwen/Qwen2.5-VL-3B-Instruct && {python_bin} {script_path}'
```

> 说明：`processed_data_url` 模式用于演示如何调用 HF `AutoProcessor`。  
> 严格生产语义仍建议在你们与引擎约定的“processor_output/skip 契约”下落地，避免回编码带来的信息损失。

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
