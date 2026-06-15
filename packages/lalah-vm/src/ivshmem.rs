//! Map the IVSHMEM BAR2 region into the process via the Looking Glass /
//! virtio-win `ivshmem.sys` driver (SetupAPI to find the device, then
//! `DeviceIoControl` to request the mmap).
//!
//! With QEMU `-device ivshmem-plain` there is NO register prefix: the pointer
//! returned by `IOCTL_IVSHMEM_REQUEST_MMAP` is BAR2 offset 0, which equals the
//! host memory-backend-file offset 0 — i.e. the [`shared::ShmHeader`].
//!
//! The device-interface GUID, the IOCTL codes, and the `IVSHMEM_MMAP` struct
//! layout below are the public IVSHMEM driver interface defined by the Looking
//! Glass project (GPL-2.0). They are reproduced here as interface facts solely
//! to interoperate with the user-installed `ivshmem.sys` driver; no Looking
//! Glass code is used.
//!
//! 64-bit only: the `IVSHMEM_MMAP` struct layout is x86_64-specific.

use std::ffi::c_void;
use windows::core::{Result, GUID, PCWSTR};
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW,
    SetupDiGetDeviceInterfaceDetailW, DIGCF_DEVICEINTERFACE, DIGCF_PRESENT,
    SP_DEVICE_INTERFACE_DATA, SP_DEVICE_INTERFACE_DETAIL_DATA_W,
};
use windows::Win32::Foundation::{CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::IO::DeviceIoControl;

/// `{df576976-569d-4672-95a0-f57e4ea0b210}` — the IVSHMEM device interface.
const GUID_DEVINTERFACE_IVSHMEM: GUID = GUID::from_u128(0xdf576976_569d_4672_95a0_f57e4ea0b210);

const FILE_DEVICE_UNKNOWN: u32 = 0x0000_0022;
const METHOD_BUFFERED: u32 = 0;
const FILE_ANY_ACCESS: u32 = 0;

const fn ctl_code(device: u32, function: u32, method: u32, access: u32) -> u32 {
    (device << 16) | (access << 14) | (function << 2) | method
}

const IOCTL_IVSHMEM_REQUEST_SIZE: u32 =
    ctl_code(FILE_DEVICE_UNKNOWN, 0x801, METHOD_BUFFERED, FILE_ANY_ACCESS);
const IOCTL_IVSHMEM_REQUEST_MMAP: u32 =
    ctl_code(FILE_DEVICE_UNKNOWN, 0x802, METHOD_BUFFERED, FILE_ANY_ACCESS);

/// Cache mode for the mapping. CACHED keeps the shared-crate atomics working with
/// normal hardware coherency (no manual SFENCE needed).
const IVSHMEM_CACHE_CACHED: u8 = 1;

/// Input for `IOCTL_IVSHMEM_REQUEST_MMAP` on current driver builds (1 byte).
#[repr(C)]
struct IvshmemMmapConfig {
    cache_mode: u8,
}

/// Output for `IOCTL_IVSHMEM_REQUEST_MMAP`. Natural x64 alignment, size 32:
/// peer_id@0, size@8, ptr@16, vectors@24. Do NOT pack.
#[repr(C)]
struct IvshmemMmap {
    peer_id: u16,
    size: u64,
    ptr: *mut c_void,
    vectors: u16,
}

/// A mapped IVSHMEM region. Closing the handle (or process exit) unmaps it, so
/// the handle is kept alive for the region's lifetime.
pub struct IvshmemRegion {
    handle: HANDLE,
    pub base: *mut u8,
    pub len: usize,
}

impl Drop for IvshmemRegion {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.handle);
        }
    }
}

/// Find the single IVSHMEM device, request the mmap, and return the region.
pub fn map_ivshmem() -> Result<IvshmemRegion> {
    unsafe {
        // Enumerate present devices exposing the IVSHMEM interface.
        let hdev = SetupDiGetClassDevsW(
            Some(&GUID_DEVINTERFACE_IVSHMEM),
            PCWSTR::null(),
            None,
            DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
        )?;

        let mut did = SP_DEVICE_INTERFACE_DATA {
            cbSize: std::mem::size_of::<SP_DEVICE_INTERFACE_DATA>() as u32,
            ..Default::default()
        };

        let enum_res = SetupDiEnumDeviceInterfaces(
            hdev,
            None,
            &GUID_DEVINTERFACE_IVSHMEM,
            0,
            &mut did,
        );
        if let Err(e) = enum_res {
            let _ = SetupDiDestroyDeviceInfoList(hdev);
            return Err(e);
        }

        // First detail call: probe the required buffer size (expected to fail
        // with ERROR_INSUFFICIENT_BUFFER).
        let mut required: u32 = 0;
        let _ = SetupDiGetDeviceInterfaceDetailW(hdev, &did, None, 0, Some(&mut required), None);

        // Allocate the variable-length detail buffer and set the FIXED header
        // size in cbSize (NOT the buffer length).
        let mut buf = vec![0u8; required as usize];
        let detail = buf.as_mut_ptr() as *mut SP_DEVICE_INTERFACE_DETAIL_DATA_W;
        (*detail).cbSize = std::mem::size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32;

        let detail_res = SetupDiGetDeviceInterfaceDetailW(
            hdev,
            &did,
            Some(detail),
            required,
            None,
            None,
        );
        if let Err(e) = detail_res {
            let _ = SetupDiDestroyDeviceInfoList(hdev);
            return Err(e);
        }

        let path = PCWSTR((*detail).DevicePath.as_ptr());
        let handle = CreateFileW(
            path,
            (GENERIC_READ | GENERIC_WRITE).0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            Default::default(),
            None,
        );
        let _ = SetupDiDestroyDeviceInfoList(hdev);
        let handle = handle?;

        // Query the region size.
        let mut size: u64 = 0;
        let mut returned: u32 = 0;
        DeviceIoControl(
            handle,
            IOCTL_IVSHMEM_REQUEST_SIZE,
            None,
            0,
            Some(&mut size as *mut u64 as *mut c_void),
            std::mem::size_of::<u64>() as u32,
            Some(&mut returned),
            None,
        )?;

        // Request the mmap.
        let cfg = IvshmemMmapConfig {
            cache_mode: IVSHMEM_CACHE_CACHED,
        };
        let mut map = IvshmemMmap {
            peer_id: 0,
            size: 0,
            ptr: std::ptr::null_mut(),
            vectors: 0,
        };
        DeviceIoControl(
            handle,
            IOCTL_IVSHMEM_REQUEST_MMAP,
            Some(&cfg as *const IvshmemMmapConfig as *const c_void),
            std::mem::size_of::<IvshmemMmapConfig>() as u32,
            Some(&mut map as *mut IvshmemMmap as *mut c_void),
            std::mem::size_of::<IvshmemMmap>() as u32,
            Some(&mut returned),
            None,
        )?;

        Ok(IvshmemRegion {
            handle,
            base: map.ptr as *mut u8,
            len: map.size as usize,
        })
    }
}
