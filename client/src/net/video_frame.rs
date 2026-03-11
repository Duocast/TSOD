use bytes::Bytes;

use crate::proto::voiceplatform::v1 as pb;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    Bgra,
    Nv12,
}

#[derive(Debug, Clone)]
pub enum FramePlanes {
    Bgra {
        bytes: Bytes,
        stride: u32,
    },
    Nv12 {
        y: Bytes,
        uv: Bytes,
        y_stride: u32,
        uv_stride: u32,
    },
}

#[derive(Debug, Clone)]
pub struct VideoFrame {
    pub width: u32,
    pub height: u32,
    pub ts_ms: u32,
    pub format: PixelFormat,
    pub planes: FramePlanes,
}

#[derive(Debug, Clone)]
pub struct EncodedAccessUnit {
    pub codec: pb::VideoCodec,
    pub layer_id: u8,
    pub ts_ms: u32,
    pub is_keyframe: bool,
    pub data: Bytes,
}
