//! High-resolution timestamps for the core seat input resources.
//!
//! Register [`InputTimestampsState`] alongside the seat globals. Subscriptions
//! belong to individual input resources; only events delivered to those resources
//! carry timestamps. Releasing an input resource leaves its subscriptions inert.

use std::sync::{
    Mutex,
    atomic::{AtomicBool, Ordering},
};

use wayland_protocols::wp::input_timestamps::zv1::server::{
    zwp_input_timestamps_manager_v1::{self, ZwpInputTimestampsManagerV1},
    zwp_input_timestamps_v1::{self, ZwpInputTimestampsV1},
};
use wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource, Weak,
    backend::{ClientId, GlobalId},
    protocol::{wl_keyboard::WlKeyboard, wl_pointer::WlPointer, wl_touch::WlTouch},
};

use super::{
    Dispatch2, GlobalData, GlobalDispatch2,
    seat::{KeyboardUserData, PointerUserData, TouchUserData},
};
use crate::{backend::input::InputTime, input::SeatHandler};

/// The standard input-timestamps v1 global.
#[derive(Debug)]
pub struct InputTimestampsState {
    global: GlobalId,
}

impl InputTimestampsState {
    /// Advertise high-resolution timestamps for keyboard, pointer and touch.
    pub fn new<D>(display: &DisplayHandle) -> Self
    where
        D: SeatHandler + GlobalDispatch<ZwpInputTimestampsManagerV1, GlobalData> + 'static,
    {
        Self {
            global: display.create_global::<D, ZwpInputTimestampsManagerV1, _>(1, GlobalData),
        }
    }

    /// The advertised global.
    pub fn global(&self) -> GlobalId {
        self.global.clone()
    }
}

#[derive(Debug, Default)]
pub(crate) struct Timestamps {
    active: AtomicBool,
    subscriptions: Mutex<Vec<Weak<ZwpInputTimestampsV1>>>,
}

impl Timestamps {
    fn subscribe(&self, timestamps: &ZwpInputTimestampsV1) {
        let mut subscriptions = self.subscriptions.lock().unwrap();
        subscriptions.push(timestamps.downgrade());
        self.active.store(true, Ordering::Relaxed);
    }

    fn remove(&self, timestamps: &ZwpInputTimestampsV1) {
        let mut subscriptions = self.subscriptions.lock().unwrap();
        subscriptions.retain(|subscription| subscription.id() != timestamps.id());
        self.active.store(!subscriptions.is_empty(), Ordering::Relaxed);
    }

    pub(crate) fn send(&self, time: InputTime) {
        if !self.active.load(Ordering::Relaxed) {
            return;
        }
        let seconds = time.micros() / 1_000_000;
        let nanoseconds = ((time.micros() % 1_000_000) * 1000) as u32;
        for subscription in self.subscriptions.lock().unwrap().iter() {
            if let Ok(subscription) = subscription.upgrade() {
                subscription.timestamp((seconds >> 32) as u32, seconds as u32, nanoseconds);
            }
        }
    }
}

#[derive(Debug)]
enum Device {
    Keyboard(Weak<WlKeyboard>),
    Pointer(Weak<WlPointer>),
    Touch(Weak<WlTouch>),
}

/// Resource association for an input timestamp subscription.
#[derive(Debug)]
pub struct InputTimestampsData(Device);

impl<D: SeatHandler + 'static> GlobalDispatch2<ZwpInputTimestampsManagerV1, D> for GlobalData
where
    D: Dispatch<ZwpInputTimestampsManagerV1, GlobalData>,
{
    fn bind(
        &self,
        _state: &mut D,
        _display: &DisplayHandle,
        _client: &Client,
        resource: New<ZwpInputTimestampsManagerV1>,
        data_init: &mut DataInit<'_, D>,
    ) {
        data_init.init(resource, GlobalData);
    }
}

impl<D: SeatHandler + 'static> Dispatch2<ZwpInputTimestampsManagerV1, D> for GlobalData
where
    D: Dispatch<ZwpInputTimestampsV1, InputTimestampsData>,
{
    fn request(
        &self,
        _state: &mut D,
        _client: &Client,
        _resource: &ZwpInputTimestampsManagerV1,
        request: zwp_input_timestamps_manager_v1::Request,
        _display: &DisplayHandle,
        data_init: &mut DataInit<'_, D>,
    ) {
        use zwp_input_timestamps_manager_v1::Request;
        match request {
            Request::GetKeyboardTimestamps { id, keyboard } => {
                let timestamps =
                    data_init.init(id, InputTimestampsData(Device::Keyboard(keyboard.downgrade())));
                keyboard
                    .data::<KeyboardUserData<D>>()
                    .unwrap()
                    .timestamps
                    .subscribe(&timestamps);
            }
            Request::GetPointerTimestamps { id, pointer } => {
                let timestamps =
                    data_init.init(id, InputTimestampsData(Device::Pointer(pointer.downgrade())));
                pointer
                    .data::<PointerUserData<D>>()
                    .unwrap()
                    .timestamps
                    .subscribe(&timestamps);
            }
            Request::GetTouchTimestamps { id, touch } => {
                let timestamps = data_init.init(id, InputTimestampsData(Device::Touch(touch.downgrade())));
                touch
                    .data::<TouchUserData<D>>()
                    .unwrap()
                    .timestamps
                    .subscribe(&timestamps);
            }
            Request::Destroy => (),
            _ => unreachable!(),
        }
    }
}

impl<D: SeatHandler + 'static> Dispatch2<ZwpInputTimestampsV1, D> for InputTimestampsData {
    fn request(
        &self,
        _state: &mut D,
        _client: &Client,
        _resource: &ZwpInputTimestampsV1,
        _request: zwp_input_timestamps_v1::Request,
        _display: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
    }

    fn destroyed(&self, _state: &mut D, _client: ClientId, resource: &ZwpInputTimestampsV1) {
        match &self.0 {
            Device::Keyboard(keyboard) => {
                if let Ok(keyboard) = keyboard.upgrade() {
                    keyboard
                        .data::<KeyboardUserData<D>>()
                        .unwrap()
                        .timestamps
                        .remove(resource);
                }
            }
            Device::Pointer(pointer) => {
                if let Ok(pointer) = pointer.upgrade() {
                    pointer
                        .data::<PointerUserData<D>>()
                        .unwrap()
                        .timestamps
                        .remove(resource);
                }
            }
            Device::Touch(touch) => {
                if let Ok(touch) = touch.upgrade() {
                    touch
                        .data::<TouchUserData<D>>()
                        .unwrap()
                        .timestamps
                        .remove(resource);
                }
            }
        }
    }
}
