//! `--probe`: opens the virtual camera like any client app and reads frames.
//!
//! Reports the negotiated format, the delivered frame rate, what kind of
//! content arrived (the app's test pattern, the source's idle pattern, or
//! something else such as a webcam), and how many frames differed from the
//! previous one. Set `PATINA_PROBE_SNAPSHOT=path.bmp` to save the last frame.

use std::time::Instant;

use windows::Win32::Media::MediaFoundation::*;
use windows_core::Result;

use crate::devices::{subtype_name, video_devices};

const FIRST_VIDEO_STREAM: u32 = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Content {
    LivePattern,
    IdlePattern,
    Other,
}

pub fn probe(camera_name: &str, frame_count: u32) -> Result<()> {
    let devices = video_devices()?;
    println!("Video capture devices:");
    for device in &devices {
        println!("  - {}", device.name);
    }

    let Some(device) = devices.into_iter().find(|d| d.name.starts_with(camera_name)) else {
        println!("\"{camera_name}\" not found. Is patina-cli running?");
        return Err(MF_E_NOT_FOUND.into());
    };

    let source: IMFMediaSource = unsafe { device.activate.ActivateObject()? };
    let result = read_frames(&source, frame_count);
    unsafe {
        let _ = source.Shutdown();
        let _ = device.activate.ShutdownObject();
    }
    result
}

fn read_frames(source: &IMFMediaSource, frame_count: u32) -> Result<()> {
    let reader = unsafe { MFCreateSourceReaderFromMediaSource(source, None)? };
    let media_type = unsafe { reader.GetCurrentMediaType(FIRST_VIDEO_STREAM)? };
    let subtype = unsafe { media_type.GetGUID(&MF_MT_SUBTYPE)? };
    let size = unsafe { media_type.GetUINT64(&MF_MT_FRAME_SIZE)? };
    let (width, height) = ((size >> 32) as usize, (size & 0xffff_ffff) as usize);
    let is_nv12 = subtype == MFVideoFormat_NV12;
    println!("Format: {} {width}x{height}", subtype_name(&subtype));

    let started = Instant::now();
    let mut received = 0u32;
    let mut changed = 0u32;
    let mut counts = [0u32; 3];
    let mut previous: Vec<u8> = Vec::new();
    let mut first_timestamp = None;
    let mut last_timestamp = 0i64;

    while received < frame_count {
        let mut flags = 0u32;
        let mut timestamp = 0i64;
        let mut sample = None;
        unsafe {
            reader.ReadSample(FIRST_VIDEO_STREAM, 0, None, Some(&mut flags), Some(&mut timestamp), Some(&mut sample))?;
        }
        if flags & MF_SOURCE_READERF_ERROR.0 as u32 != 0 {
            println!("Reader reported an error (flags {flags:#x})");
            return Err(MF_E_INVALIDREQUEST.into());
        }
        let Some(sample) = sample else { continue };

        let buffer = unsafe { sample.ConvertToContiguousBuffer()? };
        let mut data = std::ptr::null_mut();
        let mut length = 0u32;
        unsafe { buffer.Lock(&mut data, None, Some(&mut length))? };
        let bytes = unsafe { std::slice::from_raw_parts(data, length as usize) }.to_vec();
        unsafe { buffer.Unlock()? };

        let content = if is_nv12 { classify(&bytes, width, height) } else { Content::Other };
        counts[content as usize] += 1;
        if !previous.is_empty() && previous != bytes {
            changed += 1;
        }
        if received == 0 || received + 1 == frame_count {
            let luma = if is_nv12 {
                format!(", mean Y={:.0}, detail={:.2}", mean_luma(&bytes, width, height), detail(&bytes, width, height))
            } else {
                String::new()
            };
            println!("  frame {received}: {length} bytes{luma}, content: {content:?}");
        }

        previous = bytes;
        first_timestamp.get_or_insert(timestamp);
        last_timestamp = timestamp;
        received += 1;
    }

    let elapsed = started.elapsed().as_secs_f64();
    let span = (last_timestamp - first_timestamp.unwrap_or(0)) as f64 / 1e7;
    println!("Received {received} frames in {elapsed:.2}s ({:.1} fps wall clock, {span:.2}s of timestamps)", received as f64 / elapsed);
    println!(
        "Content: {} live pattern, {} idle pattern, {} other (webcam); {changed} frames differed from the previous one",
        counts[Content::LivePattern as usize],
        counts[Content::IdlePattern as usize],
        counts[Content::Other as usize]
    );

    if let (Some(path), true) = (std::env::var_os("PATINA_PROBE_SNAPSHOT"), is_nv12) {
        write_bmp(std::path::Path::new(&path), &previous, width, height).map_err(|e| windows_core::Error::new(MF_E_UNEXPECTED, e.to_string()))?;
        println!("Saved the last frame to {}", std::path::Path::new(&path).display());
    }
    Ok(())
}

/// Checks the middle of the 5th of 8 bars: magenta in the app's test
/// pattern, flat gray (Y=57) in the source's idle pattern.
fn classify(frame: &[u8], width: usize, height: usize) -> Content {
    let (x, y) = (width * 9 / 16, height / 3);
    let uv = width * height + (y / 2) * width + (x / 2) * 2;
    match (frame.get(y * width + x), frame.get(uv), frame.get(uv + 1)) {
        (Some(_), Some(&u), Some(&v)) if (u as i32 - 128).abs() + (v as i32 - 128).abs() > 40 && (u as i32) > 160 && (v as i32) > 160 => Content::LivePattern,
        (Some(&57), Some(&128), Some(&128)) => Content::IdlePattern,
        _ => Content::Other,
    }
}

/// Mean absolute difference between horizontally adjacent luma samples: a
/// rough sharpness measure that drops when the background is blurred.
fn detail(frame: &[u8], width: usize, height: usize) -> f64 {
    let y_plane = &frame[..(width * height).min(frame.len())];
    let total: u64 = y_plane.chunks_exact(width).flat_map(|row| row.windows(2)).map(|pair| pair[0].abs_diff(pair[1]) as u64).sum();
    total as f64 / (height * (width - 1)).max(1) as f64
}

fn mean_luma(frame: &[u8], width: usize, height: usize) -> f64 {
    let y_plane = &frame[..(width * height).min(frame.len())];
    y_plane.iter().map(|&v| v as u64).sum::<u64>() as f64 / y_plane.len().max(1) as f64
}

/// Writes an NV12 frame as a 24-bit bottom-up BMP (BT.601 limited range).
fn write_bmp(path: &std::path::Path, frame: &[u8], width: usize, height: usize) -> std::io::Result<()> {
    let row = (width * 3 + 3) & !3;
    let image_size = row * height;
    let mut out = Vec::with_capacity(54 + image_size);
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&((54 + image_size) as u32).to_le_bytes());
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(&54u32.to_le_bytes());
    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&(width as i32).to_le_bytes());
    out.extend_from_slice(&(height as i32).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&24u16.to_le_bytes());
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(&(image_size as u32).to_le_bytes());
    out.extend_from_slice(&[0; 16]);

    let uv_plane = width * height;
    for y in (0..height).rev() {
        let start = out.len();
        for x in 0..width {
            let luma = frame.get(y * width + x).copied().unwrap_or(16) as i32 - 16;
            let uv = uv_plane + (y / 2) * width + (x / 2) * 2;
            let u = frame.get(uv).copied().unwrap_or(128) as i32 - 128;
            let v = frame.get(uv + 1).copied().unwrap_or(128) as i32 - 128;
            let c = 298 * luma;
            let clamp = |value: i32| ((value + 128) >> 8).clamp(0, 255) as u8;
            out.extend_from_slice(&[clamp(c + 516 * u), clamp(c - 100 * u - 208 * v), clamp(c + 409 * v)]);
        }
        out.resize(start + row, 0);
    }
    std::fs::write(path, out)
}
