use rayon::prelude::*;

use crate::error::{GatewayError, Result};

pub fn image_to_pixel_values_nchw_f32(raw: &[u8], target_edge: u32) -> Result<(Vec<u8>, u32, u32)> {
    let img = image::load_from_memory(raw)
        .map_err(|e| GatewayError::Internal(format!("decode image failed: {e}")))?;
    let resized = resize_keep_ratio(img, target_edge);
    let rgb = resized.to_rgb8();
    let (w, h) = rgb.dimensions();

    // Build channel planes in parallel, then flatten to contiguous bytes.
    let planes: Vec<Vec<f32>> = (0..3_usize)
        .into_par_iter()
        .map(|c| {
            let mut plane = Vec::with_capacity((w as usize) * (h as usize));
            for y in 0..h {
                for x in 0..w {
                    plane.push(rgb.get_pixel(x, y).0[c] as f32 / 255.0);
                }
            }
            plane
        })
        .collect();

    let mut out = Vec::with_capacity((w as usize) * (h as usize) * 3 * std::mem::size_of::<f32>());
    for plane in planes {
        for v in plane {
            out.extend_from_slice(&v.to_le_bytes());
        }
    }
    Ok((out, h, w))
}

pub(crate) fn resize_keep_ratio(img: image::DynamicImage, target_edge: u32) -> image::DynamicImage {
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
    img.resize_exact(nw, nh, image::imageops::FilterType::Lanczos3)
}

#[cfg(test)]
mod tests {
    use super::image_to_pixel_values_nchw_f32;
    use image::{ImageBuffer, ImageFormat, Rgb};
    use std::io::Cursor;

    #[test]
    fn preprocess_image_outputs_pixel_values_payload() {
        let img =
            image::DynamicImage::ImageRgb8(ImageBuffer::<Rgb<u8>, _>::from_fn(2, 2, |_, _| {
                Rgb([255, 128, 0])
            }));
        let mut encoded = Vec::new();
        img.write_to(&mut Cursor::new(&mut encoded), ImageFormat::Png)
            .expect("png encode");

        let (out, h, w) = image_to_pixel_values_nchw_f32(&encoded, 2).expect("preprocess");
        assert_eq!((h, w), (2, 2));
        assert_eq!(out.len(), 2 * 2 * 3 * 4);
    }
}
