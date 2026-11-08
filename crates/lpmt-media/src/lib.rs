//! Media decoding for LPMT.
//!
//! Images are always available. Video decoding is compiled in with the
//! `video` feature and decodes into tightly packed RGBA frames on a worker
//! thread; the renderer uploads those frames as GPU textures. Audio playback is
//! compiled in with the `audio` feature and follows the application's show
//! clock. Linux window capture uses the optional `pipewire` feature and the
//! native pixel formats and a borrowed-buffer GPU upload path.

#[cfg(all(feature = "pipewire", target_os = "linux"))]
pub mod capture;

use std::path::Path;

use anyhow::{Context, Result};

/// Native packed window pixels. X formats have undefined alpha, not transparency.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapturePixelFormat {
    Rgba,
    Rgbx,
    Bgra,
    Bgrx,
}

/// A capture worker calls this synchronously while it owns the producer buffer.
pub trait CaptureConsumer: Send + 'static {
    type Frame: Send + 'static;
    fn upload(&mut self, index: usize, frame: CaptureFrame<'_>) -> Result<Self::Frame>;
    fn release(&mut self, index: usize);
}

/// A view into a live producer buffer, valid only until the upload returns.
/// The first byte is the visible crop origin; row padding stays in the source.
#[derive(Clone, Copy)]
pub struct CaptureFrame<'a> {
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub format: CapturePixelFormat,
    pub data: &'a [u8],
}

impl CaptureFrame<'_> {
    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(self.width > 0 && self.height > 0, "Empty capture frame");
        let row = u64::from(self.width) * 4;
        anyhow::ensure!(
            u64::from(self.stride) >= row && self.stride.is_multiple_of(4),
            "Invalid capture stride"
        );
        let required = u64::from(self.height - 1) * u64::from(self.stride) + row;
        anyhow::ensure!(
            self.data.len() as u64 >= required,
            "Truncated capture frame"
        );
        Ok(())
    }
}

#[cfg(feature = "audio")]
mod audio;
#[cfg(feature = "audio")]
pub use audio::AudioPlayer;

#[cfg(feature = "video")]
mod video;
#[cfg(feature = "video")]
pub use video::{PlaybackState, VideoFrame, VideoInfo, VideoPlayer};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedImage {
    pub width: u32,
    pub height: u32,
    /// Tightly packed RGBA8 pixels, top-left origin.
    pub rgba: Vec<u8>,
}

impl DecodedImage {
    pub fn from_rgba8(width: u32, height: u32, rgba: Vec<u8>) -> Result<Self> {
        let expected = width as usize * height as usize * 4;
        anyhow::ensure!(
            rgba.len() == expected,
            "expected {expected} RGBA bytes for {width}x{height}, got {}",
            rgba.len()
        );
        Ok(Self {
            width,
            height,
            rgba,
        })
    }
}

pub fn load_image(path: impl AsRef<Path>) -> Result<DecodedImage> {
    let path = path.as_ref();
    let bytes =
        std::fs::read(path).with_context(|| format!("failed to read image {}", path.display()))?;
    decode_image(&bytes).with_context(|| format!("failed to decode image {}", path.display()))
}

pub fn decode_image(bytes: &[u8]) -> Result<DecodedImage> {
    let image = image::load_from_memory(bytes).context("unsupported or corrupt image")?;
    let rgba = image.to_rgba8();
    let (width, height) = rgba.dimensions();
    DecodedImage::from_rgba8(width, height, rgba.into_raw())
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use image::{ImageFormat, Rgba, RgbaImage};

    use super::*;

    #[test]
    fn capture_layout_validation_rejects_invalid_gpu_uploads() {
        let pixels = [0; 24];
        let frame = CaptureFrame {
            width: 2,
            height: 2,
            stride: 16,
            format: CapturePixelFormat::Bgrx,
            data: &pixels,
        };
        assert!(frame.validate().is_ok()); // final row doesn't require padding
        assert!(CaptureFrame {
            data: &pixels[..23],
            ..frame
        }
        .validate()
        .is_err());
        assert!(CaptureFrame { stride: 9, ..frame }.validate().is_err());
        assert!(CaptureFrame { stride: 4, ..frame }.validate().is_err());
        assert!(CaptureFrame { width: 0, ..frame }.validate().is_err());
        assert!(CaptureFrame {
            width: u32::MAX,
            ..frame
        }
        .validate()
        .is_err());
    }

    #[test]
    fn decodes_png_to_rgba8() {
        let image = RgbaImage::from_pixel(2, 3, Rgba([12, 34, 56, 255]));
        let mut png = Cursor::new(Vec::new());
        image.write_to(&mut png, ImageFormat::Png).unwrap();

        let decoded = decode_image(png.get_ref()).unwrap();
        assert_eq!(decoded.width, 2);
        assert_eq!(decoded.height, 3);
        assert_eq!(decoded.rgba.len(), 2 * 3 * 4);
        assert_eq!(&decoded.rgba[..4], &[12, 34, 56, 255]);
    }

    #[test]
    fn rejects_wrong_pixel_buffer_size() {
        assert!(DecodedImage::from_rgba8(2, 2, vec![0; 15]).is_err());
    }
}
