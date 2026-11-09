use std::{io, os::fd::BorrowedFd};

/// Check a Linux sync_file's execution result. Returns WouldBlock while the
/// fence is pending, the reported errno on failed work, or Ok on success.
/// The caller must establish completion before retiring sampled resources.
pub fn sync_file_result(fd: BorrowedFd<'_>) -> io::Result<()> {
    use rustix::ioctl::{Updater, ioctl, opcode::read_write};

    // Linux UAPI sync_file_info. No per-fence array is requested.
    #[repr(C)]
    #[derive(Default)]
    struct FileInfo {
        name: [u8; 32],
        status: i32,
        flags: u32,
        num_fences: u32,
        pad: u32,
        sync_fence_info: u64,
    }
    const FILE_INFO: rustix::ioctl::Opcode = read_write::<FileInfo>(b'>', 4);
    let mut info = FileInfo::default();
    // SAFETY: FileInfo matches SYNC_IOC_FILE_INFO, is initialized, and has no
    // output array pointer. The kernel validates whether fd is a sync_file.
    unsafe { ioctl(fd, Updater::<FILE_INFO, _>::new(&mut info))? };
    match info.status {
        1 => Ok(()),
        0 => Err(io::ErrorKind::WouldBlock.into()),
        error if error < 0 => Err(io::Error::from_raw_os_error(-error)),
        _ => Err(io::Error::other("sync_file returned an invalid status")),
    }
}
