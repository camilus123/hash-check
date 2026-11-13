//! Which physical drive a path is on, and what kind of drive that is.
//!
//! Hashing is scheduled per physical drive: many files in parallel on an
//! SSD, one file at a time on a hard disk (parallel reads make a disk head
//! seek back and forth and are slower than reading in order).
//!
//! On Windows the drive is queried through a volume handle opened with
//! *no access rights at all* (neither read nor write): that is enough for
//! the three read-only queries used here and cannot alter anything.
//! Elsewhere the kind is unknown and the conservative default applies;
//! the per-drive thread setting overrides either way.

use crate::verify::volume_root;
use std::collections::HashMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DriveKind {
    /// No seek penalty.
    Ssd,
    /// Rotating disk: seeks are expensive.
    Hdd,
    Network,
    Optical,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bus {
    Nvme,
    Sata,
    Usb,
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DriveInfo {
    /// Volume the paths are on, e.g. `L:\`.
    pub volume: String,
    pub kind: DriveKind,
    pub bus: Bus,
    pub removable: bool,
    /// Physical disk number, when the volume sits on exactly one disk.
    /// Two volumes with the same number share one device.
    pub disk: Option<u32>,
    /// Model name reported by the device, if any.
    pub model: Option<String>,
}

impl DriveInfo {
    pub fn unknown(volume: &str) -> DriveInfo {
        DriveInfo {
            volume: volume.to_string(),
            kind: DriveKind::Unknown,
            bus: Bus::Other,
            removable: false,
            disk: None,
            model: None,
        }
    }

    /// Short description for the UI, e.g. "NVMe SSD" or "HDD (USB)".
    pub fn kind_label(&self) -> String {
        let base = match (self.kind, self.bus) {
            (DriveKind::Ssd, Bus::Nvme) => "NVMe SSD",
            (DriveKind::Ssd, Bus::Sata) => "SATA SSD",
            (DriveKind::Ssd, _) => "SSD",
            (DriveKind::Hdd, _) => "HDD",
            (DriveKind::Network, _) => "Network",
            (DriveKind::Optical, _) => "Optical",
            (DriveKind::Unknown, _) => "Unknown type",
        };
        if self.bus == Bus::Usb {
            format!("{base} (USB)")
        } else if self.removable {
            format!("{base} (removable)")
        } else {
            base.to_string()
        }
    }

    /// Threads used when the user has not set a number for this drive.
    pub fn default_threads(&self) -> usize {
        let cpus = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);
        match (self.kind, self.bus) {
            // Fast enough to keep many cores busy; half the logical CPUs
            // leaves room for BLAKE3's own threads on large files.
            (DriveKind::Ssd, Bus::Nvme) => (cpus / 2).clamp(2, 16),
            (DriveKind::Ssd, Bus::Usb) => 2,
            (DriveKind::Ssd, _) => 4,
            (DriveKind::Network, _) => 2,
            // Sequential: one file at a time.
            (DriveKind::Hdd, _) | (DriveKind::Optical, _) | (DriveKind::Unknown, _) => 1,
        }
    }

    /// Key identifying the physical device, for grouping work.
    pub fn device_key(&self) -> String {
        match self.disk {
            Some(n) => format!("disk{n}"),
            None => self.volume.clone(),
        }
    }
}

pub const MAX_THREADS: usize = 64;

/// How a drive will be read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DrivePlan {
    pub info: DriveInfo,
    pub threads: usize,
    /// Whether `threads` came from the user's per-drive setting.
    pub overridden: bool,
}

impl DrivePlan {
    /// One file at a time, hashing spread over the cores where the
    /// algorithm allows.
    pub fn sequential(&self) -> bool {
        self.threads <= 1
    }

    pub fn describe(&self) -> String {
        let mode = if self.sequential() {
            "1 file at a time".to_string()
        } else {
            format!("{} files in parallel", self.threads)
        };
        format!("{} {}: {mode}", self.info.volume, self.info.kind_label())
    }
}

/// Looks up drives, asking the operating system once per volume.
#[derive(Default)]
pub struct Detector {
    cache: HashMap<String, DriveInfo>,
}

impl Detector {
    pub fn new() -> Detector {
        Detector::default()
    }

    /// The drive holding `path`. Paths without a recognisable volume are
    /// grouped under one unknown drive.
    pub fn drive_of(&mut self, path: &str) -> &DriveInfo {
        let volume = volume_root(path).unwrap_or("");
        if !self.cache.contains_key(volume) {
            self.cache.insert(volume.to_string(), detect(volume));
        }
        &self.cache[volume]
    }
}

/// Query the operating system about a volume such as `L:\`.
pub fn detect(volume: &str) -> DriveInfo {
    imp::detect(volume)
}

#[cfg(not(windows))]
mod imp {
    use super::DriveInfo;

    pub fn detect(volume: &str) -> DriveInfo {
        DriveInfo::unknown(volume)
    }
}

#[cfg(windows)]
mod imp {
    use super::{Bus, DriveInfo, DriveKind};
    use std::ffi::c_void;

    type Handle = *mut c_void;
    const INVALID_HANDLE_VALUE: Handle = -1isize as Handle;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CreateFileW(
            name: *const u16,
            desired_access: u32,
            share_mode: u32,
            security: *const c_void,
            creation: u32,
            flags: u32,
            template: Handle,
        ) -> Handle;
        fn DeviceIoControl(
            device: Handle,
            code: u32,
            in_buf: *const c_void,
            in_len: u32,
            out_buf: *mut c_void,
            out_len: u32,
            returned: *mut u32,
            overlapped: *mut c_void,
        ) -> i32;
        fn CloseHandle(h: Handle) -> i32;
        fn GetDriveTypeW(root: *const u16) -> u32;
    }

    /// Query-only: no read access, no write access.
    const NO_ACCESS: u32 = 0;
    const SHARE_ALL: u32 = 0x1 | 0x2 | 0x4;
    const OPEN_EXISTING: u32 = 3;

    const DRIVE_REMOVABLE: u32 = 2;
    const DRIVE_REMOTE: u32 = 4;
    const DRIVE_CDROM: u32 = 5;
    const DRIVE_RAMDISK: u32 = 6;

    const IOCTL_STORAGE_QUERY_PROPERTY: u32 = 0x002D_1400;
    const IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS: u32 = 0x0056_0000;
    const STORAGE_DEVICE_PROPERTY: u32 = 0;
    const STORAGE_DEVICE_SEEK_PENALTY_PROPERTY: u32 = 7;
    const PROPERTY_STANDARD_QUERY: u32 = 0;

    const BUS_TYPE_USB: u32 = 7;
    const BUS_TYPE_SATA: u32 = 11;
    const BUS_TYPE_NVME: u32 = 17;

    #[repr(C)]
    struct StoragePropertyQuery {
        property_id: u32,
        query_type: u32,
        additional: [u8; 4],
    }

    struct Volume(Handle);

    impl Drop for Volume {
        fn drop(&mut self) {
            // SAFETY: the handle came from CreateFileW and is closed once.
            unsafe { CloseHandle(self.0) };
        }
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// Run a storage property query into `out`; returns the bytes written.
    fn query(vol: &Volume, property_id: u32, out: &mut [u64]) -> Option<usize> {
        let q = StoragePropertyQuery {
            property_id,
            query_type: PROPERTY_STANDARD_QUERY,
            additional: [0; 4],
        };
        let mut returned = 0u32;
        // SAFETY: both buffers are valid for the lengths passed, and the
        // output buffer is 8-byte aligned.
        let ok = unsafe {
            DeviceIoControl(
                vol.0,
                IOCTL_STORAGE_QUERY_PROPERTY,
                (&q as *const StoragePropertyQuery).cast(),
                std::mem::size_of::<StoragePropertyQuery>() as u32,
                out.as_mut_ptr().cast(),
                std::mem::size_of_val(out) as u32,
                &mut returned,
                std::ptr::null_mut(),
            )
        };
        (ok != 0).then_some(returned as usize)
    }

    fn bytes_of(words: &[u64]) -> Vec<u8> {
        words.iter().flat_map(|w| w.to_ne_bytes()).collect()
    }

    fn u32_at(bytes: &[u8], offset: usize) -> Option<u32> {
        Some(u32::from_ne_bytes(
            bytes.get(offset..offset + 4)?.try_into().ok()?,
        ))
    }

    pub fn detect(volume: &str) -> DriveInfo {
        let mut info = DriveInfo::unknown(volume);
        let b = volume.as_bytes();
        let is_letter = b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':';
        if volume.starts_with(r"\\") {
            info.kind = DriveKind::Network;
            return info;
        }
        if !is_letter {
            return info;
        }
        let letter = &volume[..2];

        // SAFETY: the argument is a NUL-terminated UTF-16 string.
        let drive_type = unsafe { GetDriveTypeW(wide(&format!("{letter}\\")).as_ptr()) };
        match drive_type {
            DRIVE_REMOTE => {
                info.kind = DriveKind::Network;
                return info;
            }
            DRIVE_CDROM => {
                info.kind = DriveKind::Optical;
                return info;
            }
            DRIVE_RAMDISK => {
                info.kind = DriveKind::Ssd;
                return info;
            }
            DRIVE_REMOVABLE => info.removable = true,
            _ => {}
        }

        let name = wide(&format!(r"\\.\{letter}"));
        // SAFETY: valid NUL-terminated name; null security attributes and
        // template are allowed. Access 0 = query only.
        let handle = unsafe {
            CreateFileW(
                name.as_ptr(),
                NO_ACCESS,
                SHARE_ALL,
                std::ptr::null(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE || handle.is_null() {
            return info;
        }
        let vol = Volume(handle);

        // Seek penalty: the direct "is this a spinning disk" answer.
        // DEVICE_SEEK_PENALTY_DESCRIPTOR { Version: u32, Size: u32, IncursSeekPenalty: u8 }
        let mut out = [0u64; 2];
        let seek_penalty = query(&vol, STORAGE_DEVICE_SEEK_PENALTY_PROPERTY, &mut out)
            .filter(|n| *n >= 9)
            .map(|_| bytes_of(&out)[8] != 0);

        // Bus type, removable flag and model name.
        // STORAGE_DEVICE_DESCRIPTOR: RemovableMedia at 10, ProductIdOffset at 16, BusType at 28.
        let mut out = [0u64; 128];
        if let Some(n) = query(&vol, STORAGE_DEVICE_PROPERTY, &mut out) {
            let bytes = bytes_of(&out);
            let bytes = &bytes[..n.min(bytes.len())];
            if bytes.len() >= 32 {
                info.removable |= bytes[10] != 0;
                info.bus = match u32_at(bytes, 28) {
                    Some(BUS_TYPE_NVME) => Bus::Nvme,
                    Some(BUS_TYPE_SATA) => Bus::Sata,
                    Some(BUS_TYPE_USB) => Bus::Usb,
                    _ => Bus::Other,
                };
                if let Some(off) = u32_at(bytes, 16).map(|o| o as usize)
                    && off > 0
                    && off < bytes.len()
                {
                    let end = bytes[off..]
                        .iter()
                        .position(|c| *c == 0)
                        .map_or(bytes.len(), |p| off + p);
                    let model = String::from_utf8_lossy(&bytes[off..end]).trim().to_string();
                    if !model.is_empty() {
                        info.model = Some(model);
                    }
                }
            }
        }

        // Physical disk number (only when the volume is on a single disk).
        // VOLUME_DISK_EXTENTS { NumberOfDiskExtents: u32, pad, Extents[0].DiskNumber: u32 at 8, ... }
        let mut out = [0u64; 32];
        let mut returned = 0u32;
        // SAFETY: the output buffer is valid for the length passed and 8-byte aligned.
        let ok = unsafe {
            DeviceIoControl(
                vol.0,
                IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS,
                std::ptr::null(),
                0,
                out.as_mut_ptr().cast(),
                std::mem::size_of_val(&out) as u32,
                &mut returned,
                std::ptr::null_mut(),
            )
        };
        if ok != 0 && returned >= 12 {
            let bytes = bytes_of(&out);
            if u32_at(&bytes, 0) == Some(1) {
                info.disk = u32_at(&bytes, 8);
            }
        }

        info.kind = match seek_penalty {
            Some(true) => DriveKind::Hdd,
            Some(false) => DriveKind::Ssd,
            None if info.bus == Bus::Nvme => DriveKind::Ssd,
            None => DriveKind::Unknown,
        };
        info
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drive(kind: DriveKind, bus: Bus) -> DriveInfo {
        DriveInfo {
            kind,
            bus,
            ..DriveInfo::unknown(r"X:\")
        }
    }

    #[test]
    fn hard_disks_and_unknown_drives_default_to_sequential() {
        for d in [
            drive(DriveKind::Hdd, Bus::Sata),
            drive(DriveKind::Hdd, Bus::Usb),
            drive(DriveKind::Unknown, Bus::Other),
            drive(DriveKind::Optical, Bus::Other),
        ] {
            assert_eq!(d.default_threads(), 1, "{d:?}");
            let plan = DrivePlan {
                threads: d.default_threads(),
                info: d,
                overridden: false,
            };
            assert!(plan.sequential());
        }
    }

    #[test]
    fn ssds_default_to_parallel() {
        let nvme = drive(DriveKind::Ssd, Bus::Nvme).default_threads();
        let sata = drive(DriveKind::Ssd, Bus::Sata).default_threads();
        assert!((2..=16).contains(&nvme));
        assert_eq!(sata, 4);
        assert!(drive(DriveKind::Network, Bus::Other).default_threads() >= 2);
    }

    #[test]
    fn volumes_on_one_disk_share_a_device_key() {
        let mut c = drive(DriveKind::Hdd, Bus::Sata);
        let mut d = DriveInfo {
            volume: r"Y:\".into(),
            ..c.clone()
        };
        assert_ne!(c.device_key(), d.device_key());
        c.disk = Some(3);
        d.disk = Some(3);
        assert_eq!(c.device_key(), d.device_key());
        d.disk = Some(4);
        assert_ne!(c.device_key(), d.device_key());
    }

    #[test]
    fn labels() {
        assert_eq!(drive(DriveKind::Ssd, Bus::Nvme).kind_label(), "NVMe SSD");
        assert_eq!(drive(DriveKind::Hdd, Bus::Usb).kind_label(), "HDD (USB)");
        assert_eq!(
            drive(DriveKind::Unknown, Bus::Other).kind_label(),
            "Unknown type"
        );
    }

    #[test]
    fn detector_caches_per_volume_and_survives_odd_paths() {
        let mut d = Detector::new();
        for p in [
            "",
            "relative",
            "/tmp/x",
            r"C:\Windows\x",
            r"\\server\share\f",
            r"Q:\nothing",
        ] {
            let info = d.drive_of(p).clone();
            assert_eq!(&info, d.drive_of(p));
            assert!(info.default_threads() >= 1);
        }
    }
}
