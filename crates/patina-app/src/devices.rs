//! Video capture device enumeration and selection.

use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::CoTaskMemFree;
use windows_core::{Result, GUID, PWSTR};

/// Prefix of our own virtual camera's device name; never captured from.
pub const OWN_CAMERA_PREFIX: &str = "Patina";

pub struct Device {
    pub name: String,
    pub activate: IMFActivate,
}

pub fn video_devices() -> Result<Vec<Device>> {
    let mut attributes = None;
    unsafe { MFCreateAttributes(&mut attributes, 1)? };
    let attributes = attributes.ok_or_else(|| windows_core::Error::from(MF_E_UNEXPECTED))?;
    unsafe { attributes.SetGUID(&MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE, &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID)? };

    let mut list: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    unsafe { MFEnumDeviceSources(&attributes, &mut list, &mut count)? };

    let mut devices = Vec::new();
    if !list.is_null() {
        let entries = unsafe { std::slice::from_raw_parts_mut(list, count as usize) };
        for entry in entries.iter_mut() {
            if let Some(activate) = entry.take() {
                let name = friendly_name(&activate).unwrap_or_else(|_| "<unnamed>".into());
                devices.push(Device { name, activate });
            }
        }
        unsafe { CoTaskMemFree(Some(list as *const _)) };
    }
    Ok(devices)
}

/// Picks the camera to capture from. With `wanted`, the first device whose
/// name contains it (case-insensitive). Otherwise the first device that does
/// not look like a virtual camera, falling back to any device but our own.
pub fn pick_camera(devices: Vec<Device>, wanted: Option<&str>) -> Option<Device> {
    let candidates = devices.into_iter().filter(|d| !d.name.starts_with(OWN_CAMERA_PREFIX));
    match wanted {
        Some(wanted) => {
            let wanted = wanted.to_lowercase();
            candidates.into_iter().find(|d| d.name.to_lowercase().contains(&wanted))
        }
        None => {
            let (virtual_like, physical): (Vec<_>, Vec<_>) = candidates.partition(|d| looks_virtual(&d.name));
            physical.into_iter().next().or_else(|| virtual_like.into_iter().next())
        }
    }
}

fn looks_virtual(name: &str) -> bool {
    let name = name.to_lowercase();
    ["virtual", "vcam", "obs", "snap camera", "xsplit", "nvidia broadcast"].iter().any(|hint| name.contains(hint))
}

fn friendly_name(activate: &IMFActivate) -> Result<String> {
    let mut value = PWSTR::null();
    let mut length = 0u32;
    unsafe {
        activate.GetAllocatedString(&MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME, &mut value, &mut length)?;
        let name = value.to_string().unwrap_or_default();
        CoTaskMemFree(Some(value.0 as *const _));
        Ok(name)
    }
}

pub fn subtype_name(subtype: &GUID) -> String {
    let known = [
        (MFVideoFormat_NV12, "NV12"),
        (MFVideoFormat_RGB32, "RGB32"),
        (MFVideoFormat_ARGB32, "ARGB32"),
        (MFVideoFormat_RGB24, "RGB24"),
        (MFVideoFormat_YUY2, "YUY2"),
        (MFVideoFormat_MJPG, "MJPG"),
    ];
    match known.iter().find(|(guid, _)| guid == subtype) {
        Some((_, name)) => (*name).into(),
        None => {
            // Most video subtypes are FOURCCs in the first GUID field.
            let fourcc = subtype.data1.to_le_bytes();
            if fourcc.iter().all(|b| b.is_ascii_graphic()) {
                String::from_utf8_lossy(&fourcc).into_owned()
            } else {
                format!("{subtype:?}")
            }
        }
    }
}
