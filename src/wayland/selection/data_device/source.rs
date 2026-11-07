use std::{
    any::Any,
    cell::RefCell,
    os::fd::{AsFd, OwnedFd},
    sync::Mutex,
};
use tracing::debug;

use wayland_server::{
    DisplayHandle, Resource,
    backend::ClientId,
    protocol::{
        wl_data_source::{self, WlDataSource},
        wl_surface::WlSurface,
    },
};

use crate::input::{
    Seat,
    dnd::{DndAction, Source, SourceMetadata},
};
use crate::utils::{IsAlive, alive_tracker::AliveTracker};
use crate::wayland::Dispatch2;
use crate::wayland::selection::offer::OfferReplySource;
use crate::wayland::selection::seat_data::SeatData;
use crate::wayland::selection::source::SelectionSourceProvider;

use super::DataDeviceHandler;

#[doc(hidden)]
#[derive(Debug)]
pub struct DataSourceUserData {
    pub(crate) inner: Mutex<SourceMetadata>,
    alive_tracker: AliveTracker,
    pub(super) actions_set: std::sync::atomic::AtomicBool,
    display_handle: DisplayHandle,
}

impl DataSourceUserData {
    pub(super) fn new(display_handle: DisplayHandle) -> Self {
        Self {
            inner: Default::default(),
            alive_tracker: Default::default(),
            actions_set: Default::default(),
            display_handle,
        }
    }
}

impl<D> Dispatch2<WlDataSource, D> for DataSourceUserData
where
    D: DataDeviceHandler,
    D: 'static,
{
    fn request(
        &self,
        state: &mut D,
        _client: &wayland_server::Client,
        resource: &WlDataSource,
        request: wl_data_source::Request,
        _dhandle: &DisplayHandle,
        _data_init: &mut wayland_server::DataInit<'_, D>,
    ) {
        let mut data = self.inner.lock().unwrap();

        match request {
            wl_data_source::Request::Offer { mime_type } => {
                data.mime_types.push(mime_type);
            }
            wl_data_source::Request::SetActions { dnd_actions } => {
                if self.actions_set.swap(true, std::sync::atomic::Ordering::Relaxed)
                    || state.data_device_state().used_sources.contains_key(resource)
                {
                    resource.post_error(
                        wl_data_source::Error::InvalidSource,
                        "Source actions must be set once before use.",
                    );
                    return;
                }
                match dnd_actions.into_result() {
                    Ok(actions) => data.dnd_actions = DndAction::vec_from_wl(actions),
                    Err(_) => resource.post_error(
                        wl_data_source::Error::InvalidActionMask,
                        "Invalid source action mask.",
                    ),
                }
            }
            wl_data_source::Request::Destroy => {}
            _ => unreachable!(),
        }
    }

    fn destroyed(&self, state: &mut D, _client: ClientId, source: &WlDataSource) {
        self.alive_tracker.destroy_notify();

        // Remove the source from the used ones.
        let seat = match state
            .data_device_state()
            .used_sources
            .remove(source)
            .as_ref()
            .and_then(Seat::<D>::from_resource)
        {
            Some(seat) => seat,
            None => return,
        };

        let mut seat_data = seat
            .user_data()
            .get::<RefCell<SeatData<D::SelectionUserData>>>()
            .unwrap()
            .borrow_mut();

        match seat_data.get_clipboard_selection() {
            Some(OfferReplySource::Client(SelectionSourceProvider::DataDevice(set_source)))
                if set_source == source =>
            {
                seat_data.set_clipboard_selection::<D>(&self.display_handle, None)
            }
            _ => (),
        }
    }
}

impl IsAlive for WlDataSource {
    #[inline]
    fn alive(&self) -> bool {
        let data: &DataSourceUserData = self.data().unwrap();
        data.alive_tracker.alive()
    }
}

impl Source for WlDataSource {
    fn accepted(&self, mime_type: Option<String>) {
        self.target(mime_type);
    }

    fn metadata(&self) -> Option<SourceMetadata> {
        self.data::<DataSourceUserData>().map(|data| {
            let mut metadata = data.inner.lock().unwrap().clone();
            if self.version() < 3 {
                metadata.dnd_actions = [DndAction::Copy].into_iter().collect();
            }
            metadata
        })
    }

    fn choose_action(&self, action: DndAction) {
        if self.version() >= wl_data_source::EVT_ACTION_SINCE {
            self.action(action.into());
        }
    }

    fn send(&self, mime_type: &str, fd: OwnedFd) {
        debug!(?mime_type, "DnD transfer request");
        self.send(mime_type.to_owned(), fd.as_fd());
    }

    fn drop_performed(&self) {
        if self.version() >= wl_data_source::EVT_DND_DROP_PERFORMED_SINCE {
            self.dnd_drop_performed();
        }
    }

    fn cancel(&self) {
        self.cancelled();
    }

    fn finished(&self) {
        if self.version() >= wl_data_source::EVT_DND_FINISHED_SINCE {
            self.dnd_finished();
        }
    }
}

impl Source for WlSurface {
    fn is_client_local(&self, target: &dyn Any) -> bool {
        target
            .downcast_ref::<WlSurface>()
            .is_some_and(|target| target.id().same_client_as(&self.id()))
    }

    fn metadata(&self) -> Option<SourceMetadata> {
        None
    }

    fn choose_action(&self, action: DndAction) {
        let _ = action;
    }

    fn send(&self, mime_type: &str, fd: OwnedFd) {
        let _ = (mime_type, fd);
        unreachable!("Local dnd drops can't send");
    }

    fn drop_performed(&self) {}
    fn cancel(&self) {}
    fn finished(&self) {}
}
