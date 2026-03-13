use anyhow::{anyhow, bail, Result};
use rav1e::prelude::{
    ChromaSampling, ColorDescription, ColorPrimaries, Config, EncoderConfig, MatrixCoefficients,
    PixelRange, Rational, SpeedSettings, TransferCharacteristics,
};

use crate::media_codec::{VideoEncoder, VideoSessionConfig};
use crate::net::video_frame::{EncodedAccessUnit, FramePlanes, PixelFormat, VideoFrame};
use crate::proto::voiceplatform::v1 as pb;
use crate::screen_share::config::SenderPolicy;
use crate::screen_share::runtime_probe::EncodeBackendKind;

pub fn build_av1_encoder(
    backends: &[EncodeBackendKind],
    policy: SenderPolicy,
) -> Result<Box<dyn VideoEncoder>> {
    for backend in backends {
        match backend {
            EncodeBackendKind::SvtAv1 if cfg!(feature = "video-av1-software") => {
                return Ok(Box::new(Av1RealtimeEncoder::new(Av1Backend::Rav1e)))
            }
            _ => continue,
        }
    }

    if matches!(policy, SenderPolicy::AutoLowLatency) {
        bail!("interactive AV1 encode requires the `video-av1-software` feature")
    }

    Err(anyhow!("no AV1 encoder backend available"))
}

#[derive(Clone, Copy)]
enum Av1Backend {
    Rav1e,
}

pub struct Av1RealtimeEncoder {
    backend: Av1Backend,
    frame_seq: u32,
    force_next_keyframe: bool,
    config: VideoSessionConfig,
    active_signature: Option<(u32, u32, u32, u32)>,
    software: Option<SoftwareAv1Encoder>,
}

impl Av1RealtimeEncoder {
    fn new(backend: Av1Backend) -> Self {
        Self {
            backend,
            frame_seq: 0,
            force_next_keyframe: false,
            config: VideoSessionConfig {
                width: 0,
                height: 0,
                fps: 30,
                target_bitrate_bps: 2_000_000,
                low_latency: true,
                allow_frame_drop: true,
            },
            active_signature: None,
            software: None,
        }
    }

    fn ensure_software_ready(&mut self, frame: &VideoFrame) -> Result<()> {
        let signature = (
            frame.width,
            frame.height,
            self.config.fps.max(1),
            self.config.target_bitrate_bps.max(100_000),
        );

        if self.active_signature == Some(signature) && self.software.is_some() {
            return Ok(());
        }

        self.software = Some(SoftwareAv1Encoder::new(
            signature.0,
            signature.1,
            signature.2,
            signature.3,
        )?);
        self.active_signature = Some(signature);
        Ok(())
    }
}

impl VideoEncoder for Av1RealtimeEncoder {
    fn configure_session(&mut self, config: VideoSessionConfig) -> Result<()> {
        self.config = config;
        self.active_signature = None;
        self.software = None;
        Ok(())
    }

    fn request_keyframe(&mut self) -> Result<()> {
        self.force_next_keyframe = true;
        Ok(())
    }

    fn update_bitrate(&mut self, bitrate_bps: u32) -> Result<()> {
        self.config.target_bitrate_bps = bitrate_bps;
        self.active_signature = None;
        Ok(())
    }

    fn encode(&mut self, frame: VideoFrame) -> Result<EncodedAccessUnit> {
        if frame.format != PixelFormat::Bgra {
            return Err(anyhow!("AV1 encoder currently expects BGRA input"));
        }

        let force_keyframe = self.force_next_keyframe;
        self.force_next_keyframe = false;

        self.ensure_software_ready(&frame)?;
        let encoded = self
            .software
            .as_mut()
            .expect("initialized above")
            .encode(&frame, force_keyframe)?;

        let seq = self.frame_seq;
        self.frame_seq = self.frame_seq.wrapping_add(1);

        Ok(EncodedAccessUnit {
            codec: pb::VideoCodec::Av1,
            layer_id: 0,
            ts_ms: frame.ts_ms,
            is_keyframe: force_keyframe || seq % 120 == 0,
            data: bytes::Bytes::from(encoded),
        })
    }

    fn backend_name(&self) -> &'static str {
        match self.backend {
            Av1Backend::Rav1e => "av1-rav1e",
        }
    }
}

struct SoftwareAv1Encoder {
    ctx: rav1e::prelude::Context<u8>,
}

impl SoftwareAv1Encoder {
    fn new(width: u32, height: u32, fps: u32, bitrate_bps: u32) -> Result<Self> {
        let mut enc = EncoderConfig::default();
        enc.width = width as usize;
        enc.height = height as usize;
        enc.speed_settings = SpeedSettings::from_preset(10);
        enc.time_base = Rational::new(1, fps as u64);
        enc.bitrate = (bitrate_bps / 1000) as i32;
        enc.min_key_frame_interval = 30;
        enc.max_key_frame_interval = 120;
        enc.low_latency = true;
        enc.chroma_sampling = ChromaSampling::Cs420;
        enc.pixel_range = PixelRange::Limited;
        enc.color_description = Some(ColorDescription {
            color_primaries: ColorPrimaries::BT709,
            transfer_characteristics: TransferCharacteristics::BT709,
            matrix_coefficients: MatrixCoefficients::BT709,
        });

        let cfg = Config::new()
            .with_encoder_config(enc)
            .with_threads(1)
            .with_parallel_gops(false);
        let ctx = cfg
            .new_context()
            .map_err(|e| anyhow!("failed to initialize AV1 encoder context: {e}"))?;
        Ok(Self { ctx })
    }

    fn encode(&mut self, frame: &VideoFrame, force_keyframe: bool) -> Result<Vec<u8>> {
        let mut yuv = self.ctx.new_frame();
        bgra_to_i420(frame, &mut yuv)?;
        self.ctx
            .send_frame(Some(yuv))
            .map_err(|e| anyhow!("failed to submit frame to AV1 encoder: {e}"))?;

        if force_keyframe {
            self.ctx
                .flush()
                .map_err(|e| anyhow!("failed to flush encoder for keyframe request: {e}"))?;
        }

        loop {
            match self.ctx.receive_packet() {
                Ok(pkt) => return Ok(pkt.data),
                Err(rav1e::prelude::EncoderStatus::NeedMoreData) => continue,
                Err(e) => return Err(anyhow!("AV1 encoding failed: {e}")),
            }
        }
    }
}

fn bgra_to_i420(frame: &VideoFrame, yuv: &mut rav1e::prelude::Frame<u8>) -> Result<()> {
    let FramePlanes::Bgra { bytes, stride } = &frame.planes else {
        return Err(anyhow!("AV1 encoder plane mismatch"));
    };

    let width = frame.width as usize;
    let height = frame.height as usize;
    let stride = *stride as usize;

    let y_stride = yuv.planes[0].cfg.stride;
    let u_stride = yuv.planes[1].cfg.stride;
    let v_stride = yuv.planes[2].cfg.stride;

    for y in 0..height {
        let src_row = &bytes[y * stride..y * stride + (width * 4)];
        let dst_row = &mut yuv.planes[0].data_origin_mut()[y * y_stride..y * y_stride + width];
        for x in 0..width {
            let b = src_row[x * 4] as f32;
            let g = src_row[x * 4 + 1] as f32;
            let r = src_row[x * 4 + 2] as f32;
            let yv = (0.257 * r + 0.504 * g + 0.098 * b + 16.0)
                .round()
                .clamp(0.0, 255.0);
            dst_row[x] = yv as u8;
        }
    }

    for y in 0..(height / 2) {
        for x in 0..(width / 2) {
            let mut r_sum = 0.0;
            let mut g_sum = 0.0;
            let mut b_sum = 0.0;
            for oy in 0..2 {
                for ox in 0..2 {
                    let px = x * 2 + ox;
                    let py = y * 2 + oy;
                    let idx = py * stride + px * 4;
                    b_sum += bytes[idx] as f32;
                    g_sum += bytes[idx + 1] as f32;
                    r_sum += bytes[idx + 2] as f32;
                }
            }
            let r = r_sum / 4.0;
            let g = g_sum / 4.0;
            let b = b_sum / 4.0;
            let u = (-0.148 * r - 0.291 * g + 0.439 * b + 128.0)
                .round()
                .clamp(0.0, 255.0) as u8;
            let v = (0.439 * r - 0.368 * g - 0.071 * b + 128.0)
                .round()
                .clamp(0.0, 255.0) as u8;
            yuv.planes[1].data_origin_mut()[y * u_stride + x] = u;
            yuv.planes[2].data_origin_mut()[y * v_stride + x] = v;
        }
    }

    Ok(())
}
