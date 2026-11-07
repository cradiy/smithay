use std::cell::RefCell;

use tracing::debug;
use wayland_server::{
    Client, DataInit, DisplayHandle, Resource,
    protocol::{
        wl_data_device::{self, WlDataDevice},
        wl_seat::WlSeat,
    },
};

use crate::{
    input::{Seat, SeatHandler, dnd::DndFocus},
    utils::Serial,
    wayland::{
        Dispatch2, compositor,
        seat::WaylandFocus,
        selection::{
            SelectionTarget,
            device::SelectionDevice,
            offer::OfferReplySource,
            seat_data::SeatData,
            source::{SelectionSource, SelectionSourceProvider},
        },
    },
};

use super::{DataDeviceHandler, GrabType};

/// WlSurface role of drag and drop icon
pub const DND_ICON_ROLE: &str = "dnd_icon";

#[doc(hidden)]
#[derive(Debug)]
pub struct DataDeviceUserData {
    pub(crate) wl_seat: WlSeat,
}

impl<D> Dispatch2<WlDataDevice, D> for DataDeviceUserData
where
    D: DataDeviceHandler,
    D: SeatHandler,
    <D as SeatHandler>::PointerFocus: DndFocus<D>,
    <D as SeatHandler>::TouchFocus: DndFocus<D>,
    <D as SeatHandler>::KeyboardFocus: WaylandFocus,
    D: 'static,
{
    fn request(
        &self,
        handler: &mut D,
        client: &Client,
        resource: &WlDataDevice,
        request: wl_data_device::Request,
        dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        let seat = match Seat::<D>::from_resource(&self.wl_seat) {
            Some(seat) => seat,
            None => return,
        };

        match request {
            wl_data_device::Request::StartDrag {
                source,
                origin,
                icon,
                serial,
            } => {
                let serial = Serial::from(serial);
                let kind = if seat.get_pointer().is_some_and(|pointer| pointer.has_grab(serial)) {
                    GrabType::Pointer
                } else if seat.get_touch().is_some_and(|touch| touch.has_grab(serial)) {
                    GrabType::Touch
                } else {
                    debug!(serial = ?serial, client = ?client, "denying drag from client without implicit grab");
                    return;
                };
                if !handler.allow_dnd(&seat, &origin, serial) {
                    return;
                }
                if let Some(source) = source.as_ref() {
                    if handler.data_device_state().used_sources.contains_key(source) {
                        resource.post_error(
                            wl_data_device::Error::UsedSource,
                            "Data source has already been used.",
                        );
                        return;
                    }
                    handler
                        .data_device_state()
                        .used_sources
                        .insert(source.clone(), self.wl_seat.clone());
                }
                if let Some(ref icon) = icon {
                    if compositor::give_role(icon, DND_ICON_ROLE).is_err()
                        && compositor::get_role(icon) != Some(DND_ICON_ROLE)
                    {
                        resource.post_error(
                            wl_data_device::Error::Role,
                            "Given surface already has another role.",
                        );
                        return;
                    }
                }
                if let Some(source) = source {
                    handler.dnd_requested(source, icon, seat, serial, kind);
                } else {
                    handler.dnd_requested(origin, icon, seat, serial, kind);
                }
            }

            wl_data_device::Request::SetSelection { source, serial } => {
                let seat_data = match seat.get_keyboard() {
                    Some(keyboard) if keyboard.client_of_object_has_focus(&resource.id()) => seat
                        .user_data()
                        .get::<RefCell<SeatData<D::SelectionUserData>>>()
                        .unwrap(),
                    _ => {
                        debug!(
                            client = ?client,
                            "denying setting selection by a non-focused client"
                        );
                        return;
                    }
                };

                if !handler.allow_selection(&seat, serial.into()) {
                    return;
                }

                if let Some(source) = source.as_ref() {
                    if handler.data_device_state().used_sources.contains_key(source) {
                        resource.post_error(
                            wl_data_device::Error::UsedSource,
                            "Data source has already been used.",
                        );
                        return;
                    }
                    if source
                        .data::<super::DataSourceUserData>()
                        .unwrap()
                        .actions_set
                        .load(std::sync::atomic::Ordering::Relaxed)
                    {
                        source.post_error(
                            wayland_server::protocol::wl_data_source::Error::InvalidSource,
                            "Drag sources cannot become clipboard selections.",
                        );
                        return;
                    }
                    handler
                        .data_device_state()
                        .used_sources
                        .insert(source.clone(), self.wl_seat.clone());
                }

                let source = source.map(SelectionSourceProvider::DataDevice);

                handler.new_selection(
                    SelectionTarget::Clipboard,
                    source.clone().map(|provider| SelectionSource { provider }),
                    seat.clone(),
                );

                // The client has kbd focus, it can set the selection
                seat_data
                    .borrow_mut()
                    .set_clipboard_selection::<D>(dh, source.map(OfferReplySource::Client));
            }
            wl_data_device::Request::Release => seat
                .user_data()
                .get::<RefCell<SeatData<D::SelectionUserData>>>()
                .unwrap()
                .borrow_mut()
                .retain_devices(|ndd| match ndd {
                    SelectionDevice::DataDevice(ndd) => ndd != resource,
                    _ => true,
                }),

            _ => unreachable!(),
        }
    }

    fn destroyed(&self, _state: &mut D, _client: wayland_server::backend::ClientId, resource: &WlDataDevice) {
        if let Some(seat) = Seat::<D>::from_resource(&self.wl_seat) {
            if let Some(seat_data) = seat.user_data().get::<RefCell<SeatData<D::SelectionUserData>>>() {
                seat_data.borrow_mut().retain_devices(|ndd| match ndd {
                    SelectionDevice::DataDevice(ndd) => ndd != resource,
                    _ => true,
                });
            }
        }
    }
}
