use crate::error::{GatewayError, Result};
use crate::media::MediaPayload;

#[derive(Debug, Clone, Copy)]
pub struct VideoSamplePolicy {
    pub max_frames: usize,
    pub frame_interval: usize,
}

impl Default for VideoSamplePolicy {
    fn default() -> Self {
        Self {
            max_frames: 8,
            frame_interval: 2,
        }
    }
}

#[derive(Debug, Clone)]
pub struct VideoPreprocessOutput {
    pub payload: MediaPayload,
    pub sampled_frames: usize,
}

pub fn preprocess_video(
    payload: MediaPayload,
    policy: VideoSamplePolicy,
) -> Result<VideoPreprocessOutput> {
    if policy.max_frames == 0 {
        return Err(GatewayError::Internal(
            "video preprocess policy invalid: max_frames must be > 0".to_string(),
        ));
    }
    if policy.frame_interval == 0 {
        return Err(GatewayError::Internal(
            "video preprocess policy invalid: frame_interval must be > 0".to_string(),
        ));
    }

    // Current lightweight path keeps raw payload as-is.
    // This module boundary allows future ffmpeg/decord frame sampling acceleration
    // without touching the orchestration layer.
    Ok(VideoPreprocessOutput {
        payload,
        sampled_frames: 0,
    })
}
