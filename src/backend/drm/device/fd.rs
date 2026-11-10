use drm::{Device as BasicDevice, control::Device as ControlDevice};
use std::{
    io,
    os::unix::io::{AsFd, AsRawFd, BorrowedFd, RawFd},
    sync::{Arc, Weak},
};
use tracing::{error, info, warn};

use crate::backend::drm::{DrmNode, NodeType};
use crate::utils::{DevPath, DeviceFd};

#[derive(Debug)]
struct InternalDrmDeviceFd {
    fd: DeviceFd,
    privileged: bool,
}

impl PartialEq for InternalDrmDeviceFd {
    fn eq(&self, other: &Self) -> bool {
        self.fd == other.fd
    }
}

impl Drop for InternalDrmDeviceFd {
    fn drop(&mut self) {
        info!("Dropping device: {:?}", self.fd.dev_path());
        if self.privileged {
            if let Err(err) = self.release_master_lock() {
                error!("Failed to drop drm master state. Error: {}", err);
            }
        }
    }
}

impl AsFd for InternalDrmDeviceFd {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}
impl BasicDevice for InternalDrmDeviceFd {}
impl ControlDevice for InternalDrmDeviceFd {}

/// Ref-counted file descriptor of an open drm device
#[derive(Debug, Clone, PartialEq)]
pub struct DrmDeviceFd(Arc<InternalDrmDeviceFd>);

impl AsFd for DrmDeviceFd {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.fd.as_fd()
    }
}

// TODO: drop impl once not needed anymore by smithay or dependencies
impl AsRawFd for DrmDeviceFd {
    fn as_raw_fd(&self) -> RawFd {
        self.0.fd.as_raw_fd()
    }
}

impl DrmDeviceFd {
    /// Acquire DRM master, or adopt a descriptor already granted by a session broker.
    /// A brokered descriptor retains broker-managed master transitions. A directly
    /// acquired master is released on final drop. Clone the returned handle instead
    /// of creating multiple owners for the same open file description.
    pub fn new_master(fd: DeviceFd) -> io::Result<Self> {
        let mut device = InternalDrmDeviceFd {
            fd,
            privileged: false,
        };
        match device.acquire_master_lock() {
            Ok(()) => device.privileged = true,
            Err(error) => {
                // SET_MASTER checks the opener's identity even when this descriptor
                // already is master. logind opens and grants it from another process.
                // AUTH_MAGIC checks current master authority before rejecting token 0.
                let brokered_master = error.kind() == io::ErrorKind::PermissionDenied
                    && drm_ffi::auth::auth_magic_token(device.as_fd(), 0)
                        .is_err_and(|error| error.raw_os_error() == Some(libc::EINVAL));
                if !brokered_master {
                    return Err(error);
                }
            }
        }
        Ok(Self(Arc::new(device)))
    }

    /// Create a new `DrmDeviceFd`.
    ///
    /// This function will try to acquire the master lock for the underlying drm device
    /// and release the lock on drop again.
    /// For that reason you should never create multiple `DrmDeviceFd` out of the same
    /// `DeviceFd`, but instead clone the `DrmDeviceFd`.
    ///
    /// Failing to do so might fail to acquire set lock and release it early,
    /// which can cause some drm ioctls to fail later.
    pub fn new(fd: DeviceFd) -> DrmDeviceFd {
        let mut dev = InternalDrmDeviceFd {
            fd,
            privileged: false,
        };

        // Render nodes have no master; the kernel refuses SET_MASTER on them.
        let is_render_node = DrmNode::from_file(&dev.fd).is_ok_and(|node| node.ty() == NodeType::Render);

        // We want to modeset, so we better be the master, if we run via a tty session.
        // This is only needed on older kernels. Newer kernels grant this permission,
        // if no other process is already the *master*. So we skip over this error.
        if !is_render_node {
            if dev.acquire_master_lock().is_err() {
                warn!("Unable to become drm master, assuming unprivileged mode");
            } else {
                dev.privileged = true;
            }
        }

        DrmDeviceFd(Arc::new(dev))
    }

    pub(in crate::backend::drm) fn is_privileged(&self) -> bool {
        self.0.privileged
    }

    /// Returns the underlying `DeviceFd`
    pub fn device_fd(&self) -> DeviceFd {
        self.0.fd.clone()
    }

    /// Returns the `dev_t` of the underlying device
    pub fn dev_id(&self) -> rustix::io::Result<libc::dev_t> {
        Ok(rustix::fs::fstat(&self.0.fd)?.st_rdev)
    }

    /// Returns a weak reference to the underlying device
    pub fn downgrade(&self) -> WeakDrmDeviceFd {
        WeakDrmDeviceFd(Arc::downgrade(&self.0))
    }
}

impl BasicDevice for DrmDeviceFd {}
impl ControlDevice for DrmDeviceFd {}

/// Weak variant of [`DrmDeviceFd`]
#[derive(Debug, Clone, Default)]
pub struct WeakDrmDeviceFd(Weak<InternalDrmDeviceFd>);

impl WeakDrmDeviceFd {
    /// Construct an empty Weak reference, that will never upgrade successfully
    pub fn new() -> Self {
        WeakDrmDeviceFd(Weak::new())
    }

    /// Try to upgrade to a strong reference
    pub fn upgrade(&self) -> Option<DrmDeviceFd> {
        self.0.upgrade().map(DrmDeviceFd)
    }
}

impl PartialEq for WeakDrmDeviceFd {
    fn eq(&self, other: &Self) -> bool {
        Weak::ptr_eq(&self.0, &other.0)
    }
}

impl PartialEq<DrmDeviceFd> for WeakDrmDeviceFd {
    fn eq(&self, other: &DrmDeviceFd) -> bool {
        Weak::upgrade(&self.0).is_some_and(|arc| Arc::ptr_eq(&arc, &other.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs::OpenOptions,
        os::unix::process::CommandExt,
        process::{Command, Stdio},
    };

    #[test]
    #[ignore = "requires an isolated SMITHAY_DRM_MASTER_TEST_DEVICE and a root broker; changes KMS state"]
    fn brokered_master_accepts_unprivileged_clients_and_rejects_nonmasters() {
        const CHILD: &str = "SMITHAY_DRM_MASTER_TEST_CHILD";
        if let Ok(expected) = std::env::var(CHILD) {
            assert_ne!(rustix::process::geteuid().as_raw(), 0);
            let fd = std::io::stdin().as_fd().try_clone_to_owned().unwrap();
            assert_eq!(
                drm_ffi::auth::acquire_master(fd.as_fd()).unwrap_err().kind(),
                io::ErrorKind::PermissionDenied,
            );
            let result = DrmDeviceFd::new_master(DeviceFd::from(fd));
            if expected == "master" {
                let adopted = result.expect("a broker-granted master must work without root");
                assert!(!adopted.0.privileged, "the broker owns master transitions");
                let (mut device, _events) =
                    crate::backend::drm::DrmDevice::new_atomic(adopted.clone(), true).unwrap();
                device.pause();
                device.activate(false).unwrap();
                drop(device);
                drop(adopted);
            } else {
                assert_eq!(result.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
            }
            return;
        }
        assert_eq!(
            rustix::process::geteuid().as_raw(),
            0,
            "the test broker requires root"
        );
        let path =
            std::env::var_os("SMITHAY_DRM_MASTER_TEST_DEVICE").expect("use only an isolated KMS device");
        let master = OpenOptions::new().read(true).write(true).open(&path).unwrap();
        drm_ffi::auth::acquire_master(master.as_fd()).unwrap();
        let nonmaster = OpenOptions::new().read(true).write(true).open(&path).unwrap();
        for (file, expected) in [(&master, "master"), (&nonmaster, "nonmaster")] {
            let output = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "backend::drm::device::fd::tests::brokered_master_accepts_unprivileged_clients_and_rejects_nonmasters", "--ignored", "--nocapture"])
                .env(CHILD, expected)
                .uid(1000)
                .gid(1000)
                .stdin(Stdio::from(file.try_clone().unwrap()))
                .output().unwrap();
            assert!(
                output.status.success(),
                "{expected}: {}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(
                drm_ffi::auth::auth_magic_token(master.as_fd(), 0)
                    .unwrap_err()
                    .raw_os_error(),
                Some(libc::EINVAL),
                "dropping an adopted descriptor must retain the broker's master"
            );
        }
    }
}
