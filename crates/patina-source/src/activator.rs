//! `IMFActivate` object created by the class factory.
//!
//! The Frame Server creates this object from the registered CLSID and calls
//! `ActivateObject` to obtain the media source.

use std::sync::Mutex;

use windows::Win32::Foundation::E_POINTER;
use windows::Win32::Media::MediaFoundation::{
    IMFActivate, IMFActivate_Impl, IMFAttributes, MFCreateAttributes, MFT_TRANSFORM_CLSID_Attribute,
    MF_VIRTUALCAMERA_PROVIDE_ASSOCIATED_CAMERA_SOURCES,
};
use windows_core::{implement, ComObject, Result, GUID};

use crate::attributes::delegate_attributes;
use crate::log::{log, trace};
use crate::source::{self, MediaSource};
use crate::CLSID_PATINA;

#[implement(IMFActivate, IMFAttributes)]
pub struct Activator {
    attributes: IMFAttributes,
    source: Mutex<Option<ComObject<MediaSource>>>,
}

impl Activator {
    pub fn new() -> Result<Self> {
        let mut attributes = None;
        unsafe { MFCreateAttributes(&mut attributes, 4)? };
        let attributes = attributes.ok_or_else(|| windows_core::Error::from(E_POINTER))?;
        unsafe {
            attributes.SetUINT32(&MF_VIRTUALCAMERA_PROVIDE_ASSOCIATED_CAMERA_SOURCES, 1)?;
            attributes.SetGUID(&MFT_TRANSFORM_CLSID_Attribute, &CLSID_PATINA)?;
        }
        Ok(Self { attributes, source: Mutex::new(None) })
    }
}

impl IMFActivate_Impl for Activator_Impl {
    fn ActivateObject(&self, riid: *const GUID, ppv: *mut *mut core::ffi::c_void) -> Result<()> {
        if ppv.is_null() {
            return Err(E_POINTER.into());
        }
        let mut slot = self.source.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if slot.is_none() {
            *slot = Some(trace("MediaSource::create", MediaSource::create(&self.attributes))?);
        }
        log!("ActivateObject");
        trace("ActivateObject", source::query(slot.as_ref().unwrap(), riid, ppv))
    }

    fn ShutdownObject(&self) -> Result<()> {
        log!("ShutdownObject");
        if let Some(source) = self.source.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).take() {
            source::shutdown(&source);
        }
        Ok(())
    }

    fn DetachObject(&self) -> Result<()> {
        log!("DetachObject");
        self.source.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).take();
        Ok(())
    }
}

delegate_attributes!(Activator_Impl, attributes);
