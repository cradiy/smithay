//! Helper for synchronizing rendering operations
use std::{
    error::Error,
    fmt,
    os::unix::io::OwnedFd,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use downcast_rs::{Downcast, impl_downcast};

#[cfg(feature = "backend_egl")]
mod egl;

/// Waiting for the fence was interrupted for an unknown reason.
///
/// This does not mean that the fence is signalled or not, neither that
/// any timeout was reached. Waiting should be attempted again.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Interrupted;

impl fmt::Display for Interrupted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Wait for Fence was interrupted")
    }
}
impl Error for Interrupted {}

/// A fence that will be signaled in finite time
pub trait Fence: std::fmt::Debug + Send + Sync + Downcast {
    /// Queries the state of the fence
    fn is_signaled(&self) -> bool;

    /// Blocks the current thread until the fence is signaled
    fn wait(&self) -> Result<(), Interrupted>;

    /// Returns whether this fence can be exported
    /// as a native fence fd
    fn is_exportable(&self) -> bool;

    /// Export this fence as a native fence fd
    fn export(&self) -> Option<OwnedFd>;
}
impl_downcast!(Fence);

/// A sync point the will be signaled in finite time
#[derive(Debug, Clone)]
#[must_use = "this `SyncPoint` may contain a fence that should be awaited, failing to do so may result in unexpected rendering artifacts"]
pub struct SyncPoint {
    fence: Option<Arc<SharedFence<dyn Fence>>>,
}

#[derive(Debug)]
struct SharedFence<F: ?Sized> {
    reached: AtomicBool,
    fence: F,
}

impl Default for SyncPoint {
    fn default() -> Self {
        Self::signaled()
    }
}

impl SyncPoint {
    /// Create an already signaled sync point
    pub fn signaled() -> Self {
        Self {
            fence: Default::default(),
        }
    }

    /// Returns `true` if `SyncPoint` contains a [`Fence`]
    pub fn contains_fence(&self) -> bool {
        self.fence.is_some()
    }

    /// Get a reference to the underlying [`Fence`] if any
    ///
    /// Returns `None` if the sync point does not contain a fence
    /// or contains a different type of fence
    pub fn get<F: Fence + 'static>(&self) -> Option<&F> {
        self.fence.as_ref().and_then(|f| f.fence.downcast_ref())
    }

    /// Queries the state of the sync point
    ///
    /// Will always return `true` in case the sync point does not contain a fence
    pub fn is_reached(&self) -> bool {
        self.fence
            .as_ref()
            .map(|f| {
                if f.reached.load(Ordering::Acquire) {
                    return true;
                }
                if f.fence.is_signaled() {
                    f.reached.store(true, Ordering::Release);
                    true
                } else {
                    false
                }
            })
            .unwrap_or(true)
    }

    /// Record completion proven outside the backend, for example by polling an
    /// exported sync file. The caller must verify completion of this exact
    /// point before calling this method. All clones retain that proof, so later
    /// queries and waits no longer depend on a potentially lost device.
    /// Completion does not establish successful execution of rendering work.
    pub fn mark_reached(&self) {
        if let Some(fence) = &self.fence {
            fence.reached.store(true, Ordering::Release);
        }
    }

    /// Blocks the current thread until the sync point is signaled
    ///
    /// If the sync point does not contain a fence this will never block.
    #[profiling::function]
    pub fn wait(&self) -> Result<(), Interrupted> {
        if let Some(fence) = self.fence.as_ref() {
            if !fence.reached.load(Ordering::Acquire) {
                fence.fence.wait()?;
                fence.reached.store(true, Ordering::Release);
            }
            Ok(())
        } else {
            Ok(())
        }
    }

    /// Returns whether this sync point can be exported as a native fence fd
    ///
    /// Will always return `false` in case the sync point does not contain a fence
    pub fn is_exportable(&self) -> bool {
        self.fence
            .as_ref()
            .map(|f| f.fence.is_exportable())
            .unwrap_or(false)
    }

    /// Export this [`SyncPoint`] as a native fence fd
    ///
    /// Will always return `None` in case the sync point does not contain a fence
    #[profiling::function]
    pub fn export(&self) -> Option<OwnedFd> {
        self.fence.as_ref().and_then(|f| f.fence.export())
    }
}

impl<T: Fence + 'static> From<T> for SyncPoint {
    #[inline]
    fn from(value: T) -> Self {
        SyncPoint {
            fence: Some(Arc::new(SharedFence {
                reached: AtomicBool::new(false),
                fence: value,
            })),
        }
    }
}
