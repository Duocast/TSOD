use anyhow::{anyhow, Context};
use bytes::Bytes;
use pipewire as pw;
use pw::properties::properties;

use crate::media_capture::CaptureBackend;
use crate::net::video_frame::{FramePlanes, PixelFormat, VideoFrame};

pub struct PipewirePortalCapture {
    rx: std::sync::mpsc::Receiver<anyhow::Result<VideoFrame>>,
    backend_name: &'static str,
}

impl PipewirePortalCapture {
    pub fn from_source(source: &crate::ShareSource) -> anyhow::Result<Self> {
        let crate::ShareSource::LinuxPortal(token) = source else {
            return Err(anyhow!(
                "pipewire portal capture requires ShareSource::LinuxPortal"
            ));
        };

        let target = resolve_pipewire_target(token)?;
        let (tx, rx) = std::sync::mpsc::sync_channel(2);
        std::thread::Builder::new()
            .name("tsod-pipewire-video-capture".to_string())
            .spawn(move || run_pipewire_capture(target, tx))
            .context("spawn pipewire capture thread")?;

        Ok(Self {
            rx,
            backend_name: "pipewire-portal",
        })
    }
}

impl CaptureBackend for PipewirePortalCapture {
    fn next_frame(&mut self) -> anyhow::Result<VideoFrame> {
        self.rx
            .recv_timeout(std::time::Duration::from_millis(500))
            .map_err(|_| anyhow!("timeout waiting for PipeWire frame"))?
    }

    fn backend_name(&self) -> &'static str {
        self.backend_name
    }

    fn native_format(&self) -> PixelFormat {
        PixelFormat::Bgra
    }
}

fn resolve_pipewire_target(token: &str) -> anyhow::Result<Option<u32>> {
    if token == "portal-picker" {
        if let Ok(node) = std::env::var("VP_PORTAL_NODE_ID") {
            let id = node
                .parse::<u32>()
                .map_err(|_| anyhow!("VP_PORTAL_NODE_ID must be a u32"))?;
            return Ok(Some(id));
        }
        return Err(anyhow!(
            "portal-picker selected but no node id found; set VP_PORTAL_NODE_ID from xdg-desktop-portal ScreenCast response"
        ));
    }

    if let Some(raw) = token.strip_prefix("node-") {
        let id = raw
            .parse::<u32>()
            .map_err(|_| anyhow!("invalid node id token: {token}"))?;
        return Ok(Some(id));
    }

    Ok(None)
}

fn run_pipewire_capture(
    target: Option<u32>,
    tx: std::sync::mpsc::SyncSender<anyhow::Result<VideoFrame>>,
) {
    if let Err(err) = run_pipewire_capture_inner(target, tx.clone()) {
        let _ = tx.send(Err(err));
    }
}

fn run_pipewire_capture_inner(
    target: Option<u32>,
    tx: std::sync::mpsc::SyncSender<anyhow::Result<VideoFrame>>,
) -> anyhow::Result<()> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopBox::new(None).context("create PipeWire mainloop")?;
    let context =
        pw::context::ContextBox::new(mainloop.loop_(), None).context("create PipeWire context")?;
    let core = context.connect(None).context("connect PipeWire core")?;

    let stream = pw::stream::StreamBox::new(
        &core,
        "tsod-screen-capture",
        properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        },
    )
    .context("create PipeWire video stream")?;

    let listener = stream
        .add_local_listener_with_user_data(())
        .process(move |stream, _| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let datas = buffer.datas_mut();
            if datas.is_empty() {
                return;
            }
            let chunk = datas[0].chunk();
            let size = chunk.size() as usize;
            let offset = chunk.offset() as usize;
            let stride = chunk.stride().unsigned_abs();
            let Some(data) = datas[0].data() else {
                return;
            };
            if offset + size > data.len() {
                return;
            }
            let payload = &data[offset..offset + size];
            let width = 1920;
            let height = ((size as u32) / stride.max(1)).max(1);
            let frame = VideoFrame {
                width,
                height,
                ts_ms: unix_ms() as u32,
                format: PixelFormat::Bgra,
                planes: FramePlanes::Bgra {
                    bytes: Bytes::copy_from_slice(payload),
                    stride: stride.max(1),
                },
            };
            let _ = tx.try_send(Ok(frame));
        })
        .register()
        .context("register PipeWire listener")?;

    let obj = pw::spa::pod::object!(
        pw::spa::utils::SpaTypes::ObjectParamFormat,
        pw::spa::param::ParamType::EnumFormat,
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::MediaType,
            Id,
            pw::spa::param::format::MediaType::Video
        ),
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::MediaSubtype,
            Id,
            pw::spa::param::format::MediaSubtype::Raw
        ),
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            pw::spa::param::video::VideoFormat::BGRx,
            pw::spa::param::video::VideoFormat::BGRx,
            pw::spa::param::video::VideoFormat::RGBx
        ),
    );
    let values: Vec<u8> = pw::spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &pw::spa::pod::Value::Object(obj),
    )
    .map_err(|_| anyhow!("serialize PipeWire format"))?
    .0
    .into_inner();
    let mut params = [pw::spa::pod::Pod::from_bytes(&values)
        .ok_or_else(|| anyhow!("build PipeWire format pod"))?];

    stream
        .connect(
            pw::spa::utils::Direction::Input,
            target,
            pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
            &mut params,
        )
        .context("connect PipeWire stream")?;

    let _listener = listener;
    loop {
        let _ = mainloop
            .loop_()
            .iterate(std::time::Duration::from_millis(50));
    }
}

fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::resolve_pipewire_target;

    #[test]
    fn parses_explicit_node_token() {
        assert_eq!(resolve_pipewire_target("node-42").unwrap(), Some(42));
    }
}
