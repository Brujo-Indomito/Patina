//! Webcam capture with a Media Foundation source reader.
//!
//! The reader runs with advanced video processing enabled, so it decodes
//! MJPEG/YUY2 and converts and scales to exactly the format the virtual
//! camera's client negotiated. The camera's native mode is chosen to need as
//! little scaling as possible at 30 fps or more.

use windows::Win32::Media::MediaFoundation::*;
use windows_core::{Interface, Result, GUID};

use patina_shared::{FrameFormat, PixelFormat};

use crate::devices::{subtype_name, Device};

const FIRST_VIDEO_STREAM: u32 = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;

/// A camera mode reported by the device.
#[derive(Clone, Debug)]
pub struct NativeMode {
    pub index: u32,
    pub subtype: GUID,
    pub width: u32,
    pub height: u32,
    pub fps: f64,
}

impl std::fmt::Display for NativeMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}x{} {} @ {:.1} fps", self.width, self.height, subtype_name(&self.subtype), self.fps)
    }
}

pub struct Camera {
    name: String,
    activate: IMFActivate,
    source: IMFMediaSource,
    reader: IMFSourceReader,
    output: FrameFormat,
    mode: NativeMode,
}

impl Camera {
    /// Opens `device` and configures it to deliver frames in `output`.
    pub fn open(device: Device, output: FrameFormat) -> Result<Self> {
        let source: IMFMediaSource = unsafe { device.activate.ActivateObject()? };
        let result = configure(&source, output);
        match result {
            Ok((reader, mode)) => Ok(Self { name: device.name, activate: device.activate, source, reader, output, mode }),
            Err(err) => {
                unsafe {
                    let _ = source.Shutdown();
                    let _ = device.activate.ShutdownObject();
                }
                Err(err)
            }
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn mode(&self) -> &NativeMode {
        &self.mode
    }

    pub fn output(&self) -> FrameFormat {
        self.output
    }

    /// Blocks until the next frame and copies it, tightly packed in the
    /// output format, into `dst`. Returns `Ok(false)` for gaps without a frame.
    pub fn read_frame(&self, dst: &mut Vec<u8>) -> Result<bool> {
        let mut flags = 0u32;
        let mut sample = None;
        unsafe {
            self.reader.ReadSample(FIRST_VIDEO_STREAM, 0, None, Some(&mut flags), None, Some(&mut sample))?;
        }
        if flags & (MF_SOURCE_READERF_ERROR.0 | MF_SOURCE_READERF_ENDOFSTREAM.0) as u32 != 0 {
            return Err(MF_E_VIDEO_RECORDING_DEVICE_INVALIDATED.into());
        }
        let Some(sample) = sample else { return Ok(false) };

        let buffer = unsafe { sample.ConvertToContiguousBuffer()? };
        dst.resize(self.output.frame_bytes(), 0);
        match buffer.cast::<IMF2DBuffer>() {
            Ok(buffer2d) => copy_2d(&buffer2d, self.output, dst)?,
            Err(_) => copy_contiguous(&buffer, dst)?,
        }
        Ok(true)
    }
}

impl Drop for Camera {
    fn drop(&mut self) {
        unsafe {
            let _ = self.source.Shutdown();
            let _ = self.activate.ShutdownObject();
        }
    }
}

/// Lists the camera's native modes (opens the device briefly).
pub fn native_modes(device: &Device) -> Result<Vec<NativeMode>> {
    let source: IMFMediaSource = unsafe { device.activate.ActivateObject()? };
    let modes = unsafe { MFCreateSourceReaderFromMediaSource(&source, None) }.and_then(|reader| read_native_modes(&reader));
    unsafe {
        let _ = source.Shutdown();
        let _ = device.activate.ShutdownObject();
    }
    modes
}

fn read_native_modes(reader: &IMFSourceReader) -> Result<Vec<NativeMode>> {
    let mut modes = Vec::new();
    for index in 0.. {
        let Ok(media_type) = (unsafe { reader.GetNativeMediaType(FIRST_VIDEO_STREAM, index) }) else { break };
        let (Ok(subtype), Ok(size)) = (unsafe { media_type.GetGUID(&MF_MT_SUBTYPE) }, unsafe { media_type.GetUINT64(&MF_MT_FRAME_SIZE) }) else {
            continue;
        };
        let rate = unsafe { media_type.GetUINT64(&MF_MT_FRAME_RATE) }.unwrap_or(0);
        let (num, den) = ((rate >> 32) as f64, (rate & 0xffff_ffff) as f64);
        modes.push(NativeMode {
            index,
            subtype,
            width: (size >> 32) as u32,
            height: (size & 0xffff_ffff) as u32,
            fps: if den > 0.0 { num / den } else { 0.0 },
        });
    }
    Ok(modes)
}

/// Lower is better: prefer >= 29 fps, then the smallest mode that covers the
/// output, then the same aspect ratio, then uncompressed formats.
fn mode_cost(mode: &NativeMode, output: FrameFormat) -> (u8, u64, u8, u8) {
    let too_slow = u8::from(mode.fps < 29.0);
    let covers = mode.width >= output.width && mode.height >= output.height;
    let area = mode.width as u64 * mode.height as u64;
    let size_cost = if covers { area } else { u64::MAX / 2 - area };
    let aspect_mismatch = u8::from(mode.width as u64 * output.height as u64 != mode.height as u64 * output.width as u64);
    let compressed = u8::from(mode.subtype == MFVideoFormat_MJPG);
    (too_slow, size_cost, aspect_mismatch, compressed)
}

fn configure(source: &IMFMediaSource, output: FrameFormat) -> Result<(IMFSourceReader, NativeMode)> {
    let mut attributes = None;
    unsafe { MFCreateAttributes(&mut attributes, 1)? };
    let attributes = attributes.ok_or_else(|| windows_core::Error::from(MF_E_UNEXPECTED))?;
    unsafe { attributes.SetUINT32(&MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING, 1)? };
    let reader = unsafe { MFCreateSourceReaderFromMediaSource(source, &attributes)? };

    let modes = read_native_modes(&reader)?;
    let mode = modes
        .iter()
        .min_by_key(|mode| mode_cost(mode, output))
        .cloned()
        .ok_or_else(|| windows_core::Error::from(MF_E_INVALIDMEDIATYPE))?;

    unsafe {
        let native = reader.GetNativeMediaType(FIRST_VIDEO_STREAM, mode.index)?;
        reader.SetCurrentMediaType(FIRST_VIDEO_STREAM, None, &native)?;

        let wanted = MFCreateMediaType()?;
        wanted.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        wanted.SetGUID(
            &MF_MT_SUBTYPE,
            match output.pixel {
                PixelFormat::Nv12 => &MFVideoFormat_NV12,
                PixelFormat::Rgb32 => &MFVideoFormat_RGB32,
            },
        )?;
        wanted.SetUINT64(&MF_MT_FRAME_SIZE, ((output.width as u64) << 32) | output.height as u64)?;
        reader.SetCurrentMediaType(FIRST_VIDEO_STREAM, None, &wanted)?;
    }
    Ok((reader, mode))
}

/// Copies a possibly padded or bottom-up 2D buffer into a tightly packed frame.
fn copy_2d(buffer: &IMF2DBuffer, format: FrameFormat, dst: &mut [u8]) -> Result<()> {
    let mut scanline0 = std::ptr::null_mut();
    let mut pitch = 0i32;
    unsafe { buffer.Lock2D(&mut scanline0, &mut pitch)? };

    let row_bytes = format.stride() as usize;
    let rows = match format.pixel {
        // The interleaved UV plane follows the Y plane at the same pitch.
        PixelFormat::Nv12 => format.height as usize * 3 / 2,
        PixelFormat::Rgb32 => format.height as usize,
    };
    for (row, out) in dst.chunks_exact_mut(row_bytes).take(rows).enumerate() {
        unsafe {
            let src = scanline0.offset(row as isize * pitch as isize);
            std::ptr::copy_nonoverlapping(src, out.as_mut_ptr(), row_bytes);
        }
    }

    unsafe { buffer.Unlock2D() }
}

fn copy_contiguous(buffer: &IMFMediaBuffer, dst: &mut [u8]) -> Result<()> {
    let mut data = std::ptr::null_mut();
    let mut length = 0u32;
    unsafe { buffer.Lock(&mut data, None, Some(&mut length))? };
    let n = dst.len().min(length as usize);
    unsafe {
        std::ptr::copy_nonoverlapping(data, dst.as_mut_ptr(), n);
        buffer.Unlock()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mode(index: u32, subtype: GUID, width: u32, height: u32, fps: f64) -> NativeMode {
        NativeMode { index, subtype, width, height, fps }
    }

    #[test]
    fn prefers_smallest_fast_mode_covering_output() {
        let output = FrameFormat { pixel: PixelFormat::Nv12, width: 1280, height: 720 };
        let modes = [
            mode(0, MFVideoFormat_YUY2, 1920, 1080, 5.0),
            mode(1, MFVideoFormat_MJPG, 1920, 1080, 30.0),
            mode(2, MFVideoFormat_MJPG, 1280, 720, 30.0),
            mode(3, MFVideoFormat_YUY2, 1280, 720, 10.0),
            mode(4, MFVideoFormat_YUY2, 640, 480, 30.0),
        ];
        let best = modes.iter().min_by_key(|m| mode_cost(m, output)).unwrap();
        assert_eq!(best.index, 2);
    }

    #[test]
    fn prefers_uncompressed_when_otherwise_equal() {
        let output = FrameFormat { pixel: PixelFormat::Nv12, width: 640, height: 360 };
        let modes = [mode(0, MFVideoFormat_MJPG, 640, 360, 30.0), mode(1, MFVideoFormat_YUY2, 640, 360, 30.0)];
        let best = modes.iter().min_by_key(|m| mode_cost(m, output)).unwrap();
        assert_eq!(best.index, 1);
    }

    #[test]
    fn falls_back_to_largest_mode_when_none_cover() {
        let output = FrameFormat { pixel: PixelFormat::Nv12, width: 1920, height: 1080 };
        let modes = [mode(0, MFVideoFormat_YUY2, 640, 480, 30.0), mode(1, MFVideoFormat_MJPG, 1280, 720, 30.0)];
        let best = modes.iter().min_by_key(|m| mode_cost(m, output)).unwrap();
        assert_eq!(best.index, 1);
    }
}
