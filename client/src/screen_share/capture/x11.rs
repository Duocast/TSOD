use anyhow::{anyhow, Context};
use bytes::Bytes;

use crate::media_capture::CaptureBackend;
use crate::net::video_frame::{FramePlanes, PixelFormat, VideoFrame};

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use std::time::{Duration, Instant};
    use x11rb::connection::Connection;
    use x11rb::protocol::composite::{ConnectionExt as CompositeExt, Redirect};
    use x11rb::protocol::damage::{ConnectionExt as DamageExt, ReportLevel};
    use x11rb::protocol::randr::ConnectionExt as RandrExt;
    use x11rb::protocol::xproto::{ConnectionExt as XprotoExt, ImageFormat, Window};
    use x11rb::rust_connection::RustConnection;

    pub struct X11Capture {
        conn: RustConnection,
        root: Window,
        window_id: Window,
        drawable: Drawable,
        geometry: WindowGeometry,
        monitor: Option<MonitorGeometry>,
        reusable_bgra: Vec<u8>,
        has_damage: bool,
        last_geometry_poll: Instant,
    }

    #[derive(Clone, Copy, Debug)]
    enum Drawable {
        Window(Window),
        Pixmap(u32),
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct WindowGeometry {
        x: i32,
        y: i32,
        width: u32,
        height: u32,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct MonitorGeometry {
        x: i32,
        y: i32,
        width: u32,
        height: u32,
    }

    impl X11Capture {
        pub fn from_source(source: &crate::ShareSource) -> anyhow::Result<Self> {
            let crate::ShareSource::X11Window(window_id) = source else {
                return Err(anyhow!(
                    "x11 capture requires ShareSource::X11Window (dedicated X11 path)"
                ));
            };

            if std::env::var_os("DISPLAY").is_none() {
                return Err(anyhow!(
                    "x11 capture requested but DISPLAY is not set; verify X11 session and permissions"
                ));
            }

            let (conn, screen_num) = RustConnection::connect(None).context("connect to X11")?;
            let root = conn
                .setup()
                .roots
                .get(screen_num)
                .ok_or_else(|| anyhow!("invalid X11 screen index: {screen_num}"))?
                .root;
            let window_id = *window_id as Window;

            let geometry = query_geometry(&conn, root, window_id)
                .context("query initial X11 window geometry")?;
            let monitor = map_window_to_monitor(&conn, root, geometry);

            let mut drawable = Drawable::Window(window_id);
            if has_extension(&conn, b"Composite")? {
                let _ = conn
                    .composite_query_version(0, 4)
                    .context("query XComposite version")?;
                conn.composite_redirect_window(window_id, Redirect::MANUAL)
                    .context("redirect X11 window with XComposite")?;
                let pixmap_id = conn.generate_id().context("allocate X11 pixmap id")?;
                conn.composite_name_window_pixmap(window_id, pixmap_id)
                    .context("name X11 window pixmap")?;
                conn.flush().ok();
                drawable = Drawable::Pixmap(pixmap_id);
            }

            let has_damage = if has_extension(&conn, b"DAMAGE")? {
                let _ = conn.damage_query_version(1, 1).ok();
                if let Ok(damage_id) = conn.generate_id() {
                    let _ = conn.damage_create(damage_id, window_id, ReportLevel::NON_EMPTY);
                    conn.flush().ok();
                    true
                } else {
                    false
                }
            } else {
                false
            };

            Ok(Self {
                conn,
                root,
                window_id,
                drawable,
                geometry,
                monitor,
                reusable_bgra: Vec::new(),
                has_damage,
                last_geometry_poll: Instant::now(),
            })
        }

        fn refresh_geometry_if_needed(&mut self) {
            if self.last_geometry_poll.elapsed() < Duration::from_millis(200) {
                return;
            }
            self.last_geometry_poll = Instant::now();

            if let Ok(new_geom) = query_geometry(&self.conn, self.root, self.window_id) {
                if geometry_changed(self.geometry, new_geom) {
                    self.geometry = new_geom;
                    self.monitor = map_window_to_monitor(&self.conn, self.root, new_geom);
                    if let Drawable::Pixmap(old_pixmap) = self.drawable {
                        let _ = self.conn.free_pixmap(old_pixmap);
                        if let Ok(new_pixmap) = self.conn.generate_id() {
                            if self
                                .conn
                                .composite_name_window_pixmap(self.window_id, new_pixmap)
                                .is_ok()
                            {
                                self.drawable = Drawable::Pixmap(new_pixmap);
                            }
                        }
                        self.conn.flush().ok();
                    }
                }
            }
        }

        fn capture_image(&self) -> anyhow::Result<x11rb::protocol::xproto::GetImageReply> {
            let drawable = match self.drawable {
                Drawable::Window(id) => id,
                Drawable::Pixmap(id) => id,
            };
            let request = self.conn.get_image(
                ImageFormat::Z_PIXMAP,
                drawable,
                0,
                0,
                self.geometry.width.max(1) as u16,
                self.geometry.height.max(1) as u16,
                u32::MAX,
            );

            match request.and_then(|cookie| cookie.reply()) {
                Ok(reply) => Ok(reply),
                Err(e) => {
                    if matches!(self.drawable, Drawable::Pixmap(_)) {
                        let fallback = self
                            .conn
                            .get_image(
                                ImageFormat::Z_PIXMAP,
                                self.window_id,
                                0,
                                0,
                                self.geometry.width.max(1) as u16,
                                self.geometry.height.max(1) as u16,
                                u32::MAX,
                            )
                            .context("request X11 window image fallback")?
                            .reply()
                            .context("decode X11 window image fallback")?;
                        Ok(fallback)
                    } else {
                        Err(anyhow!(e)).context("capture X11 image")
                    }
                }
            }
        }
    }

    impl CaptureBackend for X11Capture {
        fn next_frame(&mut self) -> anyhow::Result<VideoFrame> {
            self.refresh_geometry_if_needed();
            let image = self.capture_image()?;
            reconfigure_buffer(
                &mut self.reusable_bgra,
                self.geometry.width.max(1),
                self.geometry.height.max(1),
            );
            normalize_x11_pixels(
                &image.data,
                self.geometry.width.max(1),
                self.geometry.height.max(1),
                &mut self.reusable_bgra,
            )?;

            if self.has_damage {
                while self.conn.poll_for_event().ok().flatten().is_some() {}
            }

            Ok(VideoFrame {
                width: self.geometry.width.max(1),
                height: self.geometry.height.max(1),
                ts_ms: unix_ms() as u32,
                format: PixelFormat::Bgra,
                planes: FramePlanes::Bgra {
                    bytes: Bytes::copy_from_slice(&self.reusable_bgra),
                    stride: self.geometry.width.max(1) * 4,
                },
            })
        }

        fn backend_name(&self) -> &'static str {
            "x11-window"
        }

        fn native_format(&self) -> PixelFormat {
            PixelFormat::Bgra
        }
    }

    fn has_extension(conn: &RustConnection, ext: &[u8]) -> anyhow::Result<bool> {
        Ok(conn
            .query_extension(ext)
            .context("query X11 extension")?
            .reply()
            .context("read X11 extension reply")?
            .present)
    }

    fn query_geometry(
        conn: &RustConnection,
        root: Window,
        window_id: Window,
    ) -> anyhow::Result<WindowGeometry> {
        let geo = conn
            .get_geometry(window_id)
            .context("request X11 geometry")?
            .reply()
            .context("read X11 geometry")?;
        let translated = conn
            .translate_coordinates(window_id, root, 0, 0)
            .context("request X11 coordinate translation")?
            .reply()
            .context("read X11 coordinate translation")?;

        Ok(WindowGeometry {
            x: translated.dst_x as i32,
            y: translated.dst_y as i32,
            width: geo.width as u32,
            height: geo.height as u32,
        })
    }

    fn map_window_to_monitor(
        conn: &RustConnection,
        root: Window,
        window: WindowGeometry,
    ) -> Option<MonitorGeometry> {
        let monitors = conn
            .randr_get_monitors(root, true)
            .ok()?
            .reply()
            .ok()?
            .monitors;

        let mut best: Option<(u32, MonitorGeometry)> = None;
        for monitor in monitors {
            let candidate = MonitorGeometry {
                x: monitor.x as i32,
                y: monitor.y as i32,
                width: monitor.width as u32,
                height: monitor.height as u32,
            };
            let overlap = intersection_area(window, candidate);
            match best {
                Some((best_overlap, _)) if overlap <= best_overlap => {}
                _ => best = Some((overlap, candidate)),
            }
        }

        best.and_then(|(overlap, monitor)| if overlap > 0 { Some(monitor) } else { None })
    }

    fn intersection_area(window: WindowGeometry, monitor: MonitorGeometry) -> u32 {
        let left = window.x.max(monitor.x);
        let top = window.y.max(monitor.y);
        let right = (window.x + window.width as i32).min(monitor.x + monitor.width as i32);
        let bottom = (window.y + window.height as i32).min(monitor.y + monitor.height as i32);
        if right <= left || bottom <= top {
            0
        } else {
            (right - left) as u32 * (bottom - top) as u32
        }
    }

    fn geometry_changed(old: WindowGeometry, new: WindowGeometry) -> bool {
        old != new
    }

    fn reconfigure_buffer(buffer: &mut Vec<u8>, width: u32, height: u32) {
        let needed = (width * height * 4) as usize;
        if buffer.len() != needed {
            buffer.resize(needed, 0);
        }
    }

    fn normalize_x11_pixels(
        src: &[u8],
        width: u32,
        height: u32,
        dst_bgra: &mut [u8],
    ) -> anyhow::Result<()> {
        let out_stride = width as usize * 4;
        let expected = out_stride * height as usize;
        if dst_bgra.len() != expected {
            return Err(anyhow!("invalid destination buffer for X11 conversion"));
        }
        if src.is_empty() {
            return Err(anyhow!("X11 returned empty image data"));
        }

        let src_stride = src.len() / height.max(1) as usize;
        if src_stride >= out_stride {
            for row in 0..height as usize {
                let src_off = row * src_stride;
                let dst_off = row * out_stride;
                let row_src = &src[src_off..src_off + out_stride];
                let row_dst = &mut dst_bgra[dst_off..dst_off + out_stride];
                row_dst.copy_from_slice(row_src);
                for px in row_dst.chunks_exact_mut(4) {
                    px[3] = 0xFF;
                }
            }
            return Ok(());
        }

        if src_stride == width as usize * 3 {
            for row in 0..height as usize {
                let src_off = row * src_stride;
                let dst_off = row * out_stride;
                for col in 0..width as usize {
                    let s = src_off + col * 3;
                    let d = dst_off + col * 4;
                    dst_bgra[d] = src[s];
                    dst_bgra[d + 1] = src[s + 1];
                    dst_bgra[d + 2] = src[s + 2];
                    dst_bgra[d + 3] = 0xFF;
                }
            }
            return Ok(());
        }

        Err(anyhow!(
            "unsupported X11 pixel layout: src_len={} width={} height={}",
            src.len(),
            width,
            height
        ))
    }

    fn unix_ms() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn reconfigure_buffer_resizes_only_on_shape_change() {
            let mut buffer = vec![1_u8; 16];
            reconfigure_buffer(&mut buffer, 2, 2);
            assert_eq!(buffer.len(), 16);
            reconfigure_buffer(&mut buffer, 3, 2);
            assert_eq!(buffer.len(), 24);
        }

        #[test]
        fn picks_monitor_with_largest_overlap() {
            let window = WindowGeometry {
                x: 100,
                y: 100,
                width: 500,
                height: 400,
            };
            let a = MonitorGeometry {
                x: 0,
                y: 0,
                width: 300,
                height: 300,
            };
            let b = MonitorGeometry {
                x: 300,
                y: 0,
                width: 800,
                height: 800,
            };
            assert!(intersection_area(window, b) > intersection_area(window, a));
        }

        #[test]
        fn detects_geometry_change() {
            let a = WindowGeometry {
                x: 1,
                y: 2,
                width: 3,
                height: 4,
            };
            let b = WindowGeometry {
                x: 1,
                y: 2,
                width: 3,
                height: 5,
            };
            assert!(geometry_changed(a, b));
            assert!(!geometry_changed(a, a));
        }

        #[test]
        fn normalizes_24bit_rows_to_bgra() {
            let src = [
                1_u8, 2, 3, 4, 5, 6, // two 24-bit pixels
            ];
            let mut dst = vec![0_u8; 8];
            normalize_x11_pixels(&src, 2, 1, &mut dst).unwrap();
            assert_eq!(dst, vec![1, 2, 3, 255, 4, 5, 6, 255]);
        }
    }
}

#[cfg(target_os = "linux")]
pub use linux::X11Capture;

#[cfg(not(target_os = "linux"))]
pub struct X11Capture;

#[cfg(not(target_os = "linux"))]
impl X11Capture {
    pub fn from_source(_source: &crate::ShareSource) -> anyhow::Result<Self> {
        Err(anyhow!("X11 capture backend is only available on Linux"))
    }
}

#[cfg(not(target_os = "linux"))]
impl CaptureBackend for X11Capture {
    fn next_frame(&mut self) -> anyhow::Result<VideoFrame> {
        Err(anyhow!("X11 capture backend is only available on Linux"))
    }

    fn backend_name(&self) -> &'static str {
        "x11-window"
    }

    fn native_format(&self) -> PixelFormat {
        PixelFormat::Bgra
    }
}
