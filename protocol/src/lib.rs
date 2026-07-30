use std::mem::size_of;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

pub const PREVIEW_SHM_MAGIC: u32 = u32::from_le_bytes(*b"GMAN");
pub const PREVIEW_SHM_VERSION: u32 = 3;
pub const PREVIEW_SHM_ALIGNMENT: usize = 64;
pub const PREVIEW_SLOT_COUNT: u32 = 16;
pub const PREVIEW_MAX_SLOT_COUNT: usize = PREVIEW_SLOT_COUNT as usize;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum PreviewPixelFormat {
    Rgba8Unorm = 1,
}

impl PreviewPixelFormat {
    pub fn from_raw(value: u32) -> Option<Self> {
        match value {
            value if value == Self::Rgba8Unorm as u32 => Some(Self::Rgba8Unorm),
            _ => None,
        }
    }

    pub const fn bytes_per_pixel(self) -> u32 {
        match self {
            Self::Rgba8Unorm => 4,
        }
    }
}

#[repr(C)]
pub struct PreviewShmHeader {
    pub magic: u32,
    pub version: u32,
    pub header_size: u32,
    pub capacity: u32,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub pixel_format: u32,
    published_request_ids: [AtomicU64; PREVIEW_MAX_SLOT_COUNT],
    published_frames: [AtomicU64; PREVIEW_MAX_SLOT_COUNT],
}

impl PreviewShmHeader {
    pub fn new(
        width: u32,
        height: u32,
        capacity: u32,
        pixel_format: PreviewPixelFormat,
    ) -> Result<Self, PreviewLayoutError> {
        let stride = width
            .checked_mul(pixel_format.bytes_per_pixel())
            .ok_or(PreviewLayoutError::Overflow)?;
        PreviewLayout::new(width, height, stride, capacity, pixel_format)?;
        Ok(Self {
            magic: PREVIEW_SHM_MAGIC,
            version: PREVIEW_SHM_VERSION,
            header_size: preview_data_offset() as u32,
            capacity,
            width,
            height,
            stride,
            pixel_format: pixel_format as u32,
            published_request_ids: std::array::from_fn(|_| AtomicU64::new(0)),
            published_frames: std::array::from_fn(|_| AtomicU64::new(u64::MAX)),
        })
    }

    pub fn layout(&self) -> Result<PreviewLayout, PreviewLayoutError> {
        if self.magic != PREVIEW_SHM_MAGIC {
            return Err(PreviewLayoutError::InvalidMagic);
        }
        if self.version != PREVIEW_SHM_VERSION {
            return Err(PreviewLayoutError::UnsupportedVersion(self.version));
        }
        if self.header_size as usize != preview_data_offset() {
            return Err(PreviewLayoutError::InvalidHeaderSize);
        }
        let pixel_format = PreviewPixelFormat::from_raw(self.pixel_format).ok_or(
            PreviewLayoutError::UnsupportedPixelFormat(self.pixel_format),
        )?;
        PreviewLayout::new(
            self.width,
            self.height,
            self.stride,
            self.capacity,
            pixel_format,
        )
    }

    pub fn publish(&self, slot: u32, request_id: u64, frame: u32) {
        let slot = slot as usize;
        assert!(slot < self.capacity as usize);
        self.published_frames[slot]
            .store(u64::from(frame), Ordering::Relaxed);
        self.published_request_ids[slot]
            .store(request_id, Ordering::Release);
    }

    pub fn published(&self, slot: u32) -> Option<(u64, u32)> {
        let slot = slot as usize;
        if slot >= self.capacity as usize {
            return None;
        }
        let request_id = self.published_request_ids[slot].load(Ordering::Acquire);
        let frame = self.published_frames[slot].load(Ordering::Relaxed);
        (frame != u64::MAX).then_some((request_id, frame as u32))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PreviewLayout {
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub capacity: u32,
    pub pixel_format: PreviewPixelFormat,
    pub frame_size: usize,
    pub total_size: usize,
}

impl PreviewLayout {
    pub fn packed_rgba(width: u32, height: u32, capacity: u32) -> Result<Self, PreviewLayoutError> {
        let stride = width.checked_mul(4).ok_or(PreviewLayoutError::Overflow)?;
        Self::new(
            width,
            height,
            stride,
            capacity,
            PreviewPixelFormat::Rgba8Unorm,
        )
    }

    pub fn new(
        width: u32,
        height: u32,
        stride: u32,
        capacity: u32,
        pixel_format: PreviewPixelFormat,
    ) -> Result<Self, PreviewLayoutError> {
        if width == 0
            || height == 0
            || capacity == 0
            || capacity as usize > PREVIEW_MAX_SLOT_COUNT
        {
            return Err(PreviewLayoutError::ZeroDimension);
        }
        let packed_stride = width
            .checked_mul(pixel_format.bytes_per_pixel())
            .ok_or(PreviewLayoutError::Overflow)?;
        if stride < packed_stride {
            return Err(PreviewLayoutError::InvalidStride);
        }
        let frame_size = (stride as usize)
            .checked_mul(height as usize)
            .ok_or(PreviewLayoutError::Overflow)?;
        let frames_size = frame_size
            .checked_mul(capacity as usize)
            .ok_or(PreviewLayoutError::Overflow)?;
        let total_size = preview_data_offset()
            .checked_add(frames_size)
            .ok_or(PreviewLayoutError::Overflow)?;
        Ok(Self {
            width,
            height,
            stride,
            capacity,
            pixel_format,
            frame_size,
            total_size,
        })
    }

    pub fn frame_offset(self, frame_index: u64) -> usize {
        preview_data_offset() + (frame_index as usize % self.capacity as usize) * self.frame_size
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PreviewLayoutError {
    ZeroDimension,
    Overflow,
    InvalidMagic,
    UnsupportedVersion(u32),
    InvalidHeaderSize,
    InvalidStride,
    UnsupportedPixelFormat(u32),
}

impl std::fmt::Display for PreviewLayoutError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ZeroDimension => {
                formatter.write_str("preview dimensions and capacity must be non-zero")
            }
            Self::Overflow => formatter.write_str("preview shared-memory size overflow"),
            Self::InvalidMagic => formatter.write_str("invalid preview shared-memory magic"),
            Self::UnsupportedVersion(version) => {
                write!(formatter, "unsupported preview protocol version {version}")
            }
            Self::InvalidHeaderSize => {
                formatter.write_str("invalid preview shared-memory header size")
            }
            Self::InvalidStride => {
                formatter.write_str("preview stride is smaller than a packed row")
            }
            Self::UnsupportedPixelFormat(format) => {
                write!(formatter, "unsupported preview pixel format {format}")
            }
        }
    }
}

impl std::error::Error for PreviewLayoutError {}

pub const fn preview_data_offset() -> usize {
    size_of::<PreviewShmHeader>().next_multiple_of(PREVIEW_SHM_ALIGNMENT)
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "cmd")]
pub enum EditorCommand {
    #[serde(rename = "load_scene")]
    LoadScene { name: String },
    #[serde(rename = "open_preview")]
    OpenPreview { shm_id: String },
    #[serde(rename = "render_frame")]
    RenderFrame {
        request_id: u64,
        frame: u32,
        slot: u32,
    },
    #[serde(rename = "quit")]
    Quit,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "event")]
pub enum EditorEvent {
    #[serde(rename = "scenes_info")]
    ScenesInfo { scenes: Vec<String> },
    #[serde(rename = "scene_ready")]
    SceneReady {
        total_frames: u64,
        width: u32,
        height: u32,
        framerate: u32,
    },
    #[serde(rename = "preview_opened")]
    PreviewOpened,
    #[serde(rename = "frame_ready")]
    FrameReady {
        request_id: u64,
        frame: u32,
        slot: u32,
    },
    #[serde(rename = "error")]
    Error { message: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multi_slot_layout_is_aligned() {
        let layout = PreviewLayout::packed_rgba(1920, 1080, PREVIEW_SLOT_COUNT).unwrap();
        assert_eq!(preview_data_offset() % PREVIEW_SHM_ALIGNMENT, 0);
        assert_eq!(layout.frame_size, 1920 * 1080 * 4);
        assert_ne!(layout.frame_offset(0), layout.frame_offset(1));
        assert_eq!(
            layout.total_size,
            preview_data_offset() + PREVIEW_SLOT_COUNT as usize * layout.frame_size
        );
    }

    #[test]
    fn header_round_trips_its_layout() {
        let header = PreviewShmHeader::new(
            1280,
            720,
            PREVIEW_SLOT_COUNT,
            PreviewPixelFormat::Rgba8Unorm,
        )
        .unwrap();
        let layout = header.layout().unwrap();
        assert_eq!(layout.width, 1280);
        assert_eq!(layout.height, 720);
        assert_eq!(layout.capacity, PREVIEW_SLOT_COUNT);
        assert_eq!(layout.stride, 1280 * 4);
    }

    #[test]
    fn published_frame_uses_request_as_release_sequence() {
        let header =
            PreviewShmHeader::new(2, 1, PREVIEW_SLOT_COUNT, PreviewPixelFormat::Rgba8Unorm)
                .unwrap();
        assert_eq!(header.published(3), None);
        header.publish(3, 17, 42);
        assert_eq!(header.published(3), Some((17, 42)));
        assert_eq!(header.published(2), None);
    }
}
