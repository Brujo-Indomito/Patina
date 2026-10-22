//! Patina media source: a COM in-process server that the Windows Camera
//! Frame Server loads to back a virtual camera created with
//! `MFCreateVirtualCamera`.
//!
//! Exports the standard COM entry points; `regsvr32` registers the CLSID under
//! HKLM so the Frame Server service can create it.

mod activator;
mod attributes;
mod log;
mod source;
mod stream;

use std::ffi::c_void;
use std::sync::atomic::{AtomicPtr, Ordering};

use windows::Win32::Foundation::{
    CLASS_E_CLASSNOTAVAILABLE, CLASS_E_NOAGGREGATION, E_POINTER, HINSTANCE, HMODULE, S_FALSE, S_OK, WIN32_ERROR,
};
use windows::Win32::System::Com::{IClassFactory, IClassFactory_Impl};
use windows::Win32::System::LibraryLoader::GetModuleFileNameW;
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteTreeW, RegSetValueExW, HKEY, HKEY_LOCAL_MACHINE, KEY_WRITE,
    REG_OPTION_NON_VOLATILE, REG_SZ,
};
use windows::Win32::System::SystemServices::DLL_PROCESS_ATTACH;
use windows_core::{implement, ComObject, IUnknown, Interface, Ref, Result, BOOL, GUID, HRESULT, HSTRING, PCWSTR};

use crate::activator::Activator;
use crate::log::log;

/// CLSID of the media source activator. The host app passes this to `MFCreateVirtualCamera`.
pub const CLSID_PATINA: GUID = GUID::from_u128(0xE3C40D88_220A_4FD1_94D8_BC3F22833AFA);
const FRIENDLY_NAME: &str = "Patina Source";

static MODULE: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());

#[no_mangle]
extern "system" fn DllMain(instance: HINSTANCE, reason: u32, _reserved: *mut c_void) -> BOOL {
    if reason == DLL_PROCESS_ATTACH {
        MODULE.store(instance.0, Ordering::Relaxed);
    }
    BOOL(1)
}

#[implement(IClassFactory)]
struct ClassFactory;

impl IClassFactory_Impl for ClassFactory_Impl {
    fn CreateInstance(&self, outer: Ref<IUnknown>, riid: *const GUID, ppv: *mut *mut c_void) -> Result<()> {
        if ppv.is_null() {
            return Err(E_POINTER.into());
        }
        unsafe { *ppv = std::ptr::null_mut() };
        if outer.as_ref().is_some() {
            return Err(CLASS_E_NOAGGREGATION.into());
        }
        let activator: IUnknown = ComObject::new(Activator::new()?).into_interface();
        log!("activator created");
        unsafe { activator.query(riid, ppv).ok() }
    }

    fn LockServer(&self, _lock: BOOL) -> Result<()> {
        Ok(())
    }
}

#[no_mangle]
unsafe extern "system" fn DllGetClassObject(clsid: *const GUID, riid: *const GUID, ppv: *mut *mut c_void) -> HRESULT {
    if ppv.is_null() || clsid.is_null() {
        return E_POINTER;
    }
    *ppv = std::ptr::null_mut();
    if *clsid != CLSID_PATINA {
        return CLASS_E_CLASSNOTAVAILABLE;
    }
    let factory: IUnknown = ComObject::new(ClassFactory).into_interface();
    factory.query(riid, ppv)
}

#[no_mangle]
extern "system" fn DllCanUnloadNow() -> HRESULT {
    // The Frame Server may keep objects alive across sessions; stay loaded.
    S_FALSE
}

#[no_mangle]
extern "system" fn DllRegisterServer() -> HRESULT {
    match register() {
        Ok(()) => S_OK,
        Err(err) => err.code(),
    }
}

#[no_mangle]
extern "system" fn DllUnregisterServer() -> HRESULT {
    let key = HSTRING::from(clsid_key());
    match unsafe { RegDeleteTreeW(HKEY_LOCAL_MACHINE, &key) }.ok() {
        Ok(()) => S_OK,
        Err(err) => err.code(),
    }
}

fn clsid_key() -> String {
    format!("SOFTWARE\\Classes\\CLSID\\{{{:?}}}", CLSID_PATINA)
}

fn module_path() -> Result<String> {
    let module = HMODULE(MODULE.load(Ordering::Relaxed));
    let mut buf = vec![0u16; 32 * 1024];
    let len = unsafe { GetModuleFileNameW(Some(module), &mut buf) } as usize;
    if len == 0 {
        return Err(windows_core::Error::from_thread());
    }
    Ok(String::from_utf16_lossy(&buf[..len]))
}

fn register() -> Result<()> {
    let path = module_path()?;
    let key = create_key(&clsid_key())?;
    let result = set_string(key, "", FRIENDLY_NAME).and_then(|_| {
        let inproc = create_key(&format!("{}\\InprocServer32", clsid_key()))?;
        let result = set_string(inproc, "", &path).and_then(|_| set_string(inproc, "ThreadingModel", "Both"));
        unsafe { let _ = RegCloseKey(inproc); }
        result
    });
    unsafe { let _ = RegCloseKey(key); }
    result
}

fn create_key(path: &str) -> Result<HKEY> {
    let mut key = HKEY::default();
    let path = HSTRING::from(path);
    unsafe {
        RegCreateKeyExW(HKEY_LOCAL_MACHINE, &path, None, PCWSTR::null(), REG_OPTION_NON_VOLATILE, KEY_WRITE, None, &mut key, None)
            .ok()?;
    }
    Ok(key)
}

fn set_string(key: HKEY, name: &str, value: &str) -> Result<()> {
    let name = HSTRING::from(name);
    let data: Vec<u8> = value.encode_utf16().chain([0]).flat_map(u16::to_le_bytes).collect();
    let status: WIN32_ERROR = unsafe { RegSetValueExW(key, &name, None, REG_SZ, Some(&data)) };
    status.ok()
}
