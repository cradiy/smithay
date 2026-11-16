//! Tablet-v2 pad controls and mode groups.

mod axis;
pub use crate::input::tablet::TabletPadControl;
use axis::{Ring, Strip};
pub use axis::{TabletPadAxisSource, TabletPadAxisUserData};

use wayland_server::backend::protocol::ProtocolError;

use std::{
    fmt,
    sync::{Arc, Mutex, Weak as ArcWeak},
};

use wayland_protocols::wp::tablet::zv2::server::{
    zwp_tablet_pad_group_v2::ZwpTabletPadGroupV2 as Group,
    zwp_tablet_pad_v2::{self as pad, ZwpTabletPadV2 as Pad},
    zwp_tablet_seat_v2::ZwpTabletSeatV2,
};
use wayland_server::{
    Client, Dispatch, DisplayHandle, Resource, Weak,
    backend::{ClientId, ObjectId},
    protocol::wl_surface::WlSurface,
};

use crate::{
    backend::input::{ButtonState, InputTime},
    input::tablet::{Tablet, TabletDescriptor, TabletSeat, TabletSeatHandler},
    utils::{SERIAL_COUNTER, Serial},
    wayland::Dispatch2,
};

/// Static button membership and available modes of one pad group.
#[derive(Debug, Clone)]
pub struct TabletPadGroupDescriptor {
    /// Device-local button indices belonging to this group.
    pub buttons: Vec<u32>,
    /// Device-local ring indices belonging to this group.
    pub rings: Vec<u32>,
    /// Device-local strip indices belonging to this group.
    pub strips: Vec<u32>,
    /// Number of modes, including the initial mode zero.
    pub modes: u32,
}

/// Implemented indexed controls of one tablet pad.
#[derive(Debug, Clone)]
pub struct TabletPadDescriptor {
    /// Total number of indexed buttons.
    pub buttons: u32,
    /// Total number of touch rings.
    pub rings: u32,
    /// Total number of touch strips.
    pub strips: u32,
    /// At least one mode group, with disjoint control memberships.
    pub groups: Vec<TabletPadGroupDescriptor>,
}

/// A pad description has no groups, zero modes, or invalid control membership.
#[derive(Debug, Clone, Copy)]
pub struct InvalidTabletPadDescriptor;
impl fmt::Display for InvalidTabletPadDescriptor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("pad groups require nonzero modes, disjoint controls, and complete ring/strip membership")
    }
}
impl std::error::Error for InvalidTabletPadDescriptor {}

/// Protocol state for one physical tablet pad. The compositor owns focus and
/// paired button routing, including releases before focus is revoked.
#[derive(Debug, Clone)]
pub struct TabletPadHandle(Arc<Mutex<PadState>>);

#[derive(Debug)]
struct PadState {
    identity: TabletDescriptor,
    descriptor: TabletPadDescriptor,
    modes: Vec<u32>,
    focus: Option<(WlSurface, Tablet)>,
    instances: Vec<Instance>,
}

#[derive(Debug)]
struct Instance {
    pad: Weak<Pad>,
    groups: Vec<Weak<Group>>,
    rings: Vec<Weak<Ring>>,
    strips: Vec<Weak<Strip>>,
    seat: ObjectId,
    serials: Vec<Option<Serial>>,
    focused: bool,
}

/// User data of a tablet-v2 pad resource.
#[derive(Debug)]
pub struct TabletPadUserData(ArcWeak<Mutex<PadState>>);

/// User data of a tablet-v2 pad mode group resource.
#[derive(Debug)]
pub struct TabletPadGroupUserData {
    handle: ArcWeak<Mutex<PadState>>,
    pad: ObjectId,
}

impl<D: TabletSeatHandler + 'static> TabletSeat<D> {
    /// Publish a physical pad's implemented buttons and mode groups to existing
    /// and future tablet-seat bindings. Its identity must be unique on the seat.
    pub fn add_wp_pad(
        &self,
        dh: &DisplayHandle,
        identity: &TabletDescriptor,
        descriptor: TabletPadDescriptor,
    ) -> Result<TabletPadHandle, InvalidTabletPadDescriptor>
    where
        D: Dispatch<Pad, TabletPadUserData>
            + Dispatch<Group, TabletPadGroupUserData>
            + Dispatch<Ring, TabletPadAxisUserData>
            + Dispatch<Strip, TabletPadAxisUserData>,
    {
        if descriptor.groups.is_empty() {
            return Err(InvalidTabletPadDescriptor);
        }
        let mut seen = std::collections::HashSet::new();
        let mut rings = std::collections::HashSet::new();
        let mut strips = std::collections::HashSet::new();
        for group in &descriptor.groups {
            if group.modes == 0
                || group
                    .buttons
                    .iter()
                    .any(|button| *button >= descriptor.buttons || !seen.insert(*button))
                || group
                    .rings
                    .iter()
                    .any(|ring| *ring >= descriptor.rings || !rings.insert(*ring))
                || group
                    .strips
                    .iter()
                    .any(|strip| *strip >= descriptor.strips || !strips.insert(*strip))
            {
                return Err(InvalidTabletPadDescriptor);
            }
        }
        if rings.len() != descriptor.rings as usize || strips.len() != descriptor.strips as usize {
            return Err(InvalidTabletPadDescriptor);
        }
        let handle = TabletPadHandle(Arc::new(Mutex::new(PadState {
            modes: vec![0; descriptor.groups.len()],
            identity: identity.clone(),
            descriptor,
            focus: None,
            instances: Vec::new(),
        })));
        let mut inner = self.arc.lock().unwrap();
        assert!(
            !inner.pads.contains_key(identity),
            "a physical pad is published once"
        );
        for seat in &inner.instances {
            if let Ok(seat) = seat.upgrade()
                && let Some(client) = seat.client()
            {
                handle.new_instance::<D>(&client, dh, &seat);
            }
        }
        inner.pads.insert(identity.clone(), handle.clone());
        Ok(handle)
    }

    /// Retire one pad. Drop all compositor handles after clearing its focus so
    /// its clients receive the removed event.
    pub fn remove_wp_pad(&self, identity: &TabletDescriptor) {
        self.arc.lock().unwrap().pads.remove(identity);
    }
}

impl TabletPadHandle {
    pub(super) fn new_instance<D>(&self, client: &Client, dh: &DisplayHandle, seat: &ZwpTabletSeatV2)
    where
        D: Dispatch<Pad, TabletPadUserData>
            + Dispatch<Group, TabletPadGroupUserData>
            + Dispatch<Ring, TabletPadAxisUserData>
            + Dispatch<Strip, TabletPadAxisUserData>
            + 'static,
    {
        let mut state = self.0.lock().unwrap();
        let Ok(pad) = client.create_resource::<Pad, _, D>(
            dh,
            seat.version(),
            TabletPadUserData(Arc::downgrade(&self.0)),
        ) else {
            no_memory(client, dh);
            return;
        };
        seat.pad_added(&pad);
        if let Some(path) = state.identity.syspath.as_ref().and_then(|path| path.to_str()) {
            pad.path(path.to_owned());
        }
        if state.descriptor.buttons > 0 {
            pad.buttons(state.descriptor.buttons);
        }
        let mut groups = Vec::new();
        let mut rings = vec![None; state.descriptor.rings as usize];
        let mut strips = vec![None; state.descriptor.strips as usize];
        for group in &state.descriptor.groups {
            let Ok(resource) = client.create_resource::<Group, _, D>(
                dh,
                seat.version(),
                TabletPadGroupUserData {
                    handle: Arc::downgrade(&self.0),
                    pad: pad.id(),
                },
            ) else {
                no_memory(client, dh);
                return;
            };
            pad.group(&resource);
            resource.buttons(
                group
                    .buttons
                    .iter()
                    .flat_map(|button| button.to_ne_bytes())
                    .collect(),
            );
            for index in &group.rings {
                let Ok(ring) = client.create_resource::<Ring, _, D>(
                    dh,
                    seat.version(),
                    TabletPadAxisUserData {
                        handle: Arc::downgrade(&self.0),
                        pad: pad.id(),
                        control: TabletPadControl::Ring(*index),
                    },
                ) else {
                    no_memory(client, dh);
                    return;
                };
                resource.ring(&ring);
                rings[*index as usize] = Some(ring.downgrade());
            }
            for index in &group.strips {
                let Ok(strip) = client.create_resource::<Strip, _, D>(
                    dh,
                    seat.version(),
                    TabletPadAxisUserData {
                        handle: Arc::downgrade(&self.0),
                        pad: pad.id(),
                        control: TabletPadControl::Strip(*index),
                    },
                ) else {
                    no_memory(client, dh);
                    return;
                };
                resource.strip(&strip);
                strips[*index as usize] = Some(strip.downgrade());
            }
            if group.modes > 1 {
                resource.modes(group.modes);
            }
            resource.done();
            groups.push(resource.downgrade());
        }
        pad.done();
        let mut instance = Instance {
            pad: pad.downgrade(),
            focused: false,
            seat: seat.id(),
            serials: vec![None; groups.len()],
            groups,
            rings: rings.into_iter().map(Option::unwrap).collect(),
            strips: strips.into_iter().map(Option::unwrap).collect(),
        };
        if let Some((surface, tablet)) = &state.focus {
            enter(&mut instance, surface, tablet, &state.modes, InputTime::now());
        }
        state.instances.push(instance);
    }

    /// Update the eligible surface and associated tablet. This does not move the
    /// keyboard or pointer focus. Passing None sends leave and revokes feedback.
    pub fn focus(&self, target: Option<(WlSurface, Tablet)>, time: InputTime) {
        let mut state = self.0.lock().unwrap();
        if state.focus == target {
            return;
        }
        if let Some((surface, _)) = state.focus.take() {
            for instance in &mut state.instances {
                if instance.focused {
                    if let Ok(pad) = instance.pad.upgrade() {
                        pad.leave(SERIAL_COUNTER.next_serial().into(), &surface);
                    }
                }
                instance.focused = false;
                instance.serials.fill(None);
            }
        }
        state.focus = target;
        let PadState {
            focus,
            modes,
            instances,
            ..
        } = &mut *state;
        if let Some((surface, tablet)) = focus {
            for instance in instances {
                enter(instance, surface, tablet, modes, time);
            }
        }
    }

    /// Announce an actual device mode change. Call before the button event which
    /// caused it; unchanged modes are not announced again.
    pub fn mode(&self, group: usize, mode: u32, time: InputTime) {
        let mut state = self.0.lock().unwrap();
        assert!(mode < state.descriptor.groups[group].modes);
        if state.modes[group] == mode {
            return;
        }
        state.modes[group] = mode;
        let PadState { focus, instances, .. } = &mut *state;
        if focus.is_some() {
            for instance in instances {
                if instance.focused {
                    send_mode(instance, group, mode, time);
                }
            }
        }
    }

    /// Send one indexed button transition to the current pad owner.
    pub fn button(&self, button: u32, pressed: ButtonState, time: InputTime) {
        let state = self.0.lock().unwrap();
        assert!(button < state.descriptor.buttons);
        if state.focus.is_some() {
            for instance in &state.instances {
                if instance.focused
                    && let Ok(pad) = instance.pad.upgrade()
                {
                    let pressed = match pressed {
                        ButtonState::Pressed => pad::ButtonState::Pressed,
                        ButtonState::Released => pad::ButtonState::Released,
                    };
                    pad.button(time.millis(), button, pressed);
                }
            }
        }
    }
}

fn enter(instance: &mut Instance, surface: &WlSurface, tablet: &Tablet, modes: &[u32], time: InputTime) {
    if !instance.pad.id().same_client_as(&surface.id()) {
        return;
    }
    let Some(tablet) = tablet
        .arc
        .wp_tablet
        .focused_tablet_for_seat(surface, &instance.seat)
    else {
        return;
    };
    instance.focused = true;
    if let Ok(pad) = instance.pad.upgrade() {
        pad.enter(SERIAL_COUNTER.next_serial().into(), &tablet, surface);
    }
    for (group, mode) in modes.iter().enumerate() {
        send_mode(instance, group, *mode, time);
    }
}

fn send_mode(instance: &mut Instance, group: usize, mode: u32, time: InputTime) {
    instance.serials[group] = None;
    if let Ok(resource) = instance.groups[group].upgrade() {
        let serial = SERIAL_COUNTER.next_serial();
        resource.mode_switch(time.millis(), serial.into(), mode);
        instance.serials[group] = Some(serial);
    }
}

fn no_memory(client: &Client, dh: &DisplayHandle) {
    client.kill(
        dh,
        ProtocolError {
            code: 2, // wl_display.error.no_memory; wl_display is backend-managed.
            object_id: 1,
            object_interface: "wl_display".into(),
            message: "cannot allocate tablet pad resources".into(),
        },
    );
}

impl<D: TabletSeatHandler + 'static> Dispatch2<Pad, D> for TabletPadUserData {
    fn request(
        &self,
        state: &mut D,
        _client: &Client,
        resource: &Pad,
        request: pad::Request,
        _dh: &DisplayHandle,
        _init: &mut wayland_server::DataInit<'_, D>,
    ) {
        if let pad::Request::SetFeedback {
            button,
            description,
            serial,
        } = request
        {
            let Some(handle) = self.0.upgrade() else {
                return;
            };
            let identity = {
                let Some(identity) =
                    handle
                        .lock()
                        .unwrap()
                        .feedback(&resource.id(), TabletPadControl::Button(button), serial)
                else {
                    return;
                };
                identity
            };
            state.tablet_pad_feedback(&identity, TabletPadControl::Button(button), &description);
        }
    }

    fn destroyed(&self, _state: &mut D, _client: ClientId, resource: &Pad) {
        if let Some(handle) = self.0.upgrade() {
            handle.lock().unwrap().instances.retain(|instance| {
                instance.pad.id() != resource.id() || instance.alive_except(&resource.id())
            });
        }
    }
}

impl<D> Dispatch2<Group, D> for TabletPadGroupUserData {
    fn request(
        &self,
        _state: &mut D,
        _client: &Client,
        _resource: &Group,
        _request: <Group as Resource>::Request,
        _dh: &DisplayHandle,
        _init: &mut wayland_server::DataInit<'_, D>,
    ) {
    }

    fn destroyed(&self, _state: &mut D, _client: ClientId, resource: &Group) {
        if let Some(handle) = self.handle.upgrade() {
            handle
                .lock()
                .unwrap()
                .instances
                .retain(|instance| instance.pad.id() != self.pad || instance.alive_except(&resource.id()));
        }
    }
}

impl Drop for PadState {
    fn drop(&mut self) {
        for instance in &self.instances {
            if let Ok(pad) = instance.pad.upgrade() {
                pad.removed();
            }
        }
    }
}
