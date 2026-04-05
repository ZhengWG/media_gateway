use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use image::imageops::FilterType;
use image::{DynamicImage, ImageFormat};
use std::io::Cursor;
use std::net::IpAddr;
use std::time::{Duration, Instant};
use url::Url;

use crate::error::{GatewayError, Result};

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
        .ok_or_else(|| GatewayError::BadRequest("invalid data url format".into()))?;
    if !meta.ends_with(";base64") {
        return Err(GatewayError::BadRequest(
            "data url must use base64 encoding".into(),
        ));
    }
    let mime = meta
        .trim_start_matches("data:")
        .trim_end_matches(";base64")
        .to_string();
    let bytes = BASE64
        .decode(body.as_bytes())
        .map_err(|_| GatewayError::BadRequest("invalid data url base64".into()))?;
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
    url: &str,
    max_media_bytes: usize,
    fetch_timeout: Duration,
    allowed_hosts: &std::collections::HashSet<String>,
    allow_private_network: bool,
) -> Result<MediaPayload> {
    let started = Instant::now();
    let parsed =
        Url::parse(url).map_err(|_| GatewayError::BadRequest("invalid media url".into()))?;
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return Err(GatewayError::BadRequest(format!(
            "unsupported media scheme: {}",
            parsed.scheme()
        )));
    }
    validate_remote_url(&parsed, allowed_hosts, allow_private_network)?;

    let resp = client
        .get(url)
        .timeout(fetch_timeout)
        .send()
        .await
        .map_err(|e| GatewayError::Upstream(format!("media fetch failed: {e}")))?;

    if !resp.status().is_success() {
        return Err(GatewayError::Upstream(format!(
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
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| GatewayError::Upstream(format!("read media body failed: {e}")))?
        .to_vec();
    ensure_size_limit(bytes.len(), max_media_bytes)?;

    metrics::histogram!("media_fetch_duration_seconds", "kind" => "http")
        .record(started.elapsed().as_secs_f64());
    metrics::counter!("media_fetch_total", "kind" => "http").increment(1);

    Ok(MediaPayload { mime, bytes })
}

pub fn preprocess_image(payload: MediaPayload, target_edge: u32) -> Result<MediaPayload> {
    let started = Instant::now();
    ensure_size_limit(payload.bytes.len(), usize::MAX)?;
    let img = image::load_from_memory(&payload.bytes)
        .map_err(|e| GatewayError::Validation(format!("decode image failed: {e}")))?;
    let resized = resize_keep_ratio(img, target_edge);
    let mut out = Vec::new();
    resized
        .write_to(&mut Cursor::new(&mut out), ImageFormat::Jpeg)
        .map_err(|e| GatewayError::Internal(format!("encode image failed: {e}")))?;

    metrics::histogram!(
        "media_preprocess_duration_seconds",
        "stage" => "image_resize",
        "media_type" => "image"
    )
    .record(started.elapsed().as_secs_f64());
    metrics::counter!("media_preprocess_total", "media_type" => "image").increment(1);

    Ok(MediaPayload {
        mime: "image/jpeg".to_string(),
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
    Err(GatewayError::Validation(
        "unsupported image format, expected jpeg/png/webp".into(),
    ))
}

fn resize_keep_ratio(img: DynamicImage, target_edge: u32) -> DynamicImage {
    let (w, h) = (img.width(), img.height());
    if w <= target_edge && h <= target_edge {
        return img;
    }
    let scale = if w > h {
        target_edge as f32 / w as f32
    } else {
        target_edge as f32 / h as f32
    };
    let nw = ((w as f32) * scale).round().max(1.0) as u32;
    let nh = ((h as f32) * scale).round().max(1.0) as u32;
    img.resize_exact(nw, nh, FilterType::Lanczos3)
}

fn validate_remote_url(
    url: &Url,
    allowed_hosts: &std::collections::HashSet<String>,
    allow_private_network: bool,
) -> Result<()> {
    let host = url
        .host_str()
        .ok_or_else(|| GatewayError::Validation("media url missing host".into()))?
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
        return Err(GatewayError::PayloadTooLarge { size, limit });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageBuffer, Rgb};

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
        let resized = resize_keep_ratio(img, 1024);
        assert_eq!(resized.width(), 1024);
        assert_eq!(resized.height(), 512);
    }
}
