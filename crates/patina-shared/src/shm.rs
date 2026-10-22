//! Shared-memory frame transport between the app (producer) and the media
//! source running in the Frame Server (consumer).
//!
//! The Frame Server runs as LocalService in session 0, so the section lives in
//! the `Global\` namespace. Only the consumer can create it: creating global
//! objects needs `SeCreateGlobalPrivilege`, which services have and normal
//! user processes lack. The producer opens it and retries until it exists,
//! which is also exactly when a client has the camera open.
//!
//! Layout: a 4 KiB [`Header`] followed by [`SLOT_COUNT`] frame slots of
//! [`MAX_FRAME_BYTES`] each. The producer writes into the slot after
//! `latest_slot` and publishes it; each slot carries a sequence counter
//! (odd while being written) so the consumer can detect torn reads without
//! cross-process locks.
//!
//! The consumer publishes the format its client negotiated plus a heartbeat;
//! the producer renders only while that heartbeat is fresh.

use std::sync::atomic::{fence, AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{
    CloseHandle, GetLastError, LocalFree, ERROR_ALREADY_EXISTS, HANDLE, HLOCAL, INVALID_HANDLE_VALUE,
};
use windows::Win32::Security::Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1};
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows::Win32::System::Memory::{
    CreateFileMappingW, MapViewOfFile, OpenFileMappingW, UnmapViewOfFile, FILE_MAP_READ, FILE_MAP_WRITE,
    MEMORY_MAPPED_VIEW_ADDRESS, PAGE_READWRITE,
};
use windows::Win32::System::SystemInformation::GetTickCount64;
use windows::Win32::System::Threading::{
    CreateEventW, OpenEventW, SetEvent, WaitForSingleObject, EVENT_MODIFY_STATE,
};
use windows_core::{Result, HSTRING};

use crate::frame::{FrameFormat, PixelFormat, MAX_FRAME_BYTES};

pub const MAPPING_NAME: &str = "Global\\Patina_Frames_v2";
pub const MAGIC: u32 = u32::from_le_bytes(*b"RVCM");
pub const VERSION: u32 = 2;
pub const SLOT_COUNT: usize = 3;
pub const HEADER_BYTES: usize = 4096;
pub const MAPPING_BYTES: usize = HEADER_BYTES + SLOT_COUNT * MAX_FRAME_BYTES;

/// A heartbeat older than this means the other side is gone.
pub const HEARTBEAT_TIMEOUT_MS: u64 = 1000;

/// Full control for SYSTEM, LocalService and Administrators; read/write for
/// interactively logged-on users (the app).
const SECURITY_DESCRIPTOR: &str = "D:P(A;;GA;;;SY)(A;;GA;;;LS)(A;;GA;;;BA)(A;;GRGW;;;IU)";

#[repr(C)]
pub struct Header {
    pub magic: AtomicU32,
    pub version: AtomicU32,
    // Written by the consumer.
    pub consumer_heartbeat_ms: AtomicU64,
    pub requested_pixel: AtomicU32,
    pub requested_width: AtomicU32,
    pub requested_height: AtomicU32,
    _pad0: u32,
    // Written by the producer.
    pub producer_heartbeat_ms: AtomicU64,
    pub latest_slot: AtomicU32,
    _pad1: u32,
    pub slots: [SlotHeader; SLOT_COUNT],
}

#[repr(C)]
pub struct SlotHeader {
    /// Odd while the producer is writing the slot.
    pub seq: AtomicU64,
    pub frame_number: AtomicU64,
    pub pixel: AtomicU32,
    pub width: AtomicU32,
    pub height: AtomicU32,
    pub bytes: AtomicU32,
}

const _: () = assert!(std::mem::size_of::<Header>() <= HEADER_BYTES);

pub fn now_ms() -> u64 {
    unsafe { GetTickCount64() }
}

fn is_fresh(heartbeat_ms: u64) -> bool {
    heartbeat_ms != 0 && now_ms().saturating_sub(heartbeat_ms) < HEARTBEAT_TIMEOUT_MS
}

/// A mapped view of the frame section.
struct Mapping {
    handle: HANDLE,
    view: MEMORY_MAPPED_VIEW_ADDRESS,
}

// The view is plain shared memory; all shared fields are atomics.
unsafe impl Send for Mapping {}
unsafe impl Sync for Mapping {}

impl Mapping {
    fn map(handle: HANDLE) -> Result<Self> {
        let view = unsafe { MapViewOfFile(handle, FILE_MAP_READ | FILE_MAP_WRITE, 0, 0, MAPPING_BYTES) };
        if view.Value.is_null() {
            let err = windows_core::Error::from_thread();
            unsafe { let _ = CloseHandle(handle); }
            return Err(err);
        }
        Ok(Self { handle, view })
    }

    fn header(&self) -> &Header {
        unsafe { &*(self.view.Value as *const Header) }
    }

    fn slot_ptr(&self, slot: usize) -> *mut u8 {
        debug_assert!(slot < SLOT_COUNT);
        unsafe { (self.view.Value as *mut u8).add(HEADER_BYTES + slot * MAX_FRAME_BYTES) }
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        unsafe {
            let _ = UnmapViewOfFile(self.view);
            let _ = CloseHandle(self.handle);
        }
    }
}

/// Auto-reset event the producer sets after publishing each frame.
struct FrameReady(HANDLE);

unsafe impl Send for FrameReady {}
unsafe impl Sync for FrameReady {}

impl Drop for FrameReady {
    fn drop(&mut self) {
        unsafe { let _ = CloseHandle(self.0); }
    }
}

fn event_name(mapping_name: &str) -> String {
    format!("{mapping_name}_FrameReady")
}

/// The media source's side: creates the section and reads frames.
pub struct Consumer {
    mapping: Mapping,
    /// `None` if the event could not be created; waiting then falls back to polling.
    frame_ready: Option<FrameReady>,
}

impl Consumer {
    /// Creates the section, or opens it if the app or another source instance
    /// already holds it.
    pub fn create() -> Result<Self> {
        Self::create_named(MAPPING_NAME)
    }

    /// Like [`Consumer::create`] with a custom section name (tests use `Local\`).
    pub fn create_named(name: &str) -> Result<Self> {
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(&HSTRING::from(SECURITY_DESCRIPTOR), SDDL_REVISION_1, &mut descriptor, None)?;
        }
        let attributes = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor.0,
            bInheritHandle: false.into(),
        };
        let size = MAPPING_BYTES as u64;
        let handle = unsafe {
            CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                Some(&attributes),
                PAGE_READWRITE,
                (size >> 32) as u32,
                size as u32,
                &HSTRING::from(name),
            )
        };
        let existed = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
        let frame_ready = unsafe { CreateEventW(Some(&attributes), false, false, &HSTRING::from(event_name(name))) }
            .ok()
            .map(FrameReady);
        unsafe { LocalFree(Some(HLOCAL(descriptor.0))) };
        let mapping = Mapping::map(handle?)?;

        let header = mapping.header();
        if !existed || header.magic.load(Ordering::Acquire) != MAGIC {
            header.version.store(VERSION, Ordering::Relaxed);
            header.magic.store(MAGIC, Ordering::Release);
        }
        Ok(Self { mapping, frame_ready })
    }

    /// Number of the newest complete frame, if any has been published.
    pub fn latest_frame_number(&self) -> Option<u64> {
        let header = self.mapping.header();
        let slot = header.latest_slot.load(Ordering::Acquire) as usize;
        let meta = header.slots.get(slot)?;
        let seq = meta.seq.load(Ordering::Acquire);
        (seq != 0 && seq % 2 == 0).then(|| meta.frame_number.load(Ordering::Relaxed))
    }

    /// Waits up to `timeout` for a frame other than `last` to be published.
    /// Returns whether one arrived.
    pub fn wait_for_frame_after(&self, last: Option<u64>, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            // Compare for inequality: frame numbers restart when the app restarts.
            if let Some(latest) = self.latest_frame_number() {
                if Some(latest) != last {
                    return true;
                }
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return false;
            }
            match &self.frame_ready {
                Some(event) => unsafe {
                    WaitForSingleObject(event.0, remaining.as_millis().max(1) as u32);
                },
                None => std::thread::sleep(remaining.min(Duration::from_millis(2))),
            }
        }
    }

    /// Publishes the negotiated format and refreshes the consumer heartbeat.
    pub fn request(&self, format: FrameFormat) {
        let header = self.mapping.header();
        header.requested_pixel.store(format.pixel as u32, Ordering::Relaxed);
        header.requested_width.store(format.width, Ordering::Relaxed);
        header.requested_height.store(format.height, Ordering::Relaxed);
        header.consumer_heartbeat_ms.store(now_ms(), Ordering::Release);
    }

    /// Whether a producer has sent a heartbeat recently.
    pub fn producer_alive(&self) -> bool {
        is_fresh(self.mapping.header().producer_heartbeat_ms.load(Ordering::Acquire))
    }

    /// Copies the newest complete frame of `format` into `dst` and returns its
    /// frame number, or `None` if no producer is alive or no matching frame is
    /// available.
    pub fn read_latest(&self, format: FrameFormat, dst: &mut [u8]) -> Option<u64> {
        let bytes = format.frame_bytes();
        if !self.producer_alive() || dst.len() < bytes || bytes > MAX_FRAME_BYTES {
            return None;
        }
        let header = self.mapping.header();

        for _ in 0..4 {
            let slot = header.latest_slot.load(Ordering::Acquire) as usize;
            if slot >= SLOT_COUNT {
                return None;
            }
            let meta = &header.slots[slot];
            let before = meta.seq.load(Ordering::Acquire);
            if before == 0 || before % 2 == 1 {
                continue;
            }
            let matches = meta.pixel.load(Ordering::Relaxed) == format.pixel as u32
                && meta.width.load(Ordering::Relaxed) == format.width
                && meta.height.load(Ordering::Relaxed) == format.height
                && meta.bytes.load(Ordering::Relaxed) as usize == bytes;
            if !matches {
                return None;
            }
            let frame_number = meta.frame_number.load(Ordering::Relaxed);
            unsafe { std::ptr::copy_nonoverlapping(self.mapping.slot_ptr(slot), dst.as_mut_ptr(), bytes) };
            fence(Ordering::Acquire);
            if meta.seq.load(Ordering::Relaxed) == before {
                return Some(frame_number);
            }
        }
        None
    }
}

/// The app's side: opens the section and writes frames.
pub struct Producer {
    mapping: Mapping,
    frame_ready: Option<FrameReady>,
    frame_number: u64,
}

impl Producer {
    /// Opens the section. Fails with `ERROR_FILE_NOT_FOUND` until a client has
    /// opened the camera and the source has created it.
    pub fn open() -> Result<Self> {
        Self::open_named(MAPPING_NAME)
    }

    /// Like [`Producer::open`] with a custom section name.
    pub fn open_named(name: &str) -> Result<Self> {
        let handle = unsafe {
            OpenFileMappingW((FILE_MAP_READ | FILE_MAP_WRITE).0, false, &HSTRING::from(name))?
        };
        let mapping = Mapping::map(handle)?;
        let header = mapping.header();
        if header.magic.load(Ordering::Acquire) != MAGIC || header.version.load(Ordering::Relaxed) != VERSION {
            return Err(windows::Win32::Foundation::E_UNEXPECTED.into());
        }
        let frame_ready = unsafe {
            OpenEventW(EVENT_MODIFY_STATE, false, &HSTRING::from(event_name(name)))
        }
        .ok()
        .map(FrameReady);
        Ok(Self { mapping, frame_ready, frame_number: 0 })
    }

    /// The format a client is currently consuming, if any client is.
    pub fn requested(&self) -> Option<FrameFormat> {
        let header = self.mapping.header();
        if !is_fresh(header.consumer_heartbeat_ms.load(Ordering::Acquire)) {
            return None;
        }
        let format = FrameFormat {
            pixel: PixelFormat::from_u32(header.requested_pixel.load(Ordering::Relaxed))?,
            width: header.requested_width.load(Ordering::Relaxed),
            height: header.requested_height.load(Ordering::Relaxed),
        };
        format.is_valid().then_some(format)
    }

    pub fn heartbeat(&self) {
        self.mapping.header().producer_heartbeat_ms.store(now_ms(), Ordering::Release);
    }

    /// Fills the next slot with `render` and publishes it as the latest frame.
    pub fn write_frame(&mut self, format: FrameFormat, render: impl FnOnce(&mut [u8])) {
        let bytes = format.frame_bytes();
        assert!(format.is_valid() && bytes <= MAX_FRAME_BYTES);
        let header = self.mapping.header();
        let slot = (header.latest_slot.load(Ordering::Relaxed) as usize + 1) % SLOT_COUNT;
        let meta = &header.slots[slot];

        meta.seq.fetch_add(1, Ordering::AcqRel);
        fence(Ordering::Release);
        let data = unsafe { std::slice::from_raw_parts_mut(self.mapping.slot_ptr(slot), bytes) };
        render(data);
        self.frame_number += 1;
        meta.frame_number.store(self.frame_number, Ordering::Relaxed);
        meta.pixel.store(format.pixel as u32, Ordering::Relaxed);
        meta.width.store(format.width, Ordering::Relaxed);
        meta.height.store(format.height, Ordering::Relaxed);
        meta.bytes.store(bytes as u32, Ordering::Relaxed);
        meta.seq.fetch_add(1, Ordering::Release);

        header.latest_slot.store(slot as u32, Ordering::Release);
        header.producer_heartbeat_ms.store(now_ms(), Ordering::Release);
        if let Some(event) = &self.frame_ready {
            unsafe { let _ = SetEvent(event.0); }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_name() -> String {
        format!("Local\\PatinaTest_{}_{:?}", std::process::id(), std::thread::current().id())
    }

    #[test]
    fn producer_frames_reach_consumer() {
        let name = test_name();
        let consumer = Consumer::create_named(&name).unwrap();
        let mut producer = Producer::open_named(&name).unwrap();
        let format = FrameFormat { pixel: PixelFormat::Nv12, width: 64, height: 32 };
        let mut dst = vec![0u8; format.frame_bytes()];

        // No consumer heartbeat yet, and no producer frames.
        assert_eq!(producer.requested(), None);
        assert_eq!(consumer.read_latest(format, &mut dst), None);

        consumer.request(format);
        assert_eq!(producer.requested(), Some(format));

        for n in 1..=5u8 {
            producer.write_frame(format, |buf| buf.fill(n));
            assert_eq!(consumer.read_latest(format, &mut dst), Some(n as u64));
            assert!(dst.iter().all(|&b| b == n));
        }
    }

    #[test]
    fn mismatched_format_is_not_delivered() {
        let name = test_name();
        let consumer = Consumer::create_named(&name).unwrap();
        let mut producer = Producer::open_named(&name).unwrap();
        let sent = FrameFormat { pixel: PixelFormat::Rgb32, width: 64, height: 32 };
        let wanted = FrameFormat { pixel: PixelFormat::Nv12, width: 64, height: 32 };
        producer.write_frame(sent, |buf| buf.fill(7));
        let mut dst = vec![0u8; sent.frame_bytes()];
        assert_eq!(consumer.read_latest(wanted, &mut dst), None);
        assert_eq!(consumer.read_latest(sent, &mut dst), Some(1));
    }

    #[test]
    fn consumer_wakes_when_a_new_frame_is_published() {
        let name = test_name();
        let consumer = Consumer::create_named(&name).unwrap();
        let mut producer = Producer::open_named(&name).unwrap();
        assert!(consumer.frame_ready.is_some() && producer.frame_ready.is_some());
        let format = FrameFormat { pixel: PixelFormat::Nv12, width: 64, height: 32 };

        assert!(!consumer.wait_for_frame_after(None, Duration::from_millis(20)));
        let writer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            producer.write_frame(format, |buf| buf.fill(1));
            producer
        });
        let started = Instant::now();
        assert!(consumer.wait_for_frame_after(None, Duration::from_secs(2)));
        assert!(started.elapsed() < Duration::from_secs(1));
        let mut producer = writer.join().unwrap();

        // Already delivered frame 1: waiting times out until frame 2 arrives.
        assert!(!consumer.wait_for_frame_after(Some(1), Duration::from_millis(20)));
        producer.write_frame(format, |buf| buf.fill(2));
        assert!(consumer.wait_for_frame_after(Some(1), Duration::from_millis(20)));
        assert_eq!(consumer.latest_frame_number(), Some(2));
    }

    #[test]
    fn open_fails_without_consumer() {
        assert!(Producer::open_named("Local\\PatinaTest_missing").is_err());
    }
}
