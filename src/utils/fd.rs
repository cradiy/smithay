use std::{
    fmt,
    os::unix::io::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd},
    path::PathBuf,
    sync::Arc,
};

/// Ref-counted file descriptor of an open device node
#[derive(Clone)]
pub struct DeviceFd {
    fd: Arc<OwnedFd>,
    owner: Option<Arc<dyn Send + Sync>>,
}

impl fmt::Debug for DeviceFd {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeviceFd")
            .field("fd", &self.fd)
            .field("managed", &self.owner.is_some())
            .finish()
    }
}

impl DeviceFd {
    /// Keeps an owner alive until the last descriptor reference is dropped.
    /// The descriptor is closed before the owner is dropped. Managed descriptors
    /// cannot be extracted through `TryInto<OwnedFd>`, which would discard ownership.
    pub fn with_owner<T: Send + Sync + 'static>(fd: OwnedFd, owner: T) -> Self {
        Self {
            fd: Arc::new(fd),
            owner: Some(Arc::new(owner)),
        }
    }
}

impl PartialEq for DeviceFd {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        self.fd.as_raw_fd() == other.fd.as_raw_fd()
    }
}

impl AsFd for DeviceFd {
    #[inline]
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

// TODO: drop impl once not needed anymore by smithay or dependencies
impl AsRawFd for DeviceFd {
    #[inline]
    fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
}

impl FromRawFd for DeviceFd {
    /// SAFETY:
    /// Make sure that `fd` is a valid value!
    #[inline]
    unsafe fn from_raw_fd(fd: RawFd) -> Self {
        Self::from(unsafe { OwnedFd::from_raw_fd(fd) })
    }
}

impl From<OwnedFd> for DeviceFd {
    #[inline]
    fn from(fd: OwnedFd) -> Self {
        Self {
            fd: Arc::new(fd),
            owner: None,
        }
    }
}

impl TryInto<OwnedFd> for DeviceFd {
    type Error = DeviceFd;

    #[inline]
    fn try_into(self) -> Result<OwnedFd, Self::Error> {
        if self.owner.is_some() {
            return Err(self);
        }
        Arc::try_unwrap(self.fd).map_err(|fd| Self { fd, owner: None })
    }
}

/// Trait representing open devices that *may* return a `Path`
pub trait DevPath {
    /// Returns the path of the open device if possible
    fn dev_path(&self) -> Option<PathBuf>;
}

impl<A: AsFd> DevPath for A {
    fn dev_path(&self) -> Option<PathBuf> {
        use std::fs;

        fs::read_link(format!("/proc/self/fd/{:?}", self.as_fd().as_raw_fd())).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{io::Read, os::unix::net::UnixStream, sync::mpsc, thread, time::Duration};

    struct Release {
        peer: UnixStream,
        done: mpsc::Sender<()>,
    }

    impl Drop for Release {
        fn drop(&mut self) {
            assert_eq!(
                self.peer.read(&mut [0]).unwrap(),
                0,
                "descriptor must close before its owner"
            );
            self.done.send(()).unwrap();
        }
    }

    #[test]
    fn managed_descriptor_releases_its_owner_after_the_last_shared_reference() {
        let (device, peer) = UnixStream::pair().unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let (done, observed) = mpsc::channel();
        let fd = DeviceFd::with_owner(device.into(), Release { peer, done });
        let duplicate = fd.clone();
        let extracted: Result<OwnedFd, _> = fd.try_into();
        drop(extracted.unwrap_err());
        assert!(matches!(observed.try_recv(), Err(mpsc::TryRecvError::Empty)));
        let extracted: Result<OwnedFd, _> = duplicate.try_into();
        thread::spawn(move || drop(extracted.unwrap_err()))
            .join()
            .unwrap();
        observed.recv_timeout(Duration::from_secs(2)).unwrap();
    }

    #[test]
    fn plain_descriptor_can_be_extracted_only_after_other_references_retire() {
        let (device, mut peer) = UnixStream::pair().unwrap();
        let fd = DeviceFd::from(OwnedFd::from(device));
        let original = fd.as_raw_fd();
        let duplicate = fd.clone();
        let extracted: Result<OwnedFd, _> = fd.try_into();
        let fd = extracted.unwrap_err();
        drop(duplicate);
        let extracted: OwnedFd = fd.try_into().unwrap();
        assert_eq!(extracted.as_raw_fd(), original);
        drop(extracted);
        assert_eq!(peer.read(&mut [0]).unwrap(), 0);
    }
}
