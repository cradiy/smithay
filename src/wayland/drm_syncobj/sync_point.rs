use calloop::generic::Generic;
use calloop::{EventSource, Interest, Mode, Poll, PostAction, Readiness, Token, TokenFactory};
use drm::control::Device;
use std::{
    collections::HashMap,
    io,
    os::unix::io::{AsFd, BorrowedFd, OwnedFd},
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, Ordering},
    },
};

use crate::backend::drm::DrmDeviceFd;
use crate::backend::renderer::sync::{Fence, Interrupted};
use crate::wayland::compositor::{Blocker, BlockerState};

#[derive(Debug)]
pub(super) struct DrmTimelineInner {
    timeline_fd: OwnedFd,
    dev_ctx: Mutex<DrmTimelineDeviceSpecific>,
    deferred_releases: Weak<DeferredReleases>,
}

pub(super) type DeferredReleases = Mutex<HashMap<usize, DrmSyncPoint>>;

impl DrmTimelineInner {
    pub(super) fn update_device(&self, device: &DrmDeviceFd) -> io::Result<()> {
        let mut ctx = self.dev_ctx.lock().unwrap();
        let mut new = DrmTimelineDeviceSpecific::import(self.timeline_fd.as_fd(), device)?;
        for (point, eventfd) in ctx
            .event_fds
            .iter()
            .flat_map(|(p, fd)| fd.upgrade().map(|fd| (p, fd)))
        {
            device.syncobj_eventfd(new.syncobj, *point, eventfd.as_fd(), false)?;
            new.event_fds.push((*point, Arc::downgrade(&eventfd)));
        }
        *ctx = new;
        Ok(())
    }

    pub(super) fn close_device(&self) {
        self.dev_ctx.lock().unwrap().close_device()
    }
}

#[derive(Debug)]
struct DrmTimelineDeviceSpecific {
    device: Option<DrmDeviceFd>,
    syncobj: drm::control::syncobj::Handle,
    event_fds: Vec<(u64, Weak<OwnedFd>)>,
}

impl DrmTimelineDeviceSpecific {
    fn import(fd: BorrowedFd<'_>, device: &DrmDeviceFd) -> io::Result<Self> {
        let syncobj = device.fd_to_syncobj(fd, false)?;
        Ok(DrmTimelineDeviceSpecific {
            device: Some(device.clone()),
            syncobj,
            event_fds: Vec::new(),
        })
    }

    fn close_device(&mut self) {
        if let Some(device) = self.device.as_ref() {
            let _ = device.destroy_syncobj(self.syncobj);
        }
        self.device = None;
        // The exported timeline fd keeps the kernel object and its eventfd
        // registrations alive. Closing a device must not fabricate completion.
    }
}

impl Drop for DrmTimelineDeviceSpecific {
    fn drop(&mut self) {
        // The handle was imported on `device` in `import`; once the timeline
        // (or this device context, replaced in `update_device`) is gone,
        // nothing refers to it anymore. `close_device` already destroyed it and
        // reset `device`, in which case this does nothing.
        if let Some(device) = self.device.as_ref() {
            let _ = device.destroy_syncobj(self.syncobj);
        }
    }
}

/// DRM timeline syncobj
#[derive(Clone, Debug)]
pub struct DrmTimeline(pub(super) Arc<DrmTimelineInner>);

impl PartialEq for DrmTimeline {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl DrmTimeline {
    /// Import DRM timeline from file descriptor
    pub fn new(device: &DrmDeviceFd, fd: OwnedFd) -> io::Result<Self> {
        Self::new_with_release_queue(device, fd, Weak::new())
    }

    pub(super) fn new_with_release_queue(
        device: &DrmDeviceFd,
        fd: OwnedFd,
        deferred_releases: Weak<DeferredReleases>,
    ) -> io::Result<Self> {
        let dev_ctx = Mutex::new(DrmTimelineDeviceSpecific::import(fd.as_fd(), device)?);
        Ok(Self(Arc::new(DrmTimelineInner {
            timeline_fd: fd,
            dev_ctx,
            deferred_releases,
        })))
    }

    /// Query the last signalled timeline point
    pub fn query_signalled_point(&self) -> io::Result<u64> {
        let ctx = self.0.dev_ctx.lock().unwrap();
        let device = ctx
            .device
            .as_ref()
            .ok_or::<io::Error>(io::ErrorKind::InvalidInput.into())?;

        let mut points = [0];
        device.syncobj_timeline_query(&[ctx.syncobj], &mut points, false)?;
        Ok(points[0])
    }
}

/// Point on a DRM timeline syncobj
#[derive(Clone, Debug)]
pub struct DrmSyncPoint {
    pub(super) timeline: DrmTimeline,
    pub(super) point: u64,
}

impl DrmSyncPoint {
    /// Borrow the [`DrmTimeline`] this point lives on.
    pub fn timeline(&self) -> &DrmTimeline {
        &self.timeline
    }

    /// Numeric timeline value for this point.
    pub fn point(&self) -> u64 {
        self.point
    }

    /// Create an eventfd that will be signaled by the syncpoint
    #[cfg(any(target_os = "linux", target_os = "freebsd", target_os = "android"))]
    pub fn eventfd(&self) -> io::Result<Arc<OwnedFd>> {
        let fd = rustix::event::eventfd(
            0,
            rustix::event::EventfdFlags::CLOEXEC | rustix::event::EventfdFlags::NONBLOCK,
        )?;
        let mut ctx = self.timeline.0.dev_ctx.lock().unwrap();
        ctx.device
            .as_ref()
            .ok_or::<io::Error>(io::ErrorKind::InvalidInput.into())?
            .syncobj_eventfd(ctx.syncobj, self.point, fd.as_fd(), false)?;

        let fd = Arc::new(fd);
        ctx.event_fds.retain(|(_, fd)| fd.upgrade().is_some());
        ctx.event_fds.push((self.point, Arc::downgrade(&fd)));
        Ok(fd)
    }

    /// Create an eventfd that will be signaled by the syncpoint
    #[cfg(not(any(target_os = "linux", target_os = "freebsd", target_os = "android")))]
    pub fn eventfd(&self) -> io::Result<Arc<OwnedFd>> {
        Err(io::Error::from(io::ErrorKind::Unsupported))
    }

    /// Signal the sync point.
    pub fn signal(&self) -> io::Result<()> {
        let ctx = self.timeline.0.dev_ctx.lock().unwrap();
        ctx.device
            .as_ref()
            .ok_or::<io::Error>(io::ErrorKind::InvalidInput.into())?
            .syncobj_timeline_signal(&[ctx.syncobj], &[self.point])
    }

    // A completed buffer reader owns its release even while its device is
    // absent. Keep one maximum point per timeline until the protocol owner can
    // import it on a live device, including after the wire object is destroyed.
    pub(crate) fn signal_release(&self) -> io::Result<()> {
        let ctx = self.timeline.0.dev_ctx.lock().unwrap();
        let error = match ctx.device.as_ref() {
            Some(device) => match device.syncobj_timeline_signal(&[ctx.syncobj], &[self.point]) {
                Ok(()) => return Ok(()),
                Err(error) if error.raw_os_error() == Some(libc::ENODEV) => error,
                Err(error) => return Err(error),
            },
            None => io::ErrorKind::InvalidInput.into(),
        };
        let Some(releases) = self.timeline.0.deferred_releases.upgrade() else {
            return Err(error);
        };
        // The queued point retains this allocation, so its address cannot be
        // reused for another timeline until this entry has been retired.
        let key = Arc::as_ptr(&self.timeline.0) as usize;
        releases
            .lock()
            .unwrap()
            .entry(key)
            .and_modify(|pending| pending.point = pending.point.max(self.point))
            .or_insert_with(|| self.clone());
        // Device replacement must not drain the queue between observing the
        // absent device and inserting this release. The updater never holds
        // the queue lock while taking a timeline's context lock.
        drop(ctx);
        Ok(())
    }

    /// Wait for sync point.
    pub fn wait(&self, timeout_nsec: i64) -> io::Result<()> {
        let ctx = self.timeline.0.dev_ctx.lock().unwrap();
        ctx.device
            .as_ref()
            .ok_or::<io::Error>(io::ErrorKind::InvalidInput.into())?
            .syncobj_timeline_wait(&[ctx.syncobj], &[self.point], timeout_nsec, false, false, false)?;
        Ok(())
    }

    /// Export DRM sync file for sync point.
    pub fn export_sync_file(&self) -> io::Result<OwnedFd> {
        let ctx = self.timeline.0.dev_ctx.lock().unwrap();
        let Some(device) = ctx.device.as_ref() else {
            return Err(io::ErrorKind::InvalidInput.into());
        };

        let syncobj = device.create_syncobj(false)?;
        if let Err(err) = device.syncobj_timeline_transfer(ctx.syncobj, syncobj, self.point, 0) {
            let _ = device.destroy_syncobj(syncobj);
            return Err(err);
        };

        let res = device.syncobj_to_fd(syncobj, true);
        let _ = device.destroy_syncobj(syncobj);
        res
    }

    /// Import a DRM sync file fd as the materialized fence at this
    /// timeline point. Symmetric counterpart of
    /// [`DrmSyncPoint::export_sync_file`].
    ///
    /// Internally creates a fresh binary syncobj, imports the sync
    /// file fence into it via the `IMPORT_SYNC_FILE` ioctl (raw
    /// because the `drm` crate's public wrapper hardcodes the
    /// destination handle to `0`, which the kernel rejects with
    /// `ENOENT` — only the two-step `drmSyncobjCreate` +
    /// `drmSyncobjImportSyncFile(existing_handle, fd)` pattern is
    /// supported, mirroring libdrm). Then transfers the temp's
    /// point 0 into this timeline at `self.point` and destroys the
    /// temp.
    ///
    /// Mirrors `wlr_drm_syncobj_timeline_import_sync_file`. Used by
    /// compositors that drive Vulkan explicit sync via
    /// `VK_EXTERNAL_SEMAPHORE_HANDLE_TYPE_SYNC_FD_BIT` and need to
    /// inject the resulting sync-file fence into the client's
    /// release point.
    pub fn import_sync_file(&self, fd: BorrowedFd<'_>) -> io::Result<()> {
        use rustix::ioctl::{Updater, ioctl, opcode::read_write};
        use std::os::fd::AsRawFd;

        let ctx = self.timeline.0.dev_ctx.lock().unwrap();
        let Some(device) = ctx.device.as_ref() else {
            return Err(io::ErrorKind::InvalidInput.into());
        };

        let tmp = device.create_syncobj(false)?;

        const DRM_IOCTL_SYNCOBJ_FD_TO_HANDLE: rustix::ioctl::Opcode =
            read_write::<drm_ffi::drm_syncobj_handle>(drm_ffi::DRM_IOCTL_BASE, 0xC2);

        let mut args = drm_ffi::drm_syncobj_handle {
            handle: tmp.into(),
            flags: drm_ffi::DRM_SYNCOBJ_FD_TO_HANDLE_FLAGS_IMPORT_SYNC_FILE,
            fd: fd.as_raw_fd(),
            pad: 0,
            point: 0,
        };
        // SAFETY: `device.as_fd()` is a valid DRM device fd;
        // `drm_ffi::drm_syncobj_handle` is the type expected by the
        // DRM_IOCTL_SYNCOBJ_FD_TO_HANDLE ioctl.
        let res = unsafe {
            ioctl(
                device.as_fd(),
                Updater::<DRM_IOCTL_SYNCOBJ_FD_TO_HANDLE, _>::new(&mut args),
            )
        };
        if let Err(err) = res {
            let _ = device.destroy_syncobj(tmp);
            return Err(err.into());
        }

        let res = device.syncobj_timeline_transfer(tmp, ctx.syncobj, 0, self.point);
        let _ = device.destroy_syncobj(tmp);
        res
    }

    /// Create an [`calloop::EventSource`] and [`Blocker`] for this sync point.
    ///
    /// This will fail if `drmSyncobjEventfd` isn't supported by the device. See
    /// [`supports_syncobj_eventfd`](super::supports_syncobj_eventfd).
    pub fn generate_blocker(&self) -> io::Result<(DrmSyncPointBlocker, DrmSyncPointSource)> {
        let fd = self.eventfd()?;
        let signal = Arc::new(AtomicBool::new(false));
        let blocker = DrmSyncPointBlocker {
            signal: signal.clone(),
        };
        let source = DrmSyncPointSource {
            source: Generic::new(fd, Interest::READ, Mode::Level),
            signal,
        };
        Ok((blocker, source))
    }
}

impl Fence for DrmSyncPoint {
    fn completion_result(&self, native: Option<BorrowedFd<'_>>) -> io::Result<()> {
        if let Some(fd) = native {
            return crate::backend::renderer::sync::sync_file_result(fd);
        }
        let fd = self.export_sync_file()?;
        crate::backend::renderer::sync::sync_file_result(fd.as_fd())
    }

    fn is_signaled(&self) -> bool {
        self.timeline
            .query_signalled_point()
            .ok()
            .is_some_and(|point| point >= self.point)
    }

    fn wait(&self) -> Result<(), Interrupted> {
        self.wait(i64::MAX).map_err(|_| Interrupted)
    }

    fn is_exportable(&self) -> bool {
        true
    }

    fn export(&self) -> Option<OwnedFd> {
        self.export_sync_file().ok()
    }
}

/// Event source generating an event when a [`DrmSyncPoint`] is signalled..
#[derive(Debug)]
pub struct DrmSyncPointSource {
    source: Generic<Arc<OwnedFd>>,
    signal: Arc<AtomicBool>,
}

impl EventSource for DrmSyncPointSource {
    type Event = ();
    type Metadata = ();
    type Ret = Result<(), std::io::Error>;
    type Error = io::Error;

    fn process_events<C>(
        &mut self,
        readiness: Readiness,
        token: Token,
        mut callback: C,
    ) -> Result<PostAction, Self::Error>
    where
        C: FnMut(Self::Event, &mut Self::Metadata) -> Self::Ret,
    {
        self.signal.store(true, Ordering::SeqCst);
        self.source
            .process_events(readiness, token, |_, _| Ok(PostAction::Remove))?;
        callback((), &mut ())?;
        Ok(PostAction::Remove)
    }

    fn register(&mut self, poll: &mut Poll, token_factory: &mut TokenFactory) -> calloop::Result<()> {
        self.source.register(poll, token_factory)?;
        Ok(())
    }

    fn reregister(&mut self, poll: &mut Poll, token_factory: &mut TokenFactory) -> calloop::Result<()> {
        self.source.reregister(poll, token_factory)?;
        Ok(())
    }

    fn unregister(&mut self, poll: &mut Poll) -> calloop::Result<()> {
        self.source.unregister(poll)?;
        Ok(())
    }
}

/// [`Blocker`] implementation for an accompaning [`DrmSyncPointSource`]
#[derive(Debug)]
pub struct DrmSyncPointBlocker {
    signal: Arc<AtomicBool>,
}

impl Blocker for DrmSyncPointBlocker {
    fn state(&self) -> BlockerState {
        if self.signal.load(Ordering::SeqCst) {
            BlockerState::Released
        } else {
            BlockerState::Pending
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::DeviceFd;
    use std::{fs::OpenOptions, path::Path};

    fn open_device(path: &Path) -> DrmDeviceFd {
        let fd = OpenOptions::new().read(true).write(true).open(path).unwrap();
        DrmDeviceFd::new(DeviceFd::from(OwnedFd::from(fd)))
    }

    #[test]
    #[ignore = "requires SMITHAY_TEST_DRM_RENDER_NODE and a timeline-capable render node"]
    fn timeline_retains_device_and_close_does_not_signal_acquire() {
        let path = std::env::var_os("SMITHAY_TEST_DRM_RENDER_NODE").unwrap();
        let producer = open_device(Path::new(&path));
        let importer = open_device(Path::new(&path));
        let handle = producer.create_syncobj(false).unwrap();
        let fd = producer.syncobj_to_fd(handle, false).unwrap();
        let timeline = DrmTimeline::new(&importer, fd).unwrap();
        drop(importer);
        let point = DrmSyncPoint {
            timeline: timeline.clone(),
            point: 9,
        };
        let event = point.eventfd().unwrap();
        timeline.0.close_device();
        let mut bytes = [0; 8];
        assert_eq!(
            rustix::io::read(&event, &mut bytes).unwrap_err(),
            rustix::io::Errno::AGAIN
        );
        producer.syncobj_timeline_signal(&[handle], &[9]).unwrap();
        assert_eq!(rustix::io::read(&event, &mut bytes).unwrap(), 8);
        assert_eq!(u64::from_ne_bytes(bytes), 1);
        timeline.0.update_device(&open_device(Path::new(&path))).unwrap();
        assert_eq!(timeline.query_signalled_point().unwrap(), 9);
        producer.destroy_syncobj(handle).unwrap();
    }
}
