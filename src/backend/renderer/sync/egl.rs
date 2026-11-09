use std::{
    io,
    os::fd::{AsFd, BorrowedFd, OwnedFd},
    time::Duration,
};

use crate::backend::{
    egl::fence::EGLFence,
    renderer::sync::{Fence, Interrupted},
};

impl Fence for EGLFence {
    fn wait(&self) -> Result<(), Interrupted> {
        self.client_wait(None, false).map(|_| ()).map_err(|err| {
            tracing::warn!(?err, "Waiting for fence was interrupted");
            Interrupted
        })
    }

    fn is_exportable(&self) -> bool {
        self.is_native()
    }

    fn export(&self) -> Option<OwnedFd> {
        self.export().ok()
    }

    fn completion_result(&self, native: Option<BorrowedFd<'_>>) -> io::Result<()> {
        if let Some(fd) = native {
            return super::sync_file_result(fd);
        }
        if self.is_native() {
            let fd = self.export().map_err(io::Error::other)?;
            return super::sync_file_result(fd.as_fd());
        }
        match self
            .client_wait(Some(Duration::ZERO), false)
            .map_err(io::Error::other)?
        {
            true => Ok(()),
            false => Err(io::ErrorKind::WouldBlock.into()),
        }
    }

    fn is_signaled(&self) -> bool {
        self.client_wait(Some(Duration::ZERO), false).unwrap_or(false)
    }
}
