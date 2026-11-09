//! Move native toplevels together with a Wayland data drag.
//!
//! The compositor owns placement and input grabs. Use [`ToplevelDrag::from_source`]
//! when admitting a data drag, observe its attachment, and exclude that toplevel
//! from destination hit testing. Call [`XdgToplevelDragManager::toplevel_unmapped`]
//! on mapped-to-unmapped transitions, including while no drag grab is active.
//! Source completion and cancellation retire attachments automatically.

use std::sync::{Arc, Mutex, Weak};

use wayland_protocols::xdg::toplevel_drag::v1::server::{
    xdg_toplevel_drag_manager_v1::{self, XdgToplevelDragManagerV1},
    xdg_toplevel_drag_v1::{self, XdgToplevelDragV1},
};
use wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
    backend::{ClientId, GlobalId},
    protocol::wl_data_source::WlDataSource,
};

use crate::{
    utils::{Logical, Point},
    wayland::{
        Dispatch2, GlobalData, GlobalDispatch2,
        selection::data_device::{DataDeviceHandler, DataSourceUserData},
        shell::xdg::{ToplevelSurface, XdgShellHandler},
    },
};

/// One attachment of a toplevel to a drag. Reattaching after unmapping changes
/// the sequence even when the same role is reused.
#[derive(Debug, Clone)]
pub struct ToplevelDragAttachment {
    /// Native window to move and omit from destination hit testing.
    pub toplevel: ToplevelSurface,
    /// Hint relative to the window geometry and the input hotspot.
    pub offset: Point<i32, Logical>,
    /// Identifies this attachment within its data source.
    pub sequence: u64,
}

#[derive(Debug, Default)]
pub(crate) struct DragState {
    ended: bool,
    attachment: Option<ToplevelDragAttachment>,
    sequence: u64,
}

/// Association between a data source and its toplevel-drag role.
#[derive(Debug, Clone)]
pub struct ToplevelDrag {
    source: WlDataSource,
    state: Arc<Mutex<DragState>>,
}

impl ToplevelDrag {
    /// Find the role created for this source, including after its drag has ended.
    pub fn from_source(source: &WlDataSource) -> Option<Self> {
        let state = source
            .data::<DataSourceUserData>()?
            .toplevel_drag
            .lock()
            .unwrap()
            .clone()?;
        Some(Self {
            source: source.clone(),
            state,
        })
    }

    /// The data source that owns this drag.
    pub fn source(&self) -> &WlDataSource {
        &self.source
    }

    /// Current live attachment, or `None` after detachment or source completion.
    pub fn attachment(&self) -> Option<ToplevelDragAttachment> {
        let state = self.state.lock().unwrap();
        (!state.ended)
            .then(|| state.attachment.clone())
            .flatten()
            .filter(|attachment| attachment.toplevel.alive())
    }
}

pub(crate) fn source_ended(source: &WlDataSource) {
    if let Some(drag) = ToplevelDrag::from_source(source) {
        let mut state = drag.state.lock().unwrap();
        state.ended = true;
        state.attachment = None;
    }
}

/// Compositor integration for the standard toplevel drag protocol.
pub trait XdgToplevelDragHandler: DataDeviceHandler + XdgShellHandler {
    /// Manager tracking the registered toplevel-drag roles.
    fn xdg_toplevel_drag_state(&mut self) -> &mut XdgToplevelDragManager;

    /// A client attached a toplevel. It may still be unmapped, or its source may
    /// not yet have started a drag. Placement requires an admitted data drag.
    fn toplevel_drag_attached(&mut self, drag: ToplevelDrag) {
        let _ = drag;
    }
}

/// Protocol global and weak attachment index; data sources own the role state.
#[derive(Debug)]
pub struct XdgToplevelDragManager {
    global: GlobalId,
    drags: Vec<Weak<Mutex<DragState>>>,
}

impl XdgToplevelDragManager {
    /// Advertise version 1. The compositor must implement placement, hit testing,
    /// unmapping notifications and source-side drop completion before calling this.
    pub fn new<D>(display: &DisplayHandle) -> Self
    where
        D: XdgToplevelDragHandler + GlobalDispatch<XdgToplevelDragManagerV1, GlobalData>,
    {
        Self {
            global: display.create_global::<D, XdgToplevelDragManagerV1, _>(1, GlobalData),
            drags: Vec::new(),
        }
    }

    /// Registered global identifier.
    pub fn global(&self) -> GlobalId {
        self.global.clone()
    }

    /// Detach only on a mapped-to-unmapped transition, not an initial empty commit.
    pub fn toplevel_unmapped(&mut self, toplevel: &ToplevelSurface) {
        self.drags.retain(|drag| {
            let Some(drag) = drag.upgrade() else {
                return false;
            };
            let mut drag = drag.lock().unwrap();
            if drag.attachment.as_ref().is_some_and(|a| a.toplevel == *toplevel) {
                drag.attachment = None;
            }
            true
        });
    }
}

impl<D: XdgToplevelDragHandler> GlobalDispatch2<XdgToplevelDragManagerV1, D> for GlobalData
where
    D: Dispatch<XdgToplevelDragManagerV1, GlobalData>,
{
    fn bind(
        &self,
        _: &mut D,
        _: &DisplayHandle,
        _: &Client,
        resource: New<XdgToplevelDragManagerV1>,
        data_init: &mut DataInit<'_, D>,
    ) {
        data_init.init(resource, GlobalData);
    }
}

impl<D: XdgToplevelDragHandler> Dispatch2<XdgToplevelDragManagerV1, D> for GlobalData
where
    D: Dispatch<XdgToplevelDragV1, ToplevelDrag>,
{
    fn request(
        &self,
        state: &mut D,
        _: &Client,
        resource: &XdgToplevelDragManagerV1,
        request: xdg_toplevel_drag_manager_v1::Request,
        _: &DisplayHandle,
        data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            xdg_toplevel_drag_manager_v1::Request::GetXdgToplevelDrag { id, data_source } => {
                let inner = Arc::new(Mutex::new(DragState::default()));
                data_init.init(
                    id,
                    ToplevelDrag {
                        source: data_source.clone(),
                        state: inner.clone(),
                    },
                );
                let data = data_source.data::<DataSourceUserData>().unwrap();
                let mut role = data.toplevel_drag.lock().unwrap();
                if role.is_some() || state.data_device_state().used_sources.contains_key(&data_source) {
                    resource.post_error(
                        xdg_toplevel_drag_manager_v1::Error::InvalidSource,
                        "Toplevel drag requires a fresh data source.",
                    );
                    return;
                }
                *role = Some(inner.clone());
                let manager = state.xdg_toplevel_drag_state();
                manager.drags.retain(|drag| drag.strong_count() != 0);
                manager.drags.push(Arc::downgrade(&inner));
            }
            xdg_toplevel_drag_manager_v1::Request::Destroy => {}
            _ => unreachable!(),
        }
    }
}

impl<D: XdgToplevelDragHandler> Dispatch2<XdgToplevelDragV1, D> for ToplevelDrag {
    fn request(
        &self,
        state: &mut D,
        _: &Client,
        resource: &XdgToplevelDragV1,
        request: xdg_toplevel_drag_v1::Request,
        _: &DisplayHandle,
        _: &mut DataInit<'_, D>,
    ) {
        match request {
            xdg_toplevel_drag_v1::Request::Attach {
                toplevel,
                x_offset,
                y_offset,
            } => {
                let mut inner = self.state.lock().unwrap();
                if inner.attachment.as_ref().is_some_and(|a| a.toplevel.alive()) {
                    resource.post_error(
                        xdg_toplevel_drag_v1::Error::ToplevelAttached,
                        "A live toplevel is already attached.",
                    );
                    return;
                }
                if let Some(toplevel) = state.xdg_shell_state().get_toplevel(&toplevel) {
                    inner.sequence += 1;
                    inner.attachment = Some(ToplevelDragAttachment {
                        toplevel,
                        offset: (x_offset, y_offset).into(),
                        sequence: inner.sequence,
                    });
                }
                drop(inner);
                state.toplevel_drag_attached(self.clone());
            }
            xdg_toplevel_drag_v1::Request::Destroy => {
                if !self.state.lock().unwrap().ended {
                    resource.post_error(
                        xdg_toplevel_drag_v1::Error::OngoingDrag,
                        "The underlying drag has not ended.",
                    );
                }
            }
            _ => unreachable!(),
        }
    }

    fn destroyed(&self, _: &mut D, _: ClientId, _: &XdgToplevelDragV1) {
        self.state.lock().unwrap().attachment = None;
    }
}
