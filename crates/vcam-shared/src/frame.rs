//! Pixel formats and frame geometry shared by the app and the media source.

/// Largest frame the transport carries: 1920x1080 RGB32.
pub const MAX_WIDTH: u32 = 1920;
pub const MAX_HEIGHT: u32 = 1080;
pub const MAX_FRAME_BYTES: usize = (MAX_WIDTH * MAX_HEIGHT * 4) as usize;

#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PixelFormat {
    /// Y plane followed by interleaved UV at half resolution; stride == width.
    Nv12 = 1,
    /// B, G, R, A bytes per pixel, top-down; stride == width * 4.
    Rgb32 = 2,
}

impl PixelFormat {
    pub fn from_u32(value: u32) -> Option<Self> {
        match value {
            1 => Some(Self::Nv12),
            2 => Some(Self::Rgb32),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Nv12 => "NV12",
            Self::Rgb32 => "RGB32",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameFormat {
    pub pixel: PixelFormat,
    pub width: u32,
    pub height: u32,
}

impl FrameFormat {
    pub fn stride(&self) -> u32 {
        match self.pixel {
            PixelFormat::Rgb32 => self.width * 4,
            PixelFormat::Nv12 => self.width,
        }
    }

    pub fn frame_bytes(&self) -> usize {
        let (w, h) = (self.width as usize, self.height as usize);
        match self.pixel {
            PixelFormat::Rgb32 => w * h * 4,
            PixelFormat::Nv12 => w * h * 3 / 2,
        }
    }

    /// Whether the frame fits the transport and has even dimensions (needed for NV12).
    pub fn is_valid(&self) -> bool {
        self.width > 0
            && self.height > 0
            && self.width % 2 == 0
            && self.height % 2 == 0
            && self.width <= MAX_WIDTH
            && self.height <= MAX_HEIGHT
    }
}

impl std::fmt::Display for FrameFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}x{} {}", self.width, self.height, self.pixel.name())
    }
}
