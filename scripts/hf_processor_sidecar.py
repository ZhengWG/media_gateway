#!/usr/bin/env python3
"""
HF AutoProcessor sidecar (stdin -> stdout, one JSON line).

Input line JSON:
{
  "model_id": "Qwen/Qwen2-VL-2B-Instruct",
  "payload": {
    "model": "...",
    "kind": "image|video|audio",
    "url": "data:...|http(s)://...|file://..."
  }
}

Output line JSON:
{
  "payload": {
    "url": "data:<mime>;base64,<...>",
    "meta": {
      "hf_capabilities": {"image": true, "video": false, "audio": false},
      "hf_path": "image_processor"
    }
  },
  "changed_items": 1
}
"""

from __future__ import annotations

import base64
import inspect
import io
import json
import mimetypes
import os
import sys
import tempfile
from dataclasses import dataclass
from typing import Any, Dict, Optional, Tuple
from urllib.parse import urlparse
from urllib.request import Request, urlopen

from PIL import Image
from transformers import AutoProcessor


_PROCESSOR_CACHE: Dict[str, Any] = {}


def _get_processor(model_id: str):
    if model_id not in _PROCESSOR_CACHE:
        _PROCESSOR_CACHE[model_id] = AutoProcessor.from_pretrained(model_id)
    return _PROCESSOR_CACHE[model_id]


@dataclass
class MediaPayload:
    mime: str
    data: bytes
    source: str


@dataclass
class ProcessorCapabilities:
    image: bool
    video: bool
    audio: bool


class MediaLoader:
    def load(self, url: str) -> MediaPayload:
        if url.startswith("data:"):
            mime, data = self._decode_data_url(url)
            return MediaPayload(mime=mime, data=data, source="data_url")

        if url.startswith("file://"):
            path = url[len("file://") :]
            return self._load_local(path)

        if url.startswith("/") or url.startswith("./") or url.startswith("../"):
            return self._load_local(url)

        parsed = urlparse(url)
        if parsed.scheme not in ("http", "https"):
            raise ValueError(f"unsupported media url scheme: {parsed.scheme}")
        return self._load_http(url)

    @staticmethod
    def _decode_data_url(url: str) -> Tuple[str, bytes]:
        meta, data = url.split(",", 1)
        if not meta.endswith(";base64"):
            raise ValueError("data url must be base64")
        mime = meta[len("data:") : -len(";base64")]
        return mime, base64.b64decode(data)

    @staticmethod
    def _load_local(path: str) -> MediaPayload:
        with open(path, "rb") as f:
            data = f.read()
        mime = mimetypes.guess_type(path)[0] or "application/octet-stream"
        return MediaPayload(mime=mime, data=data, source="local_file")

    @staticmethod
    def _load_http(url: str) -> MediaPayload:
        req = Request(url, headers={"User-Agent": "media-gateway-hf-sidecar/1.0"})
        with urlopen(req, timeout=15) as resp:
            data = resp.read()
            mime = resp.headers.get_content_type() or "application/octet-stream"
        return MediaPayload(mime=mime, data=data, source="http")


def _detect_capabilities(processor: Any) -> ProcessorCapabilities:
    call = getattr(processor, "__call__", None)
    if call is None:
        return ProcessorCapabilities(image=False, video=False, audio=False)
    try:
        sig = inspect.signature(call)
        params = set(sig.parameters.keys())
    except Exception:
        params = set()

    return ProcessorCapabilities(
        image=("images" in params),
        video=("videos" in params),
        audio=("audios" in params) or ("audio" in params),
    )


def _decode_data_url(url: str) -> Tuple[str, bytes]:
    meta, data = url.split(",", 1)
    if not meta.endswith(";base64"):
        raise ValueError("data url must be base64")
    mime = meta[len("data:") : -len(";base64")]
    return mime, base64.b64decode(data)


def _call_image_processor(processor: Any, image: Image.Image) -> None:
    attempts = [
        {"images": image, "return_tensors": "pt"},
        {"images": [image], "return_tensors": "pt"},
        {"text": "", "images": image, "return_tensors": "pt"},
        {"text": [""], "images": [image], "return_tensors": "pt"},
    ]
    last_err: Optional[Exception] = None
    for kwargs in attempts:
        try:
            _ = processor(**kwargs)
            return
        except Exception as e:  # noqa: BLE001
            last_err = e
    raise ValueError(f"HF image processor call failed: {last_err}")


def _call_video_processor(processor: Any, media_path: str) -> None:
    attempts = [
        {"videos": [media_path], "return_tensors": "pt"},
        {"text": "", "videos": [media_path], "return_tensors": "pt"},
        {"text": [""], "videos": [media_path], "return_tensors": "pt"},
    ]
    last_err: Optional[Exception] = None
    for kwargs in attempts:
        try:
            _ = processor(**kwargs)
            return
        except Exception as e:  # noqa: BLE001
            last_err = e
    raise ValueError(f"HF video processor call failed: {last_err}")


def _call_audio_processor(processor: Any, media_path: str) -> None:
    attempts = [
        {"audios": [media_path], "return_tensors": "pt"},
        {"audio": [media_path], "return_tensors": "pt"},
        {"text": "", "audios": [media_path], "return_tensors": "pt"},
        {"text": "", "audio": [media_path], "return_tensors": "pt"},
    ]
    last_err: Optional[Exception] = None
    for kwargs in attempts:
        try:
            _ = processor(**kwargs)
            return
        except Exception as e:  # noqa: BLE001
            last_err = e
    raise ValueError(f"HF audio processor call failed: {last_err}")


def _encode_data_url(mime: str, data: bytes) -> str:
    return f"data:{mime};base64,{base64.b64encode(data).decode('ascii')}"


def _pixel_values_to_bytes(pixel_values: Any) -> bytes:
    # Expect torch.Tensor-like object with shape [1, C, H, W] or [C, H, W].
    try:
        arr = pixel_values.detach().cpu().float().contiguous().numpy()
    except Exception as e:  # noqa: BLE001
        raise ValueError(f"unable to convert pixel_values tensor: {e}") from e

    if arr.ndim == 4 and arr.shape[0] == 1:
        arr = arr[0]
    if arr.ndim != 3:
        raise ValueError(f"unexpected pixel_values shape: {arr.shape}")
    # CHW float32 bytes.
    return arr.astype("float32", copy=False).tobytes(order="C")


def _process_image_with_hf(processor: Any, raw: bytes) -> Tuple[bytes, str]:
    # Reuse HF call path for model-specific image preprocess interface and emit pixel_values bytes.
    img = Image.open(io.BytesIO(raw)).convert("RGB")
    enc = None
    attempts = [
        {"images": img, "return_tensors": "pt"},
        {"images": [img], "return_tensors": "pt"},
        {"text": "", "images": img, "return_tensors": "pt"},
        {"text": [""], "images": [img], "return_tensors": "pt"},
    ]
    last_err: Optional[Exception] = None
    for kwargs in attempts:
        try:
            enc = processor(**kwargs)
            break
        except Exception as e:  # noqa: BLE001
            last_err = e
    if enc is None:
        raise ValueError(f"HF image processor call failed: {last_err}")

    if "pixel_values" not in enc:
        raise ValueError("HF processor output does not include pixel_values")

    pixel_values = enc["pixel_values"]
    bytes_payload = _pixel_values_to_bytes(pixel_values)
    shape = tuple(int(x) for x in pixel_values.shape)
    if len(shape) == 4:
        _, c, h, w = shape
    elif len(shape) == 3:
        c, h, w = shape
    else:
        raise ValueError(f"unexpected pixel_values shape: {shape}")
    mime = f"application/x-pixel-values+f32;layout=nchw;shape=1x{c}x{h}x{w}"
    return bytes_payload, mime


def _handle(req: Dict[str, Any]) -> Dict[str, Any]:
    model_id = req["model_id"]
    payload = req["payload"]
    kind = payload.get("kind")
    url = payload.get("url")
    if not model_id or not kind or not url:
        raise ValueError("missing model_id/kind/url")

    loader = MediaLoader()
    payload_obj = loader.load(url)
    processor = _get_processor(model_id)
    capabilities = _detect_capabilities(processor)
    mime, raw = payload_obj.mime, payload_obj.data
    path = "passthrough"

    if kind == "image":
        if not capabilities.image:
            raise ValueError(f"processor for {model_id} does not support image inputs")
        processed, mime = _process_image_with_hf(processor, raw)
        path = "image_processor"
    elif kind == "video":
        if capabilities.video:
            with tempfile.NamedTemporaryFile(suffix=".mp4", delete=True) as tmp:
                tmp.write(raw)
                tmp.flush()
                _call_video_processor(processor, tmp.name)
            path = "video_processor"
        processed = raw
    elif kind == "audio":
        if capabilities.audio:
            with tempfile.NamedTemporaryFile(suffix=".wav", delete=True) as tmp:
                tmp.write(raw)
                tmp.flush()
                _call_audio_processor(processor, tmp.name)
            path = "audio_processor"
        processed = raw
    else:
        raise ValueError(f"unsupported kind: {kind}")

    return {
        "payload": {
            "url": _encode_data_url(mime, processed),
            "meta": {
                "hf_capabilities": {
                    "image": capabilities.image,
                    "video": capabilities.video,
                    "audio": capabilities.audio,
                },
                "hf_path": path,
                "source": payload_obj.source,
            },
        },
        "changed_items": 1,
    }


def main() -> int:
    line = sys.stdin.readline()
    if not line:
        return 1
    try:
        req = json.loads(line)
        out = _handle(req)
        sys.stdout.write(json.dumps(out, ensure_ascii=True) + "\n")
        sys.stdout.flush()
        return 0
    except Exception as e:  # noqa: BLE001
        sys.stderr.write(f"{type(e).__name__}: {e}\n")
        sys.stderr.flush()
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
