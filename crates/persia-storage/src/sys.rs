//! Operating-system calls the standard library does not expose (ADR-0020): whether an open file lives on a local disk
//! file system (ROADMAP 2.3.10, for the automatic choice of mapped reads, ADR-0017), and on Windows a file's identity
//! (2.4.18, for "the path still names this file", SPEC §4.5.1). Besides `container::mapped`, the only module of this
//! crate with `unsafe` code; every call states why it is sound.
#![allow(unsafe_code)]

use std::fs::File;
use std::io;

/// Whether `file` is on a local disk file system, where a mapping's page faults read a local device: Linux ext2-4,
/// XFS, Btrfs, F2FS, ZFS, bcachefs, JFS, `ReiserFS`, NILFS, overlayfs, tmpfs and ramfs; macOS volumes flagged
/// `MNT_LOCAL` that are not FUSE; Windows fixed and RAM disks. Everything else is not: network and FUSE file
/// systems, removable media (Windows; Linux cannot tell, so FAT and exFAT, mostly removable, are excluded), and every
/// file system of an operating system not listed here.
pub(crate) fn is_local_disk(file: &File) -> io::Result<bool> {
    imp::is_local_disk(file)
}

/// The identity of `file` on Windows: its volume serial number and file id. Two handles name the same file exactly
/// when their identities are equal.
#[cfg(windows)]
pub(crate) fn file_id(file: &File) -> io::Result<(u64, [u8; 16])> {
    imp::file_id(file)
}

#[cfg(any(target_os = "linux", target_os = "android"))]
mod imp {
    use std::fs::File;
    use std::io;
    use std::mem::MaybeUninit;
    use std::os::fd::AsRawFd;

    /// `f_type` magic numbers of local disk file systems (`linux/magic.h`).
    const LOCAL: [u32; 12] = [
        0xEF53,      // ext2, ext3, ext4
        0x5846_5342, // XFS
        0x9123_683E, // Btrfs
        0xF2F5_2010, // F2FS
        0x2FC1_2FC1, // ZFS
        0xCA45_1A4E, // bcachefs
        0x3153_464A, // JFS
        0x5265_4973, // ReiserFS
        0x3434,      // NILFS
        0x794C_7630, // overlayfs (container images; its layers are local)
        0x0102_1994, // tmpfs
        0x8584_58F6, // ramfs
    ];

    pub(super) fn is_local_disk(file: &File) -> io::Result<bool> {
        let mut st = MaybeUninit::<libc::statfs>::uninit();
        // SAFETY: `fstatfs` writes one `statfs` through the pointer, which is valid for writes of that size and
        // alignment, and only reads the descriptor, which `file` keeps open for the duration of the call.
        let rc = unsafe { libc::fstatfs(file.as_raw_fd(), st.as_mut_ptr()) };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `fstatfs` returned 0, so it filled the whole struct.
        let st = unsafe { st.assume_init() };
        // `f_type`'s integer type depends on the architecture; the magic numbers are its low 32 bits.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let magic = st.f_type as u32;
        Ok(LOCAL.contains(&magic))
    }

    #[cfg(test)]
    mod tests {
        #[test]
        fn the_table_never_counts_a_network_or_fuse_type() {
            // NFS, SMB, CIFS, SMB2, FUSE, 9p, Ceph, and FAT and exFAT (mostly removable).
            for other in [
                0x6969,
                0x517B,
                0xFF53_4D42,
                0xFE53_4D42,
                0x6573_5546,
                0x0102_1997,
                0x00C3_6400,
                0x4D44,
                0x2011_BAB0,
            ] {
                assert!(!super::LOCAL.contains(&other), "{other:#x}");
            }
        }
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use std::fs::File;
    use std::io;
    use std::mem::MaybeUninit;
    use std::os::fd::AsRawFd;

    pub(super) fn is_local_disk(file: &File) -> io::Result<bool> {
        let mut st = MaybeUninit::<libc::statfs>::uninit();
        // SAFETY: as on Linux: one `statfs` written through a valid pointer, the descriptor open for the call.
        let rc = unsafe { libc::fstatfs(file.as_raw_fd(), st.as_mut_ptr()) };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `fstatfs` returned 0, so it filled the whole struct.
        let st = unsafe { st.assume_init() };
        let local = u32::try_from(libc::MNT_LOCAL).is_ok_and(|flag| st.f_flags & flag != 0);
        // macFUSE volumes may claim to be local; their pages come from a user-space process.
        let name: Vec<u8> = st
            .f_fstypename
            .iter()
            .take_while(|&&c| c != 0)
            .map(|c| c.to_ne_bytes()[0])
            .collect();
        let fuse = [&b"macfuse"[..], b"osxfuse", b"fuse"]
            .iter()
            .any(|prefix| name.starts_with(prefix));
        Ok(local && !fuse)
    }
}

#[cfg(windows)]
mod imp {
    use std::fs::File;
    use std::io;
    use std::os::windows::io::AsRawHandle;

    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, FILE_ID_INFO, FILE_NAME_NORMALIZED, FileIdInfo, GetDriveTypeW,
        GetFileInformationByHandle, GetFileInformationByHandleEx, GetFinalPathNameByHandleW,
        GetVolumePathNameW, VOLUME_NAME_DOS,
    };
    use windows_sys::Win32::System::WindowsProgramming::{DRIVE_FIXED, DRIVE_RAMDISK};

    fn handle(file: &File) -> HANDLE {
        file.as_raw_handle()
    }

    fn len_u32(len: usize) -> io::Result<u32> {
        u32::try_from(len).map_err(|_| io::Error::other("buffer too large for a Windows call"))
    }

    /// The file's final path, NUL-terminated, without a `\\?\` prefix before a drive letter.
    fn final_path(file: &File) -> io::Result<Vec<u16>> {
        let mut buf = vec![0_u16; 512];
        loop {
            let cap = len_u32(buf.len())?;
            // SAFETY: the handle stays open for the call (`file` is borrowed), and `buf` is valid for writes of `cap`
            // UTF-16 units; the call writes at most that many, NUL included.
            let n = unsafe {
                GetFinalPathNameByHandleW(
                    handle(file),
                    buf.as_mut_ptr(),
                    cap,
                    FILE_NAME_NORMALIZED | VOLUME_NAME_DOS,
                )
            };
            let n = usize::try_from(n).map_err(|_| io::Error::other("path length"))?;
            if n == 0 {
                return Err(io::Error::last_os_error());
            }
            if n < buf.len() {
                // Success: `n` units, without the NUL that follows them.
                buf.truncate(n);
                let prefix: Vec<u16> = r"\\?\".encode_utf16().collect();
                let drive = buf.get(prefix.len().saturating_add(1)) == Some(&u16::from(b':'));
                if buf.starts_with(&prefix) && drive {
                    buf.drain(..prefix.len());
                }
                buf.push(0);
                return Ok(buf);
            }
            // Too small: `n` is the length needed, NUL included.
            buf.resize(n, 0);
        }
    }

    pub(super) fn is_local_disk(file: &File) -> io::Result<bool> {
        let path = final_path(file)?;
        // The volume path is a prefix of the path, plus at most a trailing backslash and the NUL.
        let mut root = vec![0_u16; path.len().saturating_add(2)];
        let cap = len_u32(root.len())?;
        // SAFETY: `path` is NUL-terminated (`final_path`), and `root` is valid for writes of `cap` units.
        let ok = unsafe { GetVolumePathNameW(path.as_ptr(), root.as_mut_ptr(), cap) };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `GetVolumePathNameW` succeeded, so `root` holds a NUL-terminated path within its length.
        let kind = unsafe { GetDriveTypeW(root.as_ptr()) };
        Ok(matches!(kind, DRIVE_FIXED | DRIVE_RAMDISK))
    }

    pub(super) fn file_id(file: &File) -> io::Result<(u64, [u8; 16])> {
        let mut info = FILE_ID_INFO::default();
        let size = len_u32(size_of::<FILE_ID_INFO>())?;
        // SAFETY: the handle stays open for the call, and `info` is a `FILE_ID_INFO`, the type `FileIdInfo` writes,
        // of exactly `size` bytes.
        let ok = unsafe {
            GetFileInformationByHandleEx(handle(file), FileIdInfo, (&raw mut info).cast(), size)
        };
        if ok != 0 {
            return Ok((info.VolumeSerialNumber, info.FileId.Identifier));
        }
        // File systems without 128-bit ids (FAT, exFAT): the 64-bit file index, unique on them.
        let mut info = BY_HANDLE_FILE_INFORMATION::default();
        // SAFETY: the handle stays open for the call, and `info` is valid for writes of the struct the call fills.
        let ok = unsafe { GetFileInformationByHandle(handle(file), &raw mut info) };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        let index = (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow);
        let mut id = [0; 16];
        if let Some(low) = id.get_mut(..8) {
            low.copy_from_slice(&index.to_le_bytes());
        }
        Ok((u64::from(info.dwVolumeSerialNumber), id))
    }
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    windows
)))]
mod imp {
    use std::fs::File;
    use std::io;

    /// Not detected here: never local, so mapped reads stay opt-in.
    pub(super) fn is_local_disk(_file: &File) -> io::Result<bool> {
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::is_local_disk;

    /// The temporary directory is on a local disk on every CI platform (ext4 on Linux, APFS on macOS, NTFS on Windows).
    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    #[test]
    fn a_temporary_file_is_on_a_local_disk() {
        let file = tempfile::tempfile().unwrap();
        assert!(is_local_disk(&file).unwrap());
    }

    /// procfs is not a disk; tmpfs (`/dev/shm`) counts as local memory.
    #[cfg(target_os = "linux")]
    #[test]
    fn procfs_is_not_a_local_disk_and_tmpfs_is() {
        let proc = std::fs::File::open("/proc/self/status").unwrap();
        assert!(!is_local_disk(&proc).unwrap());
        if let Ok(shm) = tempfile::tempfile_in("/dev/shm") {
            assert!(is_local_disk(&shm).unwrap());
        }
    }
}
