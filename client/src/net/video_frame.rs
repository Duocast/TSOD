use bytes::Bytes;

use crate::proto::voiceplatform::v1 as pb;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    Bgra,
    Nv12,
    I420,
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
    I420 {
        y: Bytes,
        u: Bytes,
        v: Bytes,
        y_stride: u32,
        u_stride: u32,
        v_stride: u32,
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
    /// True when this access unit is decoder-refreshing and can be used as the
    /// first frame after decoder reset/loss recovery.
    pub is_keyframe: bool,
    /// One complete decodable access unit / display frame in codec bitstream
    /// form. Codec config OBUs/headers are carried in-band on keyframes.
    pub data: Bytes,
}
