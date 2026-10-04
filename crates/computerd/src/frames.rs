use anyhow::{Context, Result};

/// Tracks which frame a screen shows and which one its session saw last.
#[derive(Debug, Default)]
pub(crate) struct FrameTracker {
    current: u64,
    seen: Option<u64>,
}

/// What an observation should return for the current frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Frame {
    pub(crate) id: u64,
    /// False when the session already has this frame's image.
    pub(crate) send_image: bool,
}

impl FrameTracker {
    /// Records an observation. `damaged` says whether the screen changed since the previous one.
    pub(crate) fn observe(&mut self, damaged: bool) -> Frame {
        if damaged || self.current == 0 {
            self.current += 1;
        }
        let send_image = self.seen != Some(self.current);
        Frame {
            id: self.current,
            send_image,
        }
    }

    /// Marks the current frame's image as delivered to the session.
    pub(crate) fn delivered(&mut self) {
        self.seen = Some(self.current);
    }
}

/// Bytes per pixel in the X server's 24-bit depth format.
pub(crate) const BYTES_PER_PIXEL: usize = 4;

/// Converts little-endian BGRX pixels to packed RGB.
pub(crate) fn bgrx_to_rgb(bgrx: &[u8]) -> Vec<u8> {
    let mut rgb = Vec::with_capacity(bgrx.len() / BYTES_PER_PIXEL * 3);
    let (pixels, _) = bgrx.as_chunks::<BYTES_PER_PIXEL>();
    for pixel in pixels {
        rgb.extend_from_slice(&[pixel[2], pixel[1], pixel[0]]);
    }
    rgb
}

/// Encodes packed RGB pixels as a PNG.
pub(crate) fn encode_png(width: u16, height: u16, rgb: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut encoder = png::Encoder::new(&mut out, u32::from(width), u32::from(height));
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.set_compression(png::Compression::Fast);
    let mut writer = encoder.write_header().context("writing the PNG header")?;
    writer
        .write_image_data(rgb)
        .context("writing the PNG pixels")?;
    writer.finish().context("finishing the PNG")?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unchanged_screen_keeps_its_frame_and_skips_the_image() {
        let mut tracker = FrameTracker::default();
        let first = tracker.observe(false);
        assert_eq!(
            first,
            Frame {
                id: 1,
                send_image: true
            }
        );
        tracker.delivered();

        assert_eq!(
            tracker.observe(false),
            Frame {
                id: 1,
                send_image: false
            }
        );

        let changed = tracker.observe(true);
        assert_eq!(
            changed,
            Frame {
                id: 2,
                send_image: true
            }
        );
    }

    #[test]
    fn frame_is_resent_until_it_was_delivered() {
        let mut tracker = FrameTracker::default();
        tracker.observe(false);
        assert_eq!(
            tracker.observe(false),
            Frame {
                id: 1,
                send_image: true
            }
        );
    }

    #[test]
    fn pixels_are_reordered_from_bgrx_to_rgb() {
        assert_eq!(
            bgrx_to_rgb(&[1, 2, 3, 0xff, 10, 20, 30, 0]),
            vec![3, 2, 1, 30, 20, 10]
        );
    }

    #[test]
    fn png_round_trips_the_pixels() {
        let rgb: Vec<u8> = (0..4 * 2 * 3)
            .map(|n| u8::try_from(n * 7).unwrap())
            .collect();
        let bytes = encode_png(4, 2, &rgb).unwrap();
        let mut reader = png::Decoder::new(std::io::Cursor::new(bytes))
            .read_info()
            .unwrap();
        let mut decoded = vec![0; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut decoded).unwrap();
        assert_eq!((info.width, info.height), (4, 2));
        assert_eq!(&decoded[..info.buffer_size()], rgb.as_slice());
    }
}
