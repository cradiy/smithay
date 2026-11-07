use std::{
    rc::Rc,
    sync::atomic::{self, AtomicBool},
};

use wayland_protocols::wp::keyboard_shortcuts_inhibit::zv1::server::{
    zwp_keyboard_shortcuts_inhibit_manager_v1::{self, ZwpKeyboardShortcutsInhibitManagerV1},
    zwp_keyboard_shortcuts_inhibitor_v1::{self, ZwpKeyboardShortcutsInhibitorV1},
};
use wayland_server::{
    Dispatch, Resource,
    backend::{ClientId, ObjectId},
    protocol::wl_surface::WlSurface,
};

use crate::{
    input::{Seat, SeatHandler},
    wayland::{Dispatch2, GlobalData, GlobalDispatch2},
};

use super::KeyboardShortcutsInhibitHandler;

/// User data of [ZwpKeyboardShortcutsInhibitorV1] object
#[derive(Debug)]
pub struct KeyboardShortcutsInhibitorUserData {
    /// Seat that is being inhibited
    pub(crate) seat: ObjectId,
    /// Surface that is inhibiting shortcuts
    pub(crate) surface: WlSurface,
    pub(crate) is_active: AtomicBool,
}

impl<D> GlobalDispatch2<ZwpKeyboardShortcutsInhibitManagerV1, D> for GlobalData
where
    D: KeyboardShortcutsInhibitHandler,
    D: Dispatch<ZwpKeyboardShortcutsInhibitManagerV1, GlobalData>,
    D: Dispatch<ZwpKeyboardShortcutsInhibitorV1, KeyboardShortcutsInhibitorUserData>,
{
    fn bind(
        &self,
        _state: &mut D,
        _handle: &wayland_server::DisplayHandle,
        _client: &wayland_server::Client,
        resource: wayland_server::New<ZwpKeyboardShortcutsInhibitManagerV1>,
        data_init: &mut wayland_server::DataInit<'_, D>,
    ) {
        data_init.init(resource, GlobalData);
    }
}

impl<D> Dispatch2<ZwpKeyboardShortcutsInhibitManagerV1, D> for GlobalData
where
    D: KeyboardShortcutsInhibitHandler,
    D: SeatHandler,
    D: Dispatch<ZwpKeyboardShortcutsInhibitorV1, KeyboardShortcutsInhibitorUserData>,
{
    fn request(
        &self,
        handler: &mut D,
        _client: &wayland_server::Client,
        resource: &ZwpKeyboardShortcutsInhibitManagerV1,
        request: zwp_keyboard_shortcuts_inhibit_manager_v1::Request,
        _dhandle: &wayland_server::DisplayHandle,
        data_init: &mut wayland_server::DataInit<'_, D>,
    ) {
        match request {
            zwp_keyboard_shortcuts_inhibit_manager_v1::Request::InhibitShortcuts { id, surface, seat } => {
                let seat_id = seat.id();

                // Multiple wl_seat bindings refer to the same logical seat.
                // A removed seat leaves its existing resources inert.
                let inhibitors = Seat::<D>::from_resource(&seat)
                    .map(|seat| super::SeatData::get(&seat).borrow().inhibitors.clone());
                if inhibitors
                    .as_ref()
                    .is_some_and(|list| list.borrow().surface_has_inhibitor(&surface))
                {
                    resource.post_error(
                        zwp_keyboard_shortcuts_inhibit_manager_v1::Error::AlreadyInhibited,
                        "Keyboard shortcuts for this surface are already inhibited",
                    );
                    return;
                }

                let inhibitor = data_init.init(
                    id,
                    KeyboardShortcutsInhibitorUserData {
                        seat: seat_id.clone(),
                        surface,
                        is_active: AtomicBool::new(false),
                    },
                );

                let inhibitor = super::KeyboardShortcutsInhibitor { inhibitor };

                if let Some(list) = inhibitors {
                    list.borrow_mut().push(inhibitor.clone());
                    handler
                        .keyboard_shortcuts_inhibit_state()
                        .inhibitors
                        .insert(inhibitor.inhibitor.id(), Rc::downgrade(&list));
                    handler.new_inhibitor(inhibitor);
                }
            }
            zwp_keyboard_shortcuts_inhibit_manager_v1::Request::Destroy => {}
            _ => unreachable!(),
        }
    }
}

impl<D> Dispatch2<ZwpKeyboardShortcutsInhibitorV1, D> for KeyboardShortcutsInhibitorUserData
where
    D: KeyboardShortcutsInhibitHandler,
{
    fn request(
        &self,
        _handler: &mut D,
        _client: &wayland_server::Client,
        _inhibitor: &ZwpKeyboardShortcutsInhibitorV1,
        request: zwp_keyboard_shortcuts_inhibitor_v1::Request,
        _dhandle: &wayland_server::DisplayHandle,
        _data_init: &mut wayland_server::DataInit<'_, D>,
    ) {
        match request {
            zwp_keyboard_shortcuts_inhibitor_v1::Request::Destroy => {}
            _ => unreachable!(),
        }
    }

    fn destroyed(&self, handler: &mut D, _client: ClientId, wl_inhibitor: &ZwpKeyboardShortcutsInhibitorV1) {
        self.is_active.store(false, atomic::Ordering::Release);

        if let Some(list) = handler
            .keyboard_shortcuts_inhibit_state()
            .inhibitors
            .remove(&wl_inhibitor.id())
            .and_then(|list| list.upgrade())
        {
            let inhibitor = list.borrow_mut().remove(&self.surface.id());
            if let Some(inhibitor) = inhibitor {
                handler.inhibitor_destroyed(inhibitor)
            }
        }
    }
}
