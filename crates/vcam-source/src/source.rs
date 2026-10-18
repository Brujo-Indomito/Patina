//! The media source the Frame Server activates for the virtual camera.

use std::sync::{Mutex, MutexGuard};

use windows::Win32::Foundation::{ERROR_SET_NOT_FOUND, E_INVALIDARG, E_POINTER, S_OK};
use windows::Win32::Media::KernelStreaming::{IKsControl, IKsControl_Impl, KSCAMERAPROFILE_Legacy, KSIDENTIFIER};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows_core::{implement, w, ComObject, IUnknown, Interface, Ref, Result, BOOL, GUID, HRESULT, PCWSTR};

use crate::attributes::delegate_attributes;
use crate::log::{log, trace};
use crate::stream::MediaStream;

const STREAM_ID: u32 = 0;

#[implement(IMFMediaSourceEx, IMFGetService, IKsControl, IMFSampleAllocatorControl, IMFAttributes)]
pub struct MediaSource {
    attributes: IMFAttributes,
    queue: IMFMediaEventQueue,
    state: Mutex<SourceState>,
}

#[derive(Default)]
struct SourceState {
    descriptor: Option<IMFPresentationDescriptor>,
    stream: Option<ComObject<MediaStream>>,
    stream_active: bool,
    shutdown: bool,
}

impl MediaSource {
    /// Creates the source, copying the activator's attributes into the source attributes.
    pub fn create(activator_attributes: &IMFAttributes) -> Result<ComObject<MediaSource>> {
        let mut attributes = None;
        unsafe { MFCreateAttributes(&mut attributes, 8)? };
        let attributes = attributes.ok_or_else(|| windows_core::Error::from(E_POINTER))?;
        unsafe { activator_attributes.CopyAllItems(&attributes)? };
        match sensor_profiles() {
            Ok(profiles) => unsafe { attributes.SetUnknown(&MF_DEVICEMFT_SENSORPROFILE_COLLECTION, &profiles)? },
            Err(err) => log!("sensor profiles unavailable: {err}"),
        }

        let source = ComObject::new(MediaSource {
            attributes,
            queue: unsafe { MFCreateEventQueue()? },
            state: Mutex::new(SourceState::default()),
        });

        let stream = ComObject::new(MediaStream::new(STREAM_ID, source.to_interface::<IMFMediaSourceEx>().into())?);
        let descriptor = unsafe { MFCreatePresentationDescriptor(Some(&[Some(stream.descriptor().clone())]))? };
        unsafe { descriptor.SelectStream(0)? };

        {
            let mut state = source.lock();
            state.descriptor = Some(descriptor);
            state.stream = Some(stream);
        }

        log!("media source created");
        Ok(source)
    }

    fn lock(&self) -> MutexGuard<'_, SourceState> {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn live_state(&self) -> Result<MutexGuard<'_, SourceState>> {
        let state = self.lock();
        if state.shutdown {
            Err(MF_E_SHUTDOWN.into())
        } else {
            Ok(state)
        }
    }

    fn check_live(&self) -> Result<()> {
        self.live_state().map(drop)
    }

    fn start(&self, descriptor: Option<&IMFPresentationDescriptor>, time_format: *const GUID) -> Result<()> {
        if !time_format.is_null() && unsafe { *time_format } != GUID::zeroed() {
            return Err(MF_E_UNSUPPORTED_TIME_FORMAT.into());
        }
        let descriptor = descriptor.ok_or_else(|| windows_core::Error::from(E_INVALIDARG))?;

        let mut state = self.live_state()?;
        let stream = state.stream.clone().ok_or_else(|| windows_core::Error::from(MF_E_SHUTDOWN))?;
        let time = unsafe { MFGetSystemTime() };

        let count = unsafe { descriptor.GetStreamDescriptorCount()? };
        for index in 0..count {
            let mut selected = BOOL(0);
            let mut stream_descriptor = None;
            unsafe { descriptor.GetStreamDescriptorByIndex(index, &mut selected, &mut stream_descriptor)? };
            let stream_descriptor = stream_descriptor.ok_or_else(|| windows_core::Error::from(E_POINTER))?;
            if unsafe { stream_descriptor.GetStreamIdentifier()? } != STREAM_ID {
                return Err(E_INVALIDARG.into());
            }

            if selected.as_bool() {
                log!("Start: stream {}", if state.stream_active { "already active" } else { "new" });
                let event = if state.stream_active { MEUpdatedStream } else { MENewStream };
                let unknown: IUnknown = stream.to_interface();
                unsafe { self.queue.QueueEventParamUnk(event.0 as u32, &GUID::zeroed(), S_OK, &unknown)? };
                stream.start(&stream_descriptor, time)?;
                state.stream_active = true;
            } else if state.stream_active {
                stream.stop()?;
                state.stream_active = false;
            }
        }

        log!("source started");
        let value = PROPVARIANT::from(time);
        unsafe { self.queue.QueueEventParamVar(MESourceStarted.0 as u32, &GUID::zeroed(), S_OK, &value) }
    }
}

impl IMFMediaEventGenerator_Impl for MediaSource_Impl {
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

impl IMFMediaSource_Impl for MediaSource_Impl {
    fn GetCharacteristics(&self) -> Result<u32> {
        self.check_live()?;
        Ok(MFMEDIASOURCE_IS_LIVE.0 as u32)
    }

    fn CreatePresentationDescriptor(&self) -> Result<IMFPresentationDescriptor> {
        let state = self.live_state()?;
        let descriptor = state.descriptor.as_ref().ok_or_else(|| windows_core::Error::from(MF_E_NOT_INITIALIZED))?;
        trace("CreatePresentationDescriptor", unsafe { descriptor.Clone() })
    }

    fn Start(&self, descriptor: Ref<IMFPresentationDescriptor>, time_format: *const GUID, _start: *const PROPVARIANT) -> Result<()> {
        trace("Start", self.start(descriptor.as_ref(), time_format))
    }

    fn Stop(&self) -> Result<()> {
        let mut state = self.live_state()?;
        if state.stream_active {
            if let Some(stream) = &state.stream {
                stream.stop()?;
            }
            state.stream_active = false;
        }
        log!("source stopped");
        unsafe { self.queue.QueueEventParamVar(MESourceStopped.0 as u32, &GUID::zeroed(), S_OK, &PROPVARIANT::default()) }
    }

    fn Pause(&self) -> Result<()> {
        // Live sources cannot pause.
        Err(MF_E_INVALID_STATE_TRANSITION.into())
    }

    fn Shutdown(&self) -> Result<()> {
        let mut state = self.lock();
        if state.shutdown {
            return Ok(());
        }
        state.shutdown = true;
        state.stream_active = false;
        if let Some(stream) = state.stream.take() {
            stream.shutdown();
        }
        state.descriptor = None;
        let _ = unsafe { self.queue.Shutdown() };
        log!("source shut down");
        Ok(())
    }
}

impl IMFMediaSourceEx_Impl for MediaSource_Impl {
    fn GetSourceAttributes(&self) -> Result<IMFAttributes> {
        self.check_live()?;
        Ok(self.attributes.clone())
    }

    fn GetStreamAttributes(&self, stream_id: u32) -> Result<IMFAttributes> {
        let state = self.live_state()?;
        match &state.stream {
            Some(stream) if stream_id == STREAM_ID => Ok(stream.attributes().clone()),
            _ => Err(MF_E_INVALIDSTREAMNUMBER.into()),
        }
    }

    fn SetD3DManager(&self, manager: Ref<IUnknown>) -> Result<()> {
        // Frames are produced in system memory for now; the Frame Server uploads them.
        log!("SetD3DManager({})", if manager.as_ref().is_some() { "device" } else { "null" });
        self.check_live()?;
        Ok(())
    }
}

impl IMFGetService_Impl for MediaSource_Impl {
    fn GetService(&self, _service: *const GUID, _riid: *const GUID, object: *mut *mut core::ffi::c_void) -> Result<()> {
        if !object.is_null() {
            unsafe { *object = std::ptr::null_mut() };
        }
        Err(MF_E_UNSUPPORTED_SERVICE.into())
    }
}

impl IKsControl_Impl for MediaSource_Impl {
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

impl IMFSampleAllocatorControl_Impl for MediaSource_Impl {
    fn SetDefaultAllocator(&self, stream_id: u32, allocator: Ref<IUnknown>) -> Result<()> {
        let state = self.live_state()?;
        let stream = match &state.stream {
            Some(stream) if stream_id == STREAM_ID => stream,
            _ => return Err(MF_E_INVALIDSTREAMNUMBER.into()),
        };
        let allocator: IMFVideoSampleAllocator = allocator.ok()?.cast()?;
        log!("SetDefaultAllocator");
        stream.set_allocator(allocator)
    }

    fn GetAllocatorUsage(&self, stream_id: u32, input_stream_id: *mut u32, usage: *mut MFSampleAllocatorUsage) -> Result<()> {
        if input_stream_id.is_null() || usage.is_null() {
            return Err(E_POINTER.into());
        }
        self.check_live()?;
        if stream_id != STREAM_ID {
            return Err(MF_E_INVALIDSTREAMNUMBER.into());
        }
        // Ask the Frame Server for its allocator, as Microsoft's VirtualCamera sample does.
        unsafe {
            *input_stream_id = stream_id;
            *usage = MFSampleAllocatorUsage_UsesProvidedAllocator;
        }
        Ok(())
    }
}

delegate_attributes!(MediaSource_Impl, attributes);

/// One legacy profile allowing every media type up to 30 fps, as in
/// Microsoft's VirtualCamera sample.
fn sensor_profiles() -> Result<IMFSensorProfileCollection> {
    unsafe {
        let collection = MFCreateSensorProfileCollection()?;
        let profile = MFCreateSensorProfile(&KSCAMERAPROFILE_Legacy, 0, PCWSTR::null())?;
        profile.AddProfileFilter(STREAM_ID, w!("((RES==;FRT<=30,1;SUT==))"))?;
        collection.AddProfile(&profile)?;
        Ok(collection)
    }
}

/// Shuts down a source held by the activator.
pub fn shutdown(source: &ComObject<MediaSource>) {
    let source: IMFMediaSource = source.to_interface::<IMFMediaSourceEx>().into();
    let _ = unsafe { source.Shutdown() };
}

/// Queries `source` for `riid`, as `IMFActivate::ActivateObject` requires.
pub fn query(source: &ComObject<MediaSource>, riid: *const GUID, ppv: *mut *mut core::ffi::c_void) -> Result<()> {
    let unknown: IUnknown = source.to_interface();
    unsafe { unknown.query(riid, ppv).ok() }
}
