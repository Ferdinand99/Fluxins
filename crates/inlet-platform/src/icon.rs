use crate::{Error, Result};

/// Largest icon side accepted, in pixels.
const MAX_SIDE: u32 = 1024;

/// An RGBA image (8 bits per channel, straight alpha, rows top to bottom).
#[derive(Clone, PartialEq, Eq)]
pub struct Icon {
    width: u32,
    height: u32,
    rgba: Vec<u8>,
}

impl std::fmt::Debug for Icon {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Icon({}x{})", self.width, self.height)
    }
}

impl Icon {
    /// An icon from raw RGBA bytes.
    pub fn from_rgba(rgba: Vec<u8>, width: u32, height: u32) -> Result<Self> {
        if width == 0 || height == 0 || width > MAX_SIDE || height > MAX_SIDE {
            return Err(Error::InvalidInput("icon size"));
        }
        if rgba.len() != (width * height * 4) as usize {
            return Err(Error::InvalidInput("icon data does not match its size"));
        }
        Ok(Self {
            width,
            height,
            rgba,
        })
    }

    /// An icon from a PNG file (any colour type; converted to RGBA).
    pub fn from_png(bytes: &[u8]) -> Result<Self> {
        let limits = png::Limits {
            bytes: (MAX_SIDE * MAX_SIDE * 4) as usize,
        };
        let mut decoder = png::Decoder::new_with_limits(std::io::Cursor::new(bytes), limits);
        decoder.set_transformations(png::Transformations::normalize_to_color8());
        let mut reader = decoder
            .read_info()
            .map_err(|_| Error::InvalidInput("not a readable PNG"))?;
        let size = reader
            .output_buffer_size()
            .ok_or(Error::InvalidInput("PNG too large"))?;
        let mut buf = vec![0; size];
        let info = reader
            .next_frame(&mut buf)
            .map_err(|_| Error::InvalidInput("not a readable PNG"))?;
        buf.truncate(info.buffer_size());
        let rgba = match info.color_type {
            png::ColorType::Rgba => buf,
            png::ColorType::Rgb => buf
                .as_chunks::<3>()
                .0
                .iter()
                .flat_map(|p| [p[0], p[1], p[2], 255])
                .collect(),
            png::ColorType::GrayscaleAlpha => buf
                .as_chunks::<2>()
                .0
                .iter()
                .flat_map(|p| [p[0], p[0], p[0], p[1]])
                .collect(),
            png::ColorType::Grayscale => buf.iter().flat_map(|&g| [g, g, g, 255]).collect(),
            png::ColorType::Indexed => {
                return Err(Error::InvalidInput("indexed PNG was not expanded"))
            }
        };
        Self::from_rgba(rgba, info.width, info.height)
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn rgba(&self) -> &[u8] {
        &self.rgba
    }

    /// The pixels as ARGB32 in network byte order, the StatusNotifierItem format.
    #[cfg(any(target_os = "linux", test))]
    pub(crate) fn to_argb32_be(&self) -> Vec<u8> {
        self.rgba
            .as_chunks::<4>()
            .0
            .iter()
            .flat_map(|p| [p[3], p[0], p[1], p[2]])
            .collect()
    }

    /// The pixels as premultiplied BGRA, the layout of a 32-bit Windows DIB.
    #[cfg(any(windows, test))]
    pub(crate) fn to_bgra_premultiplied(&self) -> Vec<u8> {
        self.rgba
            .as_chunks::<4>()
            .0
            .iter()
            .flat_map(|p| {
                let a = u16::from(p[3]);
                let pm = |c: u8| ((u16::from(c) * a + 127) / 255) as u8;
                [pm(p[2]), pm(p[1]), pm(p[0]), p[3]]
            })
            .collect()
    }

    /// Fluxer's tray indicator: a dot in the top right corner with a transparent gap
    /// cut around it, both anti-aliased with 4x4 subsamples. The sizes are Fluxer's
    /// (`DOT_RADIUS_RATIO` 0.22, `DOT_GAP_RATIO` 0.0625 of the smaller side).
    pub(crate) fn with_dot(&self, rgb: [u8; 3]) -> Icon {
        const RADIUS: f32 = 0.22;
        const GAP: f32 = 0.0625;
        let (w, h) = (self.width as usize, self.height as usize);
        let side = w.min(h) as f32;
        let radius = side * RADIUS;
        let cut = radius + side * GAP;
        let (cx, cy) = (w as f32 - radius, radius);
        let mut out = self.rgba.clone();
        for y in 0..h {
            for x in 0..w {
                let cut_cov = coverage(x, y, cx, cy, cut);
                if cut_cov == 0.0 {
                    continue;
                }
                let dot = coverage(x, y, cx, cy, radius);
                let i = (y * w + x) * 4;
                for (ch, value) in rgb.iter().enumerate() {
                    let base = f32::from(out[i + ch]) * (1.0 - cut_cov);
                    out[i + ch] = (base + f32::from(*value) * dot).round() as u8;
                }
                let alpha = f32::from(out[i + 3]) * (1.0 - cut_cov) + 255.0 * dot;
                out[i + 3] = alpha.round().min(255.0) as u8;
            }
        }
        Icon {
            width: self.width,
            height: self.height,
            rgba: out,
        }
    }
}

/// The share of pixel (x, y) inside the circle, from 4x4 subsamples.
pub(crate) fn coverage(x: usize, y: usize, cx: f32, cy: f32, r: f32) -> f32 {
    const N: usize = 4;
    let mut hits = 0;
    for sy in 0..N {
        let dy = y as f32 + (sy as f32 + 0.5) / N as f32 - cy;
        for sx in 0..N {
            let dx = x as f32 + (sx as f32 + 0.5) / N as f32 - cx;
            if dx * dx + dy * dy <= r * r {
                hits += 1;
            }
        }
    }
    hits as f32 / (N * N) as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(side: u32, rgba: [u8; 4]) -> Icon {
        Icon::from_rgba(rgba.repeat((side * side) as usize), side, side).unwrap()
    }

    fn px(icon: &Icon, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * icon.width() + x) * 4) as usize;
        icon.rgba()[i..i + 4].try_into().unwrap()
    }

    #[test]
    fn sizes_are_checked() {
        assert!(Icon::from_rgba(vec![0; 16], 2, 2).is_ok());
        assert!(Icon::from_rgba(vec![0; 15], 2, 2).is_err());
        assert!(Icon::from_rgba(vec![], 0, 0).is_err());
        assert!(Icon::from_rgba(vec![0; 4 * 2048], 2048, 1).is_err());
    }

    #[test]
    fn png_is_decoded_to_rgba() {
        let mut bytes = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut bytes, 2, 1);
            enc.set_color(png::ColorType::Rgb);
            enc.set_depth(png::BitDepth::Eight);
            let mut w = enc.write_header().unwrap();
            w.write_image_data(&[255, 0, 0, 0, 0, 255]).unwrap();
        }
        let icon = Icon::from_png(&bytes).unwrap();
        assert_eq!((icon.width(), icon.height()), (2, 1));
        assert_eq!(icon.rgba(), &[255, 0, 0, 255, 0, 0, 255, 255]);
        assert!(Icon::from_png(b"not a png").is_err());
        assert!(Icon::from_png(&bytes[..bytes.len() / 2]).is_err());
    }

    #[test]
    fn the_dot_sits_top_right_with_a_gap() {
        let base = solid(32, [10, 20, 30, 255]);
        let dotted = base.with_dot([0xdd, 0, 0]);
        // Centre of the dot: the dot colour, opaque.
        assert_eq!(px(&dotted, 32 - 4, 4), [0xdd, 0, 0, 255]);
        // Bottom left: untouched.
        assert_eq!(px(&dotted, 1, 30), [10, 20, 30, 255]);
        // In the gap ring between dot and icon: transparent.
        let r = 32.0 * 0.22;
        let gap_x = (32.0 - r - r - 1.0) as u32;
        assert_eq!(px(&dotted, gap_x, r as u32)[3], 0);
    }

    #[test]
    fn pixel_formats() {
        let icon = Icon::from_rgba(vec![1, 2, 3, 255, 200, 100, 50, 0], 2, 1).unwrap();
        assert_eq!(icon.to_argb32_be(), vec![255, 1, 2, 3, 0, 200, 100, 50]);
        assert_eq!(icon.to_bgra_premultiplied(), vec![3, 2, 1, 255, 0, 0, 0, 0]);
    }
}
