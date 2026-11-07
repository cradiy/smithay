use x11rb::{
    connection::Connection,
    protocol::xproto::{AtomEnum, ClientMessageEvent, ConnectionExt, EventMask, InputFocus, PropMode},
    rust_connection::ConnectionError,
    wrapper::ConnectionExt as _,
};

use super::{X11Surface, X11Wm, surface::WmInputModel};

#[derive(Debug, Default)]
pub(super) enum KeyboardFocus {
    #[default]
    Unmanaged,
    Idle,
    Clock {
        window: X11Surface,
        sequence: u16,
    },
    Waiting(X11Surface),
    Active,
}

impl X11Wm {
    /// Selects the compositor-owned X11 keyboard target. Completion is reported
    /// through `XwmHandler::keyboard_focus_ready`; no X11 round trip blocks here.
    /// Call with `None` when the compositor selects a native surface or no target.
    pub fn set_keyboard_focus(&mut self, window: Option<&X11Surface>) -> Result<(), ConnectionError> {
        if let Some(window) = window {
            assert_eq!(window.xwm_id(), Some(self.id));
            assert_ne!(window.input_model(), WmInputModel::None);
        }
        self.keyboard_focus = KeyboardFocus::Idle;
        self.conn
            .set_input_focus(InputFocus::NONE, x11rb::NONE, x11rb::CURRENT_TIME)?;
        if self.net_supported.contains(&self.atoms._NET_ACTIVE_WINDOW) {
            self.conn.change_property32(
                PropMode::REPLACE,
                self.screen.root,
                self.atoms._NET_ACTIVE_WINDOW,
                AtomEnum::WINDOW,
                &[window.map_or(x11rb::NONE, X11Surface::window_id)],
            )?;
        }
        if let Some(window) = window {
            // PropertyNotify carries server time and the request sequence. A
            // superseding request cannot consume an earlier timestamp reply.
            let sequence = self
                .conn
                .change_property32(
                    PropMode::REPLACE,
                    *self.wm_window,
                    self.atoms._SMITHAY_FOCUS_TIMESTAMP,
                    AtomEnum::CARDINAL,
                    &[0],
                )?
                .sequence_number() as u16;
            self.keyboard_focus = KeyboardFocus::Clock {
                window: window.clone(),
                sequence,
            };
        }
        self.conn.flush()
    }

    pub(super) fn keyboard_timestamp(&mut self, sequence: u16, time: u32) -> Result<(), ConnectionError> {
        let KeyboardFocus::Clock {
            window,
            sequence: expected,
        } = &self.keyboard_focus
        else {
            return Ok(());
        };
        if sequence != *expected || !window.alive() || !window.is_mapped() {
            return Ok(());
        }
        let window = window.clone();
        if time == x11rb::CURRENT_TIME {
            return self.set_keyboard_focus(Some(&window));
        }
        match window.input_model() {
            WmInputModel::Passive | WmInputModel::LocallyActive => {
                self.conn
                    .set_input_focus(InputFocus::NONE, window.window_id(), time)?;
            }
            WmInputModel::GloballyActive => (),
            WmInputModel::None => return Ok(()),
        }
        if matches!(
            window.input_model(),
            WmInputModel::LocallyActive | WmInputModel::GloballyActive
        ) {
            self.conn.send_event(
                false,
                window.window_id(),
                EventMask::NO_EVENT,
                ClientMessageEvent::new(
                    32,
                    window.window_id(),
                    self.atoms.WM_PROTOCOLS,
                    [self.atoms.WM_TAKE_FOCUS, time, 0, 0, 0],
                ),
            )?;
        }
        self.keyboard_focus = KeyboardFocus::Waiting(window);
        self.conn.flush()
    }

    pub(super) fn keyboard_entered(&mut self, xid: u32) -> Option<X11Surface> {
        let KeyboardFocus::Waiting(window) = &self.keyboard_focus else {
            return None;
        };
        if window.window_id() != xid || !window.alive() || !window.is_mapped() {
            return None;
        }
        let window = window.clone();
        self.keyboard_focus = KeyboardFocus::Active;
        Some(window)
    }
}
