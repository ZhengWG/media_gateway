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
    "url": "data:<mime>;base64,<...>"
  },
  "changed_items": 1
}
"""

from __future__ import annotations

import base64
import io
import json
import mimetypes
import os
import sys
from typing import Any, Dict, Tuple
from urllib.parse import urlparse
from urllib.request import Request, urlopen

from PIL import Image
from transformers import AutoProcessor


_PROCESSOR_CACHE: Dict[str, Any] = {}


def _get_processor(model_id: str):
    if model_id not in _PROCESSOR_CACHE:
        _PROCESSOR_CACHE[model_id] = AutoProcessor.from_pretrained(model_id)
    return _PROCESSOR_CACHE[model_id]


def _decode_data_url(url: str) -> Tuple[str, bytes]:
    meta, data = url.split(",", 1)
    if not meta.endswith(";base64"):
        raise ValueError("data url must be base64")
    mime = meta[len("data:") : -len(";base64")]
    return mime, base64.b64decode(data)


def _load_bytes(url: str) -> Tuple[str, bytes]:
    if url.startswith("data:"):
        return _decode_data_url(url)

    if url.startswith("file://"):
        path = url[len("file://") :]
        with open(path, "rb") as f:
            data = f.read()
        mime = mimetypes.guess_type(path)[0] or "application/octet-stream"
        return mime, data

    if url.startswith("/") or url.startswith("./") or url.startswith("../"):
        with open(url, "rb") as f:
            data = f.read()
        mime = mimetypes.guess_type(url)[0] or "application/octet-stream"
        return mime, data

    parsed = urlparse(url)
    if parsed.scheme not in ("http", "https"):
        raise ValueError(f"unsupported media url scheme: {parsed.scheme}")
    req = Request(url, headers={"User-Agent": "media-gateway-hf-sidecar/1.0"})
    with urlopen(req, timeout=15) as resp:
        data = resp.read()
        mime = resp.headers.get_content_type() or "application/octet-stream"
    return mime, data


def _encode_data_url(mime: str, data: bytes) -> str:
    return f"data:{mime};base64,{base64.b64encode(data).decode('ascii')}"


def _process_image_with_hf(model_id: str, raw: bytes) -> bytes:
    # Call AutoProcessor to align with HF preprocess semantics.
    # We still return a normalized image bytes payload for gateway compatibility.
    processor = _get_processor(model_id)
    img = Image.open(io.BytesIO(raw)).convert("RGB")
    _ = processor(images=img, return_tensors="pt")

    out = io.BytesIO()
    img.save(out, format="JPEG", quality=90)
    return out.getvalue()


def _handle(req: Dict[str, Any]) -> Dict[str, Any]:
    model_id = req["model_id"]
    payload = req["payload"]
    kind = payload.get("kind")
    url = payload.get("url")
    if not model_id or not kind or not url:
        raise ValueError("missing model_id/kind/url")

    mime, raw = _load_bytes(url)
    if kind == "image":
        processed = _process_image_with_hf(model_id, raw)
        mime = "image/jpeg"
    else:
        # video/audio passthrough for current phase
        processed = raw

    return {
        "payload": {
            "url": _encode_data_url(mime, processed),
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
