//! Read-only, nonblocking v4l2loopback frames and local QR decoding.
use gtk4::glib;
use linux_video::{
    types::{BufferType, ContentType, In, Mmap, PixFormat},
    Device, Stream,
};
use std::os::unix::io::AsRawFd;

const MAX_RAW: usize = 32 * 1024 * 1024;
const MAX_PIXELS: usize = 960 * 720;

#[derive(Clone, Debug)]
pub struct QrLink {
    pub url: String,
    pub bounds: [(f64, f64); 4],
}

pub struct Frame {
    pub width: usize,
    pub height: usize,
    pub rgb: Vec<u8>,
    pub links: Vec<QrLink>,
}

fn web_url(text: &str) -> Option<String> {
    if text.len() > 4096 || text.chars().any(char::is_control) {
        return None;
    }
    let text = text.trim();
    let uri = glib::Uri::parse(text, glib::UriFlags::NONE).ok()?;
    if !matches!(uri.scheme().as_str(), "http" | "https")
        || uri.host().is_none_or(|host| host.is_empty())
        || uri.userinfo().is_some()
    {
        return None;
    }
    Some(text.to_owned())
}

pub fn scan_rgb(width: usize, height: usize, rgb: Vec<u8>) -> Result<Frame, String> {
    let pixels = width
        .checked_mul(height)
        .filter(|n| *n > 0 && *n <= MAX_PIXELS)
        .ok_or("Invalid preview dimensions")?;
    if rgb.len() != pixels * 3 {
        return Err("Incomplete preview frame".into());
    }
    let mut image = rqrr::PreparedImage::prepare_from_greyscale(width, height, |x, y| {
        let i = (y * width + x) * 3;
        ((u32::from(rgb[i]) * 77 + u32::from(rgb[i + 1]) * 150 + u32::from(rgb[i + 2]) * 29) >> 8)
            as u8
    });
    let links = image
        .detect_grids()
        .into_iter()
        .filter_map(|grid| {
            let (_, text) = grid.decode().ok()?;
            Some(QrLink {
                url: web_url(&text)?,
                bounds: grid.bounds.map(|p| (f64::from(p.x), f64::from(p.y))),
            })
        })
        .collect();
    Ok(Frame {
        width,
        height,
        rgb,
        links,
    })
}

#[derive(Clone, Copy)]
struct Layout {
    width: usize,
    height: usize,
    stride: usize,
    fourcc: [u8; 4],
}
impl Layout {
    fn required(self) -> Result<usize, String> {
        let Self {
            width: w,
            height: h,
            stride: s,
            fourcc: code,
        } = self;
        if w == 0 || h == 0 || w > 8192 || h > 8192 {
            return Err("Invalid camera dimensions".into());
        }
        let min_stride = match &code {
            b"YU12" | b"YV12" | b"NV12" | b"NV21" => w,
            b"YUYV" | b"UYVY" => w.div_ceil(2) * 4,
            b"RGB3" | b"BGR3" => w * 3,
            _ => {
                return Err(format!(
                    "Preview does not support camera format {}",
                    String::from_utf8_lossy(&code)
                ))
            }
        };
        if s < min_stride || s > MAX_RAW / h {
            return Err("Invalid camera stride".into());
        }
        let size = match &code {
            b"YU12" | b"YV12" => s * h + 2 * s.div_ceil(2) * h.div_ceil(2),
            b"NV12" | b"NV21" => {
                if s < w.div_ceil(2) * 2 {
                    return Err("Invalid chroma stride".into());
                }
                s * h + s * h.div_ceil(2)
            }
            _ => s * h,
        };
        if size > MAX_RAW {
            return Err("Camera frame exceeds preview size limit".into());
        }
        Ok(size)
    }

    fn convert(self, bytes: &[u8]) -> Result<Frame, String> {
        if bytes.len() < self.required()? {
            return Err("Incomplete camera frame".into());
        }
        let Self {
            width: w,
            height: h,
            stride: s,
            fourcc: code,
        } = self;
        let scale = (960.0 / w as f64).min(720.0 / h as f64).min(1.0);
        let width = ((w as f64 * scale) as usize).max(1);
        let height = ((h as f64 * scale) as usize).max(1);
        let mut rgb = Vec::with_capacity(width * height * 3);
        for y in 0..height {
            let sy = y * h / height;
            for x in 0..width {
                let sx = x * w / width;
                let color = match &code {
                    b"RGB3" | b"BGR3" => {
                        let p = sy * s + sx * 3;
                        if code == *b"RGB3" {
                            [bytes[p], bytes[p + 1], bytes[p + 2]]
                        } else {
                            [bytes[p + 2], bytes[p + 1], bytes[p]]
                        }
                    }
                    _ => {
                        let (luma, u, v) = match &code {
                            b"YU12" | b"YV12" => {
                                let chroma_stride = s.div_ceil(2);
                                let plane = chroma_stride * h.div_ceil(2);
                                let p = s * h + (sy / 2) * chroma_stride + sx / 2;
                                let (u, v) = if code == *b"YU12" {
                                    (bytes[p], bytes[p + plane])
                                } else {
                                    (bytes[p + plane], bytes[p])
                                };
                                (bytes[sy * s + sx], u, v)
                            }
                            b"NV12" | b"NV21" => {
                                let p = s * h + (sy / 2) * s + (sx / 2) * 2;
                                let (u, v) = if code == *b"NV12" {
                                    (bytes[p], bytes[p + 1])
                                } else {
                                    (bytes[p + 1], bytes[p])
                                };
                                (bytes[sy * s + sx], u, v)
                            }
                            _ => {
                                let p = sy * s + (sx / 2) * 4;
                                if code == *b"YUYV" {
                                    (bytes[p + (sx % 2) * 2], bytes[p + 1], bytes[p + 3])
                                } else {
                                    (bytes[p + 1 + (sx % 2) * 2], bytes[p], bytes[p + 2])
                                }
                            }
                        };
                        let c = (i32::from(luma) - 16).max(0);
                        let d = i32::from(u) - 128;
                        let e = i32::from(v) - 128;
                        let clamp = |v: i32| ((v + 128) >> 8).clamp(0, 255) as u8;
                        [
                            clamp(298 * c + 409 * e),
                            clamp(298 * c - 100 * d - 208 * e),
                            clamp(298 * c + 516 * d),
                        ]
                    }
                };
                rgb.extend_from_slice(&color);
            }
        }
        scan_rgb(width, height, rgb)
    }
}

/// Reads frames with the mmap streaming API. `read()` I/O on v4l2loopback holds
/// the device exclusively, which would stop other apps from using the webcam.
pub struct VideoReader {
    stream: Stream<In, Mmap>,
    layout: Layout,
}
impl VideoReader {
    pub fn open(path: &str) -> Result<Self, String> {
        let device = Device::open(path).map_err(|e| format!("Cannot open camera preview: {e}"))?;
        let format = device
            .format(BufferType::VideoCapture)
            .map_err(|e| format!("Cannot read camera format: {e}"))?;
        let pix = format
            .try_ref::<PixFormat>()
            .ok_or("Camera is not a video capture device")?;
        let code = *AsRef::<[u8; 4]>::as_ref(&pix.pixel_format());
        let min_stride = match &code {
            b"RGB3" | b"BGR3" => pix.width() * 3,
            b"YUYV" | b"UYVY" => pix.width().div_ceil(2) * 4,
            _ => pix.width(),
        };
        let layout = Layout {
            width: pix.width() as usize,
            height: pix.height() as usize,
            stride: pix.bytes_per_line().max(min_stride) as usize,
            fourcc: code,
        };
        if layout.required()?.max(pix.size_image() as usize) > MAX_RAW {
            return Err("Camera frame exceeds preview size limit".into());
        }
        // Nonblocking dequeue: the preview thread must never block on a stalled producer.
        let flags = unsafe { libc::fcntl(device.as_raw_fd(), libc::F_GETFL) };
        if flags < 0
            || unsafe { libc::fcntl(device.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) }
                < 0
        {
            return Err("Cannot configure camera preview".into());
        }
        let stream = device
            .stream::<In, Mmap>(ContentType::Video, 4)
            .map_err(|e| format!("Cannot stream camera preview: {e}"))?;
        Ok(Self { stream, layout })
    }

    /// Nonblocking: returns `Ok(None)` when no new frame is available yet.
    pub fn next_frame(&mut self) -> Result<Option<Frame>, String> {
        let buffer = match self.stream.next() {
            Ok(buffer) => buffer,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Ok(None),
            Err(e) => return Err(format!("Cannot read camera preview: {e}")),
        };
        let frame = {
            let data = buffer.lock();
            self.layout.convert(data.as_ref())
        };
        frame.map(Some)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    pub fn qr_frame() -> Frame {
        let width = 640;
        let height = 320;
        let mut rgb = vec![255; width * height * 3];
        for (offset, text) in [
            (20, "https://example.com/scan?a=1&b=2"),
            (335, "plain QR text"),
        ] {
            let qr = qrcode::QrCode::new(text.as_bytes()).unwrap();
            let scale = 5;
            for y in 0..qr.width() {
                for x in 0..qr.width() {
                    if qr[(x, y)] == qrcode::Color::Dark {
                        for dy in 0..scale {
                            for dx in 0..scale {
                                let p =
                                    ((y * scale + dy + 40) * width + x * scale + dx + offset) * 3;
                                rgb[p..p + 3].fill(0);
                            }
                        }
                    }
                }
            }
        }
        scan_rgb(width, height, rgb).unwrap()
    }

    #[test]
    fn detects_web_qr_and_preserves_click_coordinates() {
        let frame = qr_frame();
        assert_eq!(frame.links.len(), 1);
        assert_eq!(frame.links[0].url, "https://example.com/scan?a=1&b=2");
        assert!(frame.links[0]
            .bounds
            .iter()
            .all(|&(x, y)| (15.0..300.0).contains(&x) && (30.0..300.0).contains(&y)));
        assert!(scan_rgb(0, 10, vec![]).is_err());
        assert!(scan_rgb(usize::MAX, 2, vec![]).is_err());
        assert!(scan_rgb(10, 10, vec![0; 3]).is_err());
    }

    #[test]
    fn only_web_urls_without_credentials_or_controls_are_clickable() {
        for text in [
            "javascript:alert(1)",
            "file:///tmp/a",
            "https://",
            "https://user@example.com",
            "https://example.com\n",
            "WIFI:T:WPA;S:test;;",
        ] {
            assert!(web_url(text).is_none(), "{text}");
        }
        assert_eq!(
            web_url("https://example.com/path?q=1#test").as_deref(),
            Some("https://example.com/path?q=1#test")
        );
    }

    #[test]
    fn converts_padded_yuv420_and_rejects_incomplete_frames() {
        let layout = Layout {
            width: 2,
            height: 2,
            stride: 4,
            fourcc: *b"YU12",
        };
        let bytes = [81, 81, 0, 0, 81, 81, 0, 0, 90, 0, 240, 0];
        let frame = layout.convert(&bytes).unwrap();
        assert_eq!(frame.rgb, [255, 0, 0].repeat(4));
        assert!(layout.convert(&bytes[..11]).is_err());
        assert!(Layout {
            stride: 1,
            ..layout
        }
        .required()
        .is_err());
        assert!(Layout {
            width: 8193,
            ..layout
        }
        .required()
        .is_err());
        let bgr = Layout {
            width: 1,
            height: 1,
            stride: 3,
            fourcc: *b"BGR3",
        }
        .convert(&[10, 20, 30])
        .unwrap();
        assert_eq!(bgr.rgb, [30, 20, 10]);
    }
}
