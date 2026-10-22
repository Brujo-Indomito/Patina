//! The single video stream exposed by the media source.

use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{ERROR_SET_NOT_FOUND, E_POINTER, S_OK};
use windows::Win32::Media::KernelStreaming::{IKsControl, IKsControl_Impl, KSIDENTIFIER, PINNAME_VIDEO_CAPTURE};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows_core::{implement, IUnknown, Interface, Ref, Result, GUID, HRESULT};

use crate::attributes::delegate_attributes;
use crate::log::{log, trace};
use patina_shared::pattern::{Style, TestPattern};
use patina_shared::shm::Consumer;
use patina_shared::{FrameFormat, PixelFormat};

const FRAME_RATE: u32 = 30;
/// Frame duration in 100 ns units.
const FRAME_DURATION: i64 = 10_000_000 / FRAME_RATE as i64;
const FRAME_INTERVAL: Duration = Duration::from_nanos(1_000_000_000 / FRAME_RATE as u64);
/// How long to wait for the app's next frame before repeating the last one.
const APP_FRAME_TIMEOUT: Duration = Duration::from_millis(100);
/// Samples requested from the Frame Server's allocator.
const ALLOCATOR_SAMPLES: u32 = 10;

/// What a sample request waits for before building the frame.
enum Wait {
    AppFrame(Option<u64>),
    Sleep(Duration),
}

/// Resolutions offered to clients; the first NV12 entry is the default.
const RESOLUTIONS: [(u32, u32); 3] = [(1280, 720), (1920, 1080), (640, 360)];
const PIXELS: [PixelFormat; 2] = [PixelFormat::Nv12, PixelFormat::Rgb32];

#[implement(IMFMediaStream2, IKsControl, IMFAttributes)]
pub struct MediaStream {
    attributes: IMFAttributes,
    queue: IMFMediaEventQueue,
    descriptor: IMFStreamDescriptor,
    /// Shared-memory link to the app; `None` if the section could not be
    /// created. Thread-safe, so it lives outside the state lock and requests
    /// can wait on it without blocking other calls.
    consumer: Option<Consumer>,
    state: Mutex<StreamState>,
}

struct StreamState {
    /// Back-reference returned by `GetMediaSource`; cleared on shutdown to break the cycle.
    source: Option<IMFMediaSource>,
    stream_state: MF_STREAM_STATE,
    format: FrameFormat,
    /// Fallback shown while no app is publishing frames.
    pattern: TestPattern,
    /// Whether the last frame came from the app (for logging transitions).
    live: bool,
    /// Number of the last app frame delivered, to wait for the next one.
    last_app_frame: Option<u64>,
    frames_sent: u64,
    /// When the next frame may be delivered; paces output to `FRAME_RATE`.
    next_due: Option<Instant>,
    /// Sample allocator provided by the Frame Server through
    /// `IMFSampleAllocatorControl`.
    allocator: Option<IMFVideoSampleAllocator>,
    /// Format the allocator was last initialized for.
    allocator_format: Option<FrameFormat>,
    /// Requests served from plain memory because the allocator had no sample.
    allocator_misses: u64,
    /// Staging frame used when the allocator's buffer pitch differs from the stride.
    staging: Vec<u8>,
    shutdown: bool,
}

impl MediaStream {
    pub fn new(index: u32, source: IMFMediaSource) -> Result<Self> {
        let media_types = PIXELS
            .iter()
            .flat_map(|&pixel| RESOLUTIONS.iter().map(move |&(width, height)| FrameFormat { pixel, width, height }))
            .map(|format| create_media_type(format).map(Some))
            .collect::<Result<Vec<_>>>()?;

        let descriptor = unsafe { MFCreateStreamDescriptor(index, &media_types)? };
        let handler = unsafe { descriptor.GetMediaTypeHandler()? };
        unsafe { handler.SetCurrentMediaType(media_types[0].as_ref())? };

        let mut attributes = None;
        unsafe { MFCreateAttributes(&mut attributes, 8)? };
        let attributes = attributes.ok_or_else(|| windows_core::Error::from(E_POINTER))?;
        unsafe {
            attributes.SetGUID(&MF_DEVICESTREAM_STREAM_CATEGORY, &PINNAME_VIDEO_CAPTURE)?;
            attributes.SetUINT32(&MF_DEVICESTREAM_STREAM_ID, index)?;
            attributes.SetUINT32(&MF_DEVICESTREAM_FRAMESERVER_SHARED, 1)?;
            attributes.SetUINT32(&MF_DEVICESTREAM_ATTRIBUTE_FRAMESOURCE_TYPES, MFFrameSourceTypes_Color.0 as u32)?;
            // The stream descriptor carries the same attributes the Frame Server inspects.
            attributes.CopyAllItems(&descriptor)?;
        }

        let queue = unsafe { MFCreateEventQueue()? };
        let (width, height) = RESOLUTIONS[0];

        let consumer = match Consumer::create() {
            Ok(consumer) => {
                log!("shared frame section ready");
                Some(consumer)
            }
            Err(err) => {
                log!("shared frame section unavailable, showing idle pattern only: {err}");
                None
            }
        };

        Ok(Self {
            attributes,
            queue,
            descriptor,
            consumer,
            state: Mutex::new(StreamState {
                source: Some(source),
                stream_state: MF_STREAM_STATE_STOPPED,
                format: FrameFormat { pixel: PIXELS[0], width, height },
                pattern: TestPattern::new(Style::Idle),
                live: false,
                last_app_frame: None,
                frames_sent: 0,
                next_due: None,
                allocator: None,
                allocator_format: None,
                allocator_misses: 0,
                staging: Vec::new(),
                shutdown: false,
            }),
        })
    }

    pub fn descriptor(&self) -> &IMFStreamDescriptor {
        &self.descriptor
    }

    pub fn attributes(&self) -> &IMFAttributes {
        &self.attributes
    }

    fn lock(&self) -> MutexGuard<'_, StreamState> {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Starts streaming using the media type currently selected on `selected`,
    /// the stream descriptor from the presentation descriptor passed to `Start`.
    pub fn start(&self, selected: &IMFStreamDescriptor, time: i64) -> Result<()> {
        let format = current_format(selected).or_else(|_| current_format(&self.descriptor))?;
        {
            let mut state = self.lock();
            if state.shutdown {
                return Err(MF_E_SHUTDOWN.into());
            }
            state.format = format;
            state.stream_state = MF_STREAM_STATE_RUNNING;
            state.next_due = None;
        }
        if let Some(consumer) = &self.consumer {
            consumer.request(format);
        }
        log!("stream start: {format}");
        let value = PROPVARIANT::from(time);
        unsafe { self.queue.QueueEventParamVar(MEStreamStarted.0 as u32, &GUID::zeroed(), S_OK, &value) }
    }

    pub fn stop(&self) -> Result<()> {
        {
            let mut state = self.lock();
            if state.shutdown {
                return Err(MF_E_SHUTDOWN.into());
            }
            state.stream_state = MF_STREAM_STATE_STOPPED;
        }
        log!("stream stop");
        unsafe { self.queue.QueueEventParamVar(MEStreamStopped.0 as u32, &GUID::zeroed(), S_OK, &PROPVARIANT::default()) }
    }

    pub fn shutdown(&self) {
        let mut state = self.lock();
        state.shutdown = true;
        state.source = None;
        release_allocator(&mut state);
        let _ = unsafe { self.queue.Shutdown() };
    }

    /// Takes the allocator the Frame Server provides for this stream.
    pub fn set_allocator(&self, allocator: IMFVideoSampleAllocator) -> Result<()> {
        let mut state = self.lock();
        if state.shutdown {
            return Err(MF_E_SHUTDOWN.into());
        }
        release_allocator(&mut state);
        state.allocator = Some(allocator);
        Ok(())
    }

    /// Allocates a sample from the Frame Server's allocator, initializing it
    /// for the current format first. `None` if there is no allocator or it
    /// cannot supply a sample; the caller then uses a plain memory buffer.
    fn allocate_sample(state: &mut StreamState) -> Option<IMFSample> {
        let allocator = state.allocator.clone()?;
        if state.allocator_format != Some(state.format) {
            if state.allocator_format.take().is_some() {
                let _ = unsafe { allocator.UninitializeSampleAllocator() };
            }
            let initialized = create_media_type(state.format)
                .and_then(|media_type| unsafe { allocator.InitializeSampleAllocator(ALLOCATOR_SAMPLES, &media_type) });
            match initialized {
                Ok(()) => {
                    log!("sample allocator initialized for {}", state.format);
                    state.allocator_format = Some(state.format);
                }
                Err(err) => {
                    log!("sample allocator initialization failed, using memory buffers: {err}");
                    state.allocator = None;
                    return None;
                }
            }
        }
        match unsafe { allocator.AllocateSample() } {
            Ok(sample) => Some(sample),
            Err(err) => {
                if state.allocator_misses % 300 == 0 {
                    log!("sample allocator had no sample ({} so far): {err}", state.allocator_misses + 1);
                }
                state.allocator_misses += 1;
                None
            }
        }
    }

    fn create_sample(consumer: Option<&Consumer>, state: &mut StreamState) -> Result<IMFSample> {
        let format = state.format;
        let size = format.frame_bytes();
        let sample = match Self::allocate_sample(state) {
            Some(sample) => sample,
            None => {
                let sample = unsafe { MFCreateSample()? };
                unsafe { sample.AddBuffer(&MFCreateMemoryBuffer(size as u32)?)? };
                sample
            }
        };
        let buffer = unsafe { sample.GetBufferByIndex(0)? };

        let length = match buffer.cast::<IMF2DBuffer>() {
            Ok(buffer_2d) => {
                let mut scanline = std::ptr::null_mut();
                let mut pitch = 0i32;
                unsafe { buffer_2d.Lock2D(&mut scanline, &mut pitch)? };
                let stride = format.stride() as usize;
                let rows = size / stride;
                if pitch as usize == stride {
                    Self::fill(consumer, state, unsafe { std::slice::from_raw_parts_mut(scanline, size) });
                } else {
                    // Render contiguously, then copy each row at the buffer's pitch.
                    let mut staging = std::mem::take(&mut state.staging);
                    staging.resize(size, 0);
                    Self::fill(consumer, state, &mut staging);
                    for (row, line) in staging.chunks_exact(stride).enumerate() {
                        unsafe { std::ptr::copy_nonoverlapping(line.as_ptr(), scanline.offset(row as isize * pitch as isize), stride) };
                    }
                    state.staging = staging;
                }
                unsafe { buffer_2d.Unlock2D()? };
                (rows * pitch.unsigned_abs() as usize).min(unsafe { buffer.GetMaxLength()? } as usize)
            }
            Err(_) => {
                let mut data = std::ptr::null_mut();
                let mut max_length = 0u32;
                unsafe { buffer.Lock(&mut data, Some(&mut max_length), None)? };
                if (max_length as usize) < size {
                    unsafe { buffer.Unlock()? };
                    return Err(MF_E_BUFFERTOOSMALL.into());
                }
                Self::fill(consumer, state, unsafe { std::slice::from_raw_parts_mut(data, size) });
                unsafe { buffer.Unlock()? };
                size
            }
        };

        unsafe {
            buffer.SetCurrentLength(length as u32)?;
            sample.SetSampleTime(MFGetSystemTime())?;
            sample.SetSampleDuration(FRAME_DURATION)?;
        }
        Ok(sample)
    }

    /// Writes the next frame into `pixels`: the app's newest frame, or the idle pattern.
    fn fill(consumer: Option<&Consumer>, state: &mut StreamState, pixels: &mut [u8]) {
        let format = state.format;
        let app_frame = consumer.and_then(|consumer| {
            consumer.request(format);
            consumer.read_latest(format, pixels)
        });
        let live = app_frame.is_some();
        if live {
            state.last_app_frame = app_frame;
        } else {
            state.pattern.render(&format, pixels);
        }
        if live != state.live {
            log!("{}", if live { "receiving frames from app" } else { "no app frames, showing idle pattern" });
            state.live = live;
        }
    }
}

fn release_allocator(state: &mut StreamState) {
    if let Some(allocator) = state.allocator.take() {
        if state.allocator_format.take().is_some() {
            let _ = unsafe { allocator.UninitializeSampleAllocator() };
        }
    }
}

fn create_media_type(format: FrameFormat) -> Result<IMFMediaType> {
    unsafe {
        let media_type = MFCreateMediaType()?;
        media_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        media_type.SetGUID(&MF_MT_SUBTYPE, &subtype_of(format.pixel))?;
        media_type.SetUINT64(&MF_MT_FRAME_SIZE, pack(format.width, format.height))?;
        media_type.SetUINT64(&MF_MT_FRAME_RATE, pack(FRAME_RATE, 1))?;
        media_type.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
        media_type.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        media_type.SetUINT32(&MF_MT_ALL_SAMPLES_INDEPENDENT, 1)?;
        media_type.SetUINT32(&MF_MT_FIXED_SIZE_SAMPLES, 1)?;
        media_type.SetUINT32(&MF_MT_SAMPLE_SIZE, format.frame_bytes() as u32)?;
        media_type.SetUINT32(&MF_MT_DEFAULT_STRIDE, format.stride())?;
        Ok(media_type)
    }
}

fn current_format(descriptor: &IMFStreamDescriptor) -> Result<FrameFormat> {
    unsafe {
        let media_type = descriptor.GetMediaTypeHandler()?.GetCurrentMediaType()?;
        let subtype = media_type.GetGUID(&MF_MT_SUBTYPE)?;
        let size = media_type.GetUINT64(&MF_MT_FRAME_SIZE)?;
        let format = pixel_of(&subtype).map(|pixel| FrameFormat { pixel, width: (size >> 32) as u32, height: size as u32 });
        match format {
            Some(format) if format.is_valid() => Ok(format),
            _ => Err(MF_E_INVALIDMEDIATYPE.into()),
        }
    }
}

fn subtype_of(pixel: PixelFormat) -> GUID {
    match pixel {
        PixelFormat::Nv12 => MFVideoFormat_NV12,
        PixelFormat::Rgb32 => MFVideoFormat_RGB32,
    }
}

fn pixel_of(subtype: &GUID) -> Option<PixelFormat> {
    PIXELS.into_iter().find(|&pixel| subtype_of(pixel) == *subtype)
}

fn pack(high: u32, low: u32) -> u64 {
    ((high as u64) << 32) | low as u64
}

impl IMFMediaEventGenerator_Impl for MediaStream_Impl {
    fn GetEvent(&self, flags: MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS) -> Result<IMFMediaEvent> {
        unsafe { self.queue.GetEvent(flags.0) }
    }

    fn BeginGetEvent(&self, callback: Ref<IMFAsyncCallback>, state: Ref<IUnknown>) -> Result<()> {
        unsafe { self.queue.BeginGetEvent(callback.as_ref(), state.as_ref()) }
    }

    fn EndGetEvent(&self, result: Ref<IMFAsyncResult>) -> Result<IMFMediaEvent> {
        unsafe { self.queue.EndGetEvent(result.as_ref()) }
    }

    fn QueueEvent(&self, met: u32, extended_type: *const GUID, status: HRESULT, value: *const PROPVARIANT) -> Result<()> {
        unsafe { self.queue.QueueEventParamVar(met, extended_type, status, value) }
    }
}

impl IMFMediaStream_Impl for MediaStream_Impl {
    fn GetMediaSource(&self) -> Result<IMFMediaSource> {
        let state = self.lock();
        if state.shutdown {
            return Err(MF_E_SHUTDOWN.into());
        }
        state.source.clone().ok_or_else(|| MF_E_SHUTDOWN.into())
    }

    fn GetStreamDescriptor(&self) -> Result<IMFStreamDescriptor> {
        if self.lock().shutdown {
            return Err(MF_E_SHUTDOWN.into());
        }
        Ok(self.descriptor.clone())
    }

    fn RequestSample(&self, token: Ref<IUnknown>) -> Result<()> {
        // Hold each request until a frame is due, like a hardware camera.
        // While the app publishes, follow its cadence: wait for the next app
        // frame, repeating the last one if none arrives in APP_FRAME_TIMEOUT.
        // Otherwise pace the idle pattern at FRAME_RATE. A client that falls
        // behind gets the next frame immediately, without catch-up bursts.
        let consumer = self.consumer.as_ref().filter(|consumer| consumer.producer_alive());
        let wait = {
            let mut state = self.lock();
            if state.shutdown {
                return Err(MF_E_SHUTDOWN.into());
            }
            let now = Instant::now();
            if consumer.is_some() {
                state.next_due = None;
                Wait::AppFrame(state.last_app_frame)
            } else {
                let due = state.next_due.unwrap_or(now).max(now);
                state.next_due = Some(due + FRAME_INTERVAL);
                Wait::Sleep(due - now)
            }
        };
        match (wait, consumer) {
            (Wait::AppFrame(last), Some(consumer)) => {
                consumer.wait_for_frame_after(last, APP_FRAME_TIMEOUT);
            }
            (Wait::Sleep(duration), _) if !duration.is_zero() => std::thread::sleep(duration),
            _ => {}
        }

        let sample = {
            let mut state = self.lock();
            if state.shutdown {
                return Err(MF_E_SHUTDOWN.into());
            }
            if state.stream_state != MF_STREAM_STATE_RUNNING {
                return Err(MF_E_MEDIA_SOURCE_WRONGSTATE.into());
            }
            let sample = trace("RequestSample", MediaStream::create_sample(self.consumer.as_ref(), &mut state))?;
            state.frames_sent += 1;
            if state.frames_sent == 1 || state.frames_sent % 300 == 0 {
                log!("delivered {} frames", state.frames_sent);
            }
            sample
        };

        if let Some(token) = token.as_ref() {
            unsafe { sample.SetUnknown(&MFSampleExtension_Token, token)? };
        }
        unsafe { self.queue.QueueEventParamUnk(MEMediaSample.0 as u32, &GUID::zeroed(), S_OK, &sample) }
    }
}

impl IMFMediaStream2_Impl for MediaStream_Impl {
    fn SetStreamState(&self, value: MF_STREAM_STATE) -> Result<()> {
        log!("SetStreamState({})", value.0);
        let mut state = self.lock();
        if state.shutdown {
            return Err(MF_E_SHUTDOWN.into());
        }
        state.stream_state = value;
        Ok(())
    }

    fn GetStreamState(&self) -> Result<MF_STREAM_STATE> {
        let state = self.lock();
        if state.shutdown {
            return Err(MF_E_SHUTDOWN.into());
        }
        Ok(state.stream_state)
    }
}

impl IKsControl_Impl for MediaStream_Impl {
    fn KsProperty(&self, _: *const KSIDENTIFIER, _: u32, _: *mut core::ffi::c_void, _: u32, _: *mut u32) -> Result<()> {
        Err(HRESULT::from_win32(ERROR_SET_NOT_FOUND.0).into())
    }

    fn KsMethod(&self, _: *const KSIDENTIFIER, _: u32, _: *mut core::ffi::c_void, _: u32, _: *mut u32) -> Result<()> {
        Err(HRESULT::from_win32(ERROR_SET_NOT_FOUND.0).into())
    }

    fn KsEvent(&self, _: *const KSIDENTIFIER, _: u32, _: *mut core::ffi::c_void, _: u32, _: *mut u32) -> Result<()> {
        Err(HRESULT::from_win32(ERROR_SET_NOT_FOUND.0).into())
    }
}

delegate_attributes!(MediaStream_Impl, attributes);
