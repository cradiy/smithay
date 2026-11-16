//! Ring, strip and dial resources belonging to a physical pad.
use super::*;
pub(super) use wayland_protocols::wp::tablet::zv2::server::{
    zwp_tablet_pad_dial_v2::{self as dial, ZwpTabletPadDialV2 as Dial},
    zwp_tablet_pad_ring_v2::{self as ring, ZwpTabletPadRingV2 as Ring},
    zwp_tablet_pad_strip_v2::{self as strip, ZwpTabletPadStripV2 as Strip},
};

/// Physical origin of a ring or strip update.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabletPadAxisSource {
    /// A finger interaction, terminated by a stop frame.
    Finger,
    /// No source information is available.
    Unknown,
}

/// User data of one ring, strip or dial, independent of its parent resource lifetime.
#[derive(Debug)]
pub struct TabletPadAxisUserData {
    pub(super) handle: ArcWeak<Mutex<PadState>>,
    pub(super) pad: ObjectId,
    pub(super) control: TabletPadControl,
}

impl Instance {
    pub(super) fn alive_except(&self, excluded: &ObjectId) -> bool {
        (self.pad.id() != *excluded && self.pad.upgrade().is_ok())
            || self
                .groups
                .iter()
                .any(|r| r.id() != *excluded && r.upgrade().is_ok())
            || self
                .rings
                .iter()
                .any(|r| r.id() != *excluded && r.upgrade().is_ok())
            || self
                .dials
                .iter()
                .flatten()
                .any(|r| r.id() != *excluded && r.upgrade().is_ok())
            || self
                .strips
                .iter()
                .any(|r| r.id() != *excluded && r.upgrade().is_ok())
    }
}

impl PadState {
    pub(super) fn feedback(
        &self,
        pad: &ObjectId,
        control: TabletPadControl,
        serial: u32,
    ) -> Option<TabletDescriptor> {
        let (surface, _) = self.focus.as_ref()?;
        if !pad.same_client_as(&surface.id()) {
            return None;
        }
        let group = self.descriptor.groups.iter().position(|group| match control {
            TabletPadControl::Button(i) => group.buttons.contains(&i),
            TabletPadControl::Ring(i) => group.rings.contains(&i),
            TabletPadControl::Strip(i) => group.strips.contains(&i),
            TabletPadControl::Dial(i) => group.dials.contains(&i),
        })?;
        self.instances
            .iter()
            .any(|instance| {
                instance.focused
                    && instance.pad.id() == *pad
                    && instance.serials[group] == Some(Serial::from(serial))
            })
            .then(|| self.identity.clone())
    }
}

impl TabletPadHandle {
    /// Send a complete ring frame. A missing angle terminates the interaction.
    /// Angles are clockwise degrees in [0, 360), in the pad's logical rotation.
    pub fn ring(&self, index: u32, angle: Option<f64>, source: TabletPadAxisSource, time: InputTime) {
        assert!(angle.is_none_or(|angle| angle.is_finite() && (0.0..360.0).contains(&angle)));
        let state = self.0.lock().unwrap();
        assert!(index < state.descriptor.rings);
        for instance in &state.instances {
            if instance.focused
                && let Ok(ring) = instance.rings[index as usize].upgrade()
            {
                if source == TabletPadAxisSource::Finger {
                    ring.source(ring::Source::Finger);
                }
                if let Some(angle) = angle {
                    ring.angle(angle);
                } else {
                    ring.stop();
                }
                ring.frame(time.millis());
            }
        }
    }

    /// Send a complete strip frame. Positions use the protocol's [0, 65535]
    /// range; a missing position terminates the interaction.
    pub fn strip(&self, index: u32, position: Option<u32>, source: TabletPadAxisSource, time: InputTime) {
        assert!(position.is_none_or(|position| position <= 65535));
        let state = self.0.lock().unwrap();
        assert!(index < state.descriptor.strips);
        for instance in &state.instances {
            if instance.focused
                && let Ok(strip) = instance.strips[index as usize].upgrade()
            {
                if source == TabletPadAxisSource::Finger {
                    strip.source(strip::Source::Finger);
                }
                if let Some(position) = position {
                    strip.position(position);
                } else {
                    strip.stop();
                }
                strip.frame(time.millis());
            }
        }
    }
}

impl TabletPadAxisUserData {
    fn feedback<D: TabletSeatHandler>(&self, state: &mut D, description: &str, serial: u32) {
        let Some(handle) = self.handle.upgrade() else {
            return;
        };
        let Some(identity) = handle.lock().unwrap().feedback(&self.pad, self.control, serial) else {
            return;
        };
        state.tablet_pad_feedback(&identity, self.control, description);
    }
    fn destroyed(&self, resource: &ObjectId) {
        if let Some(handle) = self.handle.upgrade() {
            handle
                .lock()
                .unwrap()
                .instances
                .retain(|instance| instance.pad.id() != self.pad || instance.alive_except(resource));
        }
    }
}

impl<D: TabletSeatHandler + 'static> Dispatch2<Ring, D> for TabletPadAxisUserData {
    fn request(
        &self,
        state: &mut D,
        _: &Client,
        _: &Ring,
        request: ring::Request,
        _: &DisplayHandle,
        _: &mut wayland_server::DataInit<'_, D>,
    ) {
        if let ring::Request::SetFeedback { description, serial } = request {
            self.feedback(state, &description, serial);
        }
    }
    fn destroyed(&self, _: &mut D, _: ClientId, resource: &Ring) {
        self.destroyed(&resource.id());
    }
}
impl<D: TabletSeatHandler + 'static> Dispatch2<Strip, D> for TabletPadAxisUserData {
    fn request(
        &self,
        state: &mut D,
        _: &Client,
        _: &Strip,
        request: strip::Request,
        _: &DisplayHandle,
        _: &mut wayland_server::DataInit<'_, D>,
    ) {
        if let strip::Request::SetFeedback { description, serial } = request {
            self.feedback(state, &description, serial);
        }
    }
    fn destroyed(&self, _: &mut D, _: ClientId, resource: &Strip) {
        self.destroyed(&resource.id());
    }
}

impl TabletPadHandle {
    /// Send a complete dial frame, with a nonzero signed delta in units of 1/120
    /// of one detent. Dial resources exist only for version 2 bindings.
    pub fn dial(&self, index: u32, value120: i32, time: InputTime) {
        assert_ne!(value120, 0);
        let state = self.0.lock().unwrap();
        assert!(index < state.descriptor.dials);
        for instance in &state.instances {
            if instance.focused
                && let Some(dial) = &instance.dials[index as usize]
                && let Ok(dial) = dial.upgrade()
            {
                dial.delta(value120);
                dial.frame(time.millis());
            }
        }
    }
}
impl<D: TabletSeatHandler + 'static> Dispatch2<Dial, D> for TabletPadAxisUserData {
    fn request(
        &self,
        state: &mut D,
        _: &Client,
        _: &Dial,
        request: dial::Request,
        _: &DisplayHandle,
        _: &mut wayland_server::DataInit<'_, D>,
    ) {
        if let dial::Request::SetFeedback { description, serial } = request {
            self.feedback(state, &description, serial);
        }
    }
    fn destroyed(&self, _: &mut D, _: ClientId, resource: &Dial) {
        self.destroyed(&resource.id());
    }
}
