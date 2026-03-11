use bytes::Bytes;

#[derive(Debug, Clone, Copy)]
pub enum PixelFormat {
    Bgra,
    Nv12,
}

#[derive(Debug, Clone)]
pub struct VideoPlane {
    pub stride: u32,
    pub data: Bytes,
}

#[derive(Debug, Clone)]
pub struct VideoFrame {
    pub width: u32,
    pub height: u32,
    pub ts_ms: u32,
    pub format: PixelFormat,
    pub planes: Vec<VideoPlane>,
}

#[derive(Debug)]
pub struct EncodedFrame {
    pub ts_ms: u32,
    pub is_keyframe: bool,
    pub data: Bytes,
}
