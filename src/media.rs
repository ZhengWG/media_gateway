use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use std::net::IpAddr;
use std::path::Path;
use std::time::{Duration, Instant};
use url::Url;

use crate::error::{GatewayError, Result};
use crate::preprocess_ops::image::image_to_pixel_values_nchw_f32;

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum MediaKind {
    Image,
    Video,
    Audio,
}

impl MediaKind {
    pub fn as_str(self) -> &'static str {
        match self {
            MediaKind::Image => "image",
            MediaKind::Video => "video",
            MediaKind::Audio => "audio",
        }
    }
}

#[derive(Debug, Clone)]
pub struct MediaPayload {
    pub mime: String,
    pub bytes: Vec<u8>,
}

pub fn decode_data_url(raw: &str) -> Result<Option<MediaPayload>> {
    if !raw.starts_with("data:") {
        return Ok(None);
    }
    let (meta, body) = raw
        .split_once(',')
        .ok_or_else(|| GatewayError::MediaLoad("invalid data url format".into()))?;
    if !meta.ends_with(";base64") {
        return Err(GatewayError::MediaLoad(
            "data url must use base64 encoding".into(),
        ));
    }
    let mime = meta
        .trim_start_matches("data:")
        .trim_end_matches(";base64")
        .to_string();
    let bytes = BASE64
        .decode(body.as_bytes())
        .map_err(|_| GatewayError::MediaLoad("invalid data url base64".into()))?;
    Ok(Some(MediaPayload { mime, bytes }))
}

pub fn encode_data_url(payload: &MediaPayload) -> String {
    format!(
        "data:{};base64,{}",
        payload.mime,
        BASE64.encode(&payload.bytes)
    )
}

pub async fn fetch_media(
    client: &reqwest::Client,
    location: &str,
    max_media_bytes: usize,
    fetch_timeout: Duration,
    allowed_hosts: &std::collections::HashSet<String>,
    allow_private_network: bool,
) -> Result<MediaPayload> {
    if let Some(data) = decode_data_url(location)? {
        ensure_size_limit(data.bytes.len(), max_media_bytes)?;
        metrics::counter!("media_fetch_total", "kind" => "data_url").increment(1);
        return Ok(data);
    }

    if is_local_path(location) {
        return load_local_media(location, max_media_bytes);
    }

    let started = Instant::now();
    let parsed =
        Url::parse(location).map_err(|_| GatewayError::MediaLoad("invalid media url".into()))?;
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return Err(GatewayError::MediaLoad(format!(
            "unsupported media scheme: {}",
            parsed.scheme()
        )));
    }
    validate_remote_url(&parsed, allowed_hosts, allow_private_network)?;

    let resp = client
        .get(location)
        .timeout(fetch_timeout)
        .send()
        .await
        .map_err(map_http_load_error)?;

    if !resp.status().is_success() {
        return Err(GatewayError::MediaLoad(format!(
            "media fetch status: {}",
            resp.status()
        )));
    }

    let mime = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next())
        .map(str::trim)
        .unwrap_or("application/octet-stream")
        .to_string();
    let bytes = resp.bytes().await.map_err(map_http_load_error)?.to_vec();
    ensure_size_limit(bytes.len(), max_media_bytes)?;

    metrics::histogram!("media_fetch_duration_seconds", "kind" => "http")
        .record(started.elapsed().as_secs_f64());
    metrics::counter!("media_fetch_total", "kind" => "http").increment(1);

    Ok(MediaPayload { mime, bytes })
}

pub fn preprocess_image_to_pixel_values(
    payload: MediaPayload,
    target_edge: u32,
) -> Result<MediaPayload> {
    let started = Instant::now();
    ensure_size_limit(payload.bytes.len(), usize::MAX)?;
    let (out, h, w) = image_to_pixel_values_nchw_f32(&payload.bytes, target_edge)?;

    metrics::histogram!(
        "media_preprocess_duration_seconds",
        "stage" => "image_resize",
        "media_type" => "image"
    )
    .record(started.elapsed().as_secs_f64());
    metrics::counter!("media_preprocess_total", "media_type" => "image").increment(1);

    Ok(MediaPayload {
        mime: format!(
            "application/x-pixel-values+f32;layout=nchw;shape=1x3x{}x{}",
            h, w
        ),
        bytes: out,
    })
}

#[allow(dead_code)]
pub fn detect_image_format(payload: &MediaPayload) -> Result<String> {
    let b = &payload.bytes;
    if b.len() >= 3 && b.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Ok("image/jpeg".to_string());
    }
    if b.len() >= 8 && b.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Ok("image/png".to_string());
    }
    if b.len() >= 12 && &b[0..4] == b"RIFF" && &b[8..12] == b"WEBP" {
        return Ok("image/webp".to_string());
    }
    Err(GatewayError::MediaLoad(
        "unsupported image format, expected jpeg/png/webp".into(),
    ))
}

fn validate_remote_url(
    url: &Url,
    allowed_hosts: &std::collections::HashSet<String>,
    allow_private_network: bool,
) -> Result<()> {
    let host = url
        .host_str()
        .ok_or_else(|| GatewayError::MediaLoad("media url missing host".into()))?
        .to_ascii_lowercase();

    if !allowed_hosts.is_empty()
        && !allowed_hosts
            .iter()
            .any(|h| host == *h || host.ends_with(&format!(".{h}")))
    {
        return Err(GatewayError::Security(format!(
            "host not allowed by policy: {host}"
        )));
    }

    if !allow_private_network {
        if host == "localhost" {
            return Err(GatewayError::Security(
                "localhost not allowed by SSRF policy".into(),
            ));
        }
        if let Ok(ip) = host.parse::<IpAddr>() {
            if is_private_ip(ip) {
                return Err(GatewayError::Security(format!(
                    "private IP not allowed by SSRF policy: {host}"
                )));
            }
        }
    }
    Ok(())
}

fn is_private_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_private() || v4.is_loopback() || v4.is_link_local(),
        IpAddr::V6(v6) => {
            let seg = v6.segments();
            let is_unique_local = (seg[0] & 0xfe00) == 0xfc00; // fc00::/7
            v6.is_loopback() || v6.is_unspecified() || is_unique_local
        }
    }
}

fn ensure_size_limit(size: usize, limit: usize) -> Result<()> {
    if size > limit {
        return Err(GatewayError::MediaLoad(format!(
            "media payload too large: {size} > {limit}"
        )));
    }
    Ok(())
}

fn is_local_path(location: &str) -> bool {
    location.starts_with("file://")
        || location.starts_with('/')
        || location.starts_with("./")
        || location.starts_with("../")
}

fn load_local_media(location: &str, max_media_bytes: usize) -> Result<MediaPayload> {
    let path = if let Some(raw) = location.strip_prefix("file://") {
        raw
    } else {
        location
    };
    let path_ref = Path::new(path);
    if !path_ref.exists() {
        return Err(GatewayError::MediaLoad(format!(
            "local file not found: {}",
            path_ref.display()
        )));
    }
    if !path_ref.is_file() {
        return Err(GatewayError::MediaLoad(format!(
            "local path is not a file: {}",
            path_ref.display()
        )));
    }

    let bytes = std::fs::read(path_ref).map_err(|e| {
        GatewayError::MediaLoad(format!(
            "failed to read local file {}: {e}",
            path_ref.display()
        ))
    })?;
    ensure_size_limit(bytes.len(), max_media_bytes)?;
    let mime = infer_mime_from_path(path_ref).to_string();
    metrics::counter!("media_fetch_total", "kind" => "local_file").increment(1);
    Ok(MediaPayload { mime, bytes })
}

fn infer_mime_from_path(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|v| v.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "webp" => "image/webp",
        "mp4" => "video/mp4",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        _ => "application/octet-stream",
    }
}

fn map_http_load_error(e: reqwest::Error) -> GatewayError {
    if e.is_timeout() {
        return GatewayError::MediaLoad("media load timeout".to_string());
    }
    GatewayError::MediaLoad(format!("media load failed: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageBuffer, ImageFormat, Rgb};
    use std::io::Cursor;

    #[test]
    fn data_url_roundtrip() {
        let raw = b"hello";
        let s = format!("data:text/plain;base64,{}", BASE64.encode(raw));
        let p = decode_data_url(&s).expect("parse").expect("some");
        assert_eq!(p.mime, "text/plain");
        assert_eq!(p.bytes, raw);
    }

    #[test]
    fn detect_private_ip() {
        assert!(is_private_ip("10.0.0.1".parse().expect("v4")));
        assert!(!is_private_ip("8.8.8.8".parse().expect("v4")));
    }

    #[test]
    fn image_resize_down_to_target_edge() {
        let img = image::DynamicImage::ImageRgb8(ImageBuffer::<Rgb<u8>, _>::from_fn(
            2000,
            1000,
            |_, _| Rgb([255, 0, 0]),
        ));
        let resized = crate::preprocess_ops::image::resize_keep_ratio(img, 1024);
        assert_eq!(resized.width(), 1024);
        assert_eq!(resized.height(), 512);
    }

    #[test]
    fn preprocess_image_outputs_pixel_values_payload() {
        let img =
            image::DynamicImage::ImageRgb8(ImageBuffer::<Rgb<u8>, _>::from_fn(2, 2, |_, _| {
                Rgb([255, 128, 0])
            }));
        let mut encoded = Vec::new();
        img.write_to(&mut Cursor::new(&mut encoded), ImageFormat::Png)
            .expect("png encode");

        let out = preprocess_image_to_pixel_values(
            MediaPayload {
                mime: "image/png".to_string(),
                bytes: encoded,
            },
            2,
        )
        .expect("preprocess");
        assert!(out
            .mime
            .starts_with("application/x-pixel-values+f32;layout=nchw;shape=1x3x2x2"));
        assert_eq!(out.bytes.len(), 2 * 2 * 3 * 4);
    }
}
