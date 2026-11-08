//! Protocol for confining the pointer.
//!
//! This provides a way for the client to request that the pointer is confined to a region or
//! locked in place.
use std::{
    collections::{HashMap, hash_map},
    ops,
    sync::{
        Mutex, Weak,
        atomic::{AtomicBool, Ordering},
    },
};

use wayland_protocols::wp::pointer_constraints::zv1::server::{
    zwp_confined_pointer_v1::{self, ZwpConfinedPointerV1},
    zwp_locked_pointer_v1::{self, ZwpLockedPointerV1},
    zwp_pointer_constraints_v1::{self, Lifetime, ZwpPointerConstraintsV1},
};
use wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource, WEnum,
    backend::{GlobalId, ObjectId},
    protocol::wl_surface::WlSurface,
};

use super::compositor::{self, Cacheable, RegionAttributes};
use crate::{
    input::{
        SeatHandler,
        pointer::{PointerHandle, PointerRc},
    },
    utils::{Logical, Point},
    wayland::{Dispatch2, GlobalData, GlobalDispatch2, seat::PointerUserData},
};

const VERSION: u32 = 1;

/// Reason for Constraint remove
#[derive(Debug)]
pub enum ConstraintRemove {
    /// Client call destroy
    Destroyed(PointerConstraint),
    /// Compositor pointer leave surface
    PointerLeave(Option<RegionAttributes>),
}

/// Handler for pointer constraints
pub trait PointerConstraintsHandler: SeatHandler {
    /// Pointer lock or confinement constraint created for `pointer` on `surface`
    ///
    /// Use [`with_pointer_constraint`] to access the constraint.
    fn new_constraint(&mut self, _surface: &WlSurface, _pointer: &PointerHandle<Self>) {}

    /// Pointer constraint removed for `pointer` on `surface`
    ///
    /// Use [`with_pointer_constraint`] to access the constraint.
    fn remove_constraint(
        &mut self,
        _surface: &WlSurface,
        _pointer: &PointerHandle<Self>,
        _constraint_remove: ConstraintRemove,
    ) {
    }

    /// The client holding a LockedPointer has committed a cursor position hint.
    ///
    /// This is emitted upon a surface commit if the cursor position hint has been updated.
    ///
    /// Use [`with_pointer_constraint`] to access the constraint and check if it is active.
    fn cursor_position_hint(
        &mut self,
        _surface: &WlSurface,
        _pointer: &PointerHandle<Self>,
        _location: Point<f64, Logical>,
    ) {
    }
}

/// Constraint confining pointer to a region of the surface
#[derive(Debug)]
pub struct ConfinedPointer {
    handle: zwp_confined_pointer_v1::ZwpConfinedPointerV1,
    region: Option<RegionAttributes>,
    lifetime: WEnum<Lifetime>,
    active: AtomicBool,
}

impl ConfinedPointer {
    /// Region in which to confine the pointer
    pub fn region(&self) -> Option<&RegionAttributes> {
        self.region.as_ref()
    }
}

/// Constraint locking pointer in place
#[derive(Debug)]
pub struct LockedPointer {
    handle: zwp_locked_pointer_v1::ZwpLockedPointerV1,
    region: Option<RegionAttributes>,
    lifetime: WEnum<Lifetime>,
    cursor_position_hint: Option<Point<f64, Logical>>,
    active: AtomicBool,
}

impl LockedPointer {
    /// Region in which to activate the lock
    pub fn region(&self) -> Option<&RegionAttributes> {
        self.region.as_ref()
    }

    /// Position the client is rendering a cursor, if any
    pub fn cursor_position_hint(&self) -> Option<Point<f64, Logical>> {
        self.cursor_position_hint
    }
}

/// A constraint imposed on the pointer instance
#[derive(Debug)]
pub enum PointerConstraint {
    /// Pointer is confined to a region of the surface
    Confined(ConfinedPointer),
    /// Pointer is locked in place
    Locked(LockedPointer),
}

/// A reference to a pointer constraint that can be activated or deactivated.
///
/// The derefs to `[PointerConstraint]`.
#[derive(Debug)]
pub struct PointerConstraintRef<'a, D: SeatHandler + 'static> {
    entry: hash_map::OccupiedEntry<'a, PointerHandle<D>, PointerConstraint>,
}

impl<D: SeatHandler + 'static> ops::Deref for PointerConstraintRef<'_, D> {
    type Target = PointerConstraint;

    fn deref(&self) -> &Self::Target {
        self.entry.get()
    }
}

impl<D: SeatHandler + PointerConstraintsHandler + 'static> PointerConstraintRef<'_, D> {
    /// Send `locked`/`unlocked`
    ///
    /// This is not sent automatically since compositors may have different
    /// policies about when to allow and activate constraints.
    pub fn activate(&self) {
        match self.entry.get() {
            PointerConstraint::Confined(confined) => {
                if !confined.active.swap(true, Ordering::SeqCst) {
                    confined.handle.confined();
                }
            }
            PointerConstraint::Locked(locked) => {
                if !locked.active.swap(true, Ordering::SeqCst) {
                    locked.handle.locked();
                }
            }
        }
    }

    /// Send `unlocked`/`unconfined`
    ///
    /// For oneshot constraints, will destroy the constraint.
    ///
    /// This is sent automatically when the surface loses pointer focus, but
    /// may also be invoked while the surface is focused.
    pub fn deactivate(self) {
        let deactivated = match self.entry.get() {
            PointerConstraint::Confined(confined) => {
                if confined.active.swap(false, Ordering::SeqCst) {
                    confined.handle.unconfined();
                    true
                } else {
                    false
                }
            }
            PointerConstraint::Locked(locked) => {
                if locked.active.swap(false, Ordering::SeqCst) {
                    locked.handle.unlocked();
                    true
                } else {
                    false
                }
            }
        };

        if deactivated && self.lifetime() == WEnum::Value(Lifetime::Oneshot) {
            self.entry.remove_entry();
        }
    }
}

impl PointerConstraint {
    /// Constraint is active
    pub fn is_active(&self) -> bool {
        match self {
            PointerConstraint::Confined(confined) => &confined.active,
            PointerConstraint::Locked(locked) => &locked.active,
        }
        .load(Ordering::SeqCst)
    }

    /// Region in which to lock or confine the pointer
    pub fn region(&self) -> Option<&RegionAttributes> {
        match self {
            PointerConstraint::Confined(confined) => confined.region(),
            PointerConstraint::Locked(locked) => locked.region(),
        }
    }

    fn lifetime(&self) -> WEnum<Lifetime> {
        match self {
            PointerConstraint::Confined(confined) => confined.lifetime,
            PointerConstraint::Locked(locked) => locked.lifetime,
        }
    }

    fn id(&self) -> ObjectId {
        match self {
            Self::Confined(confined) => confined.handle.id(),
            Self::Locked(locked) => locked.handle.id(),
        }
    }

    fn apply_pending(&mut self, pending: PendingConstraint) -> Option<Point<f64, Logical>> {
        if let Some(region) = pending.region {
            match self {
                Self::Confined(confined) => confined.region = region,
                Self::Locked(locked) => locked.region = region,
            }
        }
        if let Self::Locked(locked) = self
            && let Some(hint) = pending.cursor_position_hint
        {
            locked.cursor_position_hint = Some(hint);
            return Some(hint);
        }
        None
    }
}

#[derive(Debug, Default)]
struct PendingConstraint {
    region: Option<Option<RegionAttributes>>,
    cursor_position_hint: Option<Point<f64, Logical>>,
}

#[derive(Debug, Default)]
struct ConstraintCachedState {
    changes: HashMap<ObjectId, PendingConstraint>,
}

impl Cacheable for ConstraintCachedState {
    fn commit(&mut self, _dh: &DisplayHandle) -> Self {
        Self {
            changes: std::mem::take(&mut self.changes),
        }
    }

    fn merge_into(self, into: &mut Self, _dh: &DisplayHandle) {
        for (id, pending) in self.changes {
            let current = into.changes.entry(id).or_default();
            if pending.region.is_some() {
                current.region = pending.region;
            }
            if pending.cursor_position_hint.is_some() {
                current.cursor_position_hint = pending.cursor_position_hint;
            }
        }
    }
}

/// Pointer constraints state.
#[derive(Debug)]
pub struct PointerConstraintsState {
    global: GlobalId,
}

impl PointerConstraintsState {
    /// Create a new pointer constraints global
    pub fn new<D>(display: &DisplayHandle) -> Self
    where
        D: GlobalDispatch<ZwpPointerConstraintsV1, GlobalData>,
        D: Dispatch<ZwpPointerConstraintsV1, GlobalData>,
        D: Dispatch<ZwpConfinedPointerV1, PointerConstraintUserData<D>>,
        D: Dispatch<ZwpLockedPointerV1, PointerConstraintUserData<D>>,
        D: SeatHandler,
        D: 'static,
    {
        let global = display.create_global::<D, ZwpPointerConstraintsV1, _>(VERSION, GlobalData);

        Self { global }
    }

    /// Get the id of ZwpPointerConstraintsV1 global
    pub fn global(&self) -> GlobalId {
        self.global.clone()
    }
}

#[doc(hidden)]
#[derive(Debug)]
pub struct PointerConstraintUserData<D: SeatHandler> {
    surface: WlSurface,
    arc: Weak<PointerRc<D>>,
}

struct PointerConstraintData<D: SeatHandler + 'static> {
    constraints: HashMap<PointerHandle<D>, PointerConstraint>,
}

// TODO Public method to get current constraints for surface/seat
/// Get constraint for surface and pointer, if any
pub fn with_pointer_constraint<
    D: SeatHandler + 'static,
    T,
    F: FnOnce(Option<PointerConstraintRef<'_, D>>) -> T,
>(
    surface: &WlSurface,
    pointer: &PointerHandle<D>,
    f: F,
) -> T {
    with_constraint_data::<D, _, _>(surface, |data| {
        let constraint = data.and_then(|data| match data.constraints.entry(pointer.clone()) {
            hash_map::Entry::Occupied(entry) => Some(PointerConstraintRef { entry }),
            hash_map::Entry::Vacant(_) => None,
        });
        f(constraint)
    })
}

fn commit_hook<D: SeatHandler + PointerConstraintsHandler + 'static>(
    state: &mut D,
    _dh: &DisplayHandle,
    surface: &WlSurface,
) {
    // `with_constraint_data` locks the pointer constraints,
    // so we collect the hints first into a Vec, then release the mutex
    // and only once the mutex is released, we call the handler method.
    //
    // This is to avoid deadlocks when the handler method might try to access the constraints again.
    // It's not a hypothetical, it bit me while implementing the position hint functionality.
    let mut changes = compositor::with_states(surface, |states| {
        std::mem::take(
            &mut states
                .cached_state
                .get::<ConstraintCachedState>()
                .current()
                .changes,
        )
    });
    let position_hints = with_constraint_data::<D, _, _>(surface, |data| {
        let data = data.unwrap();
        data.constraints
            .iter_mut()
            .filter_map(|(pointer, constraint)| {
                changes
                    .remove(&constraint.id())
                    .and_then(|pending| constraint.apply_pending(pending))
                    .map(|hint| (pointer.clone(), hint))
            })
            .collect::<Vec<_>>()
    });

    for (pointer, hint) in position_hints {
        state.cursor_position_hint(surface, &pointer, hint);
    }
}

/// Get `PointerConstraintData` associated with a surface, if any.
fn with_constraint_data<
    D: SeatHandler + 'static,
    T,
    F: FnOnce(Option<&mut PointerConstraintData<D>>) -> T,
>(
    surface: &WlSurface,
    f: F,
) -> T {
    compositor::with_states(surface, |states| {
        let data = states.data_map.get::<Mutex<PointerConstraintData<D>>>();
        if let Some(data) = data {
            f(Some(&mut data.lock().unwrap()))
        } else {
            f(None)
        }
    })
}

/// Add constraint for surface, or raise protocol error if one exists
fn add_constraint<D: SeatHandler + PointerConstraintsHandler + 'static>(
    pointer_constraints: &ZwpPointerConstraintsV1,
    surface: &WlSurface,
    pointer: &PointerHandle<D>,
    constraint: PointerConstraint,
) {
    let mut added = false;
    compositor::with_states(surface, |states| {
        added = states.data_map.insert_if_missing_threadsafe(|| {
            Mutex::new(PointerConstraintData::<D> {
                constraints: HashMap::new(),
            })
        });
        let data = states.data_map.get::<Mutex<PointerConstraintData<D>>>().unwrap();
        let mut data = data.lock().unwrap();

        if data.constraints.contains_key(pointer) {
            pointer_constraints.post_error(
                zwp_pointer_constraints_v1::Error::AlreadyConstrained,
                "pointer constraint already exists for surface and seat",
            );
        } else {
            data.constraints.insert(pointer.clone(), constraint);
        }
    });

    if added {
        compositor::add_post_commit_hook(surface, commit_hook::<D>);
    }
}

fn retire_pending(surface: &WlSurface, object: ObjectId) {
    compositor::with_states(surface, |states| {
        if states.cached_state.has::<ConstraintCachedState>() {
            states
                .cached_state
                .get::<ConstraintCachedState>()
                .for_each_mut(|state| {
                    state.changes.remove(&object);
                });
        }
    });
}

fn remove_constraint<D: SeatHandler + PointerConstraintsHandler + 'static>(
    state: &mut D,
    surface: &WlSurface,
    pointer: &PointerHandle<D>,
    object: ObjectId,
) {
    let is_removed = with_constraint_data::<D, _, _>(surface, |data| {
        if let Some(data) = data
            && let hash_map::Entry::Occupied(entry) = data.constraints.entry(pointer.clone())
            && entry.get().id() == object
        {
            return Some(entry.remove());
        }
        None
    });

    if let Some(constraint) = is_removed {
        state.remove_constraint(surface, pointer, ConstraintRemove::Destroyed(constraint));
    }
}

impl<D> Dispatch2<ZwpPointerConstraintsV1, D> for GlobalData
where
    D: Dispatch<ZwpConfinedPointerV1, PointerConstraintUserData<D>>,
    D: Dispatch<ZwpLockedPointerV1, PointerConstraintUserData<D>>,
    D: SeatHandler,
    D: PointerConstraintsHandler,
    D: 'static,
{
    fn request(
        &self,
        state: &mut D,
        _client: &wayland_server::Client,
        pointer_constraints: &ZwpPointerConstraintsV1,
        request: zwp_pointer_constraints_v1::Request,
        _dh: &DisplayHandle,
        data_init: &mut wayland_server::DataInit<'_, D>,
    ) {
        match request {
            zwp_pointer_constraints_v1::Request::LockPointer {
                id,
                surface,
                pointer,
                region,
                lifetime,
            } => {
                let region = region.as_ref().map(compositor::get_region_attributes);
                let arc = pointer.data::<PointerUserData<D>>().unwrap().arc.clone();
                let handle = data_init.init(
                    id,
                    PointerConstraintUserData {
                        surface: surface.clone(),
                        arc: arc.clone(),
                    },
                );
                if let Some(arc) = arc.upgrade() {
                    let pointer = PointerHandle { arc };
                    add_constraint(
                        pointer_constraints,
                        &surface,
                        &pointer,
                        PointerConstraint::Locked(LockedPointer {
                            handle,
                            region,
                            lifetime,
                            cursor_position_hint: None,
                            active: AtomicBool::new(false),
                        }),
                    );
                    state.new_constraint(&surface, &pointer);
                }
            }
            zwp_pointer_constraints_v1::Request::ConfinePointer {
                id,
                surface,
                pointer,
                region,
                lifetime,
            } => {
                let region = region.as_ref().map(compositor::get_region_attributes);
                let arc = pointer.data::<PointerUserData<D>>().unwrap().arc.clone();
                let handle = data_init.init(
                    id,
                    PointerConstraintUserData {
                        surface: surface.clone(),
                        arc: arc.clone(),
                    },
                );
                if let Some(arc) = arc.upgrade() {
                    let pointer = PointerHandle { arc };
                    add_constraint(
                        pointer_constraints,
                        &surface,
                        &pointer,
                        PointerConstraint::Confined(ConfinedPointer {
                            handle,
                            region,
                            lifetime,
                            active: AtomicBool::new(false),
                        }),
                    );
                    state.new_constraint(&surface, &pointer);
                }
            }
            zwp_pointer_constraints_v1::Request::Destroy => {}
            _ => unreachable!(),
        }
    }
}

impl<D> GlobalDispatch2<ZwpPointerConstraintsV1, D> for GlobalData
where
    D: Dispatch<ZwpPointerConstraintsV1, GlobalData> + SeatHandler + 'static,
{
    fn bind(
        &self,
        _state: &mut D,
        _dh: &DisplayHandle,
        _client: &Client,
        resource: New<ZwpPointerConstraintsV1>,
        data_init: &mut DataInit<'_, D>,
    ) {
        data_init.init(resource, GlobalData);
    }
}

impl<D> Dispatch2<ZwpConfinedPointerV1, D> for PointerConstraintUserData<D>
where
    D: SeatHandler,
    D: PointerConstraintsHandler,
    D: 'static,
{
    fn request(
        &self,
        _state: &mut D,
        _client: &wayland_server::Client,
        confined_pointer: &ZwpConfinedPointerV1,
        request: zwp_confined_pointer_v1::Request,
        _dh: &DisplayHandle,
        _data_init: &mut wayland_server::DataInit<'_, D>,
    ) {
        match request {
            zwp_confined_pointer_v1::Request::SetRegion { region } => {
                compositor::with_states(&self.surface, |states| {
                    states
                        .cached_state
                        .get::<ConstraintCachedState>()
                        .pending()
                        .changes
                        .entry(confined_pointer.id())
                        .or_default()
                        .region = Some(region.as_ref().map(compositor::get_region_attributes));
                });
            }
            zwp_confined_pointer_v1::Request::Destroy => {}
            _ => unreachable!(),
        }
    }

    fn destroyed(
        &self,
        state: &mut D,
        _client: wayland_server::backend::ClientId,
        resource: &ZwpConfinedPointerV1,
    ) {
        retire_pending(&self.surface, resource.id());
        let Some(pointer) = &self.arc.upgrade().map(|arc| PointerHandle { arc }) else {
            return;
        };

        remove_constraint(state, &self.surface, pointer, resource.id());
    }
}

impl<D> Dispatch2<ZwpLockedPointerV1, D> for PointerConstraintUserData<D>
where
    D: SeatHandler,
    D: PointerConstraintsHandler,
    D: 'static,
{
    fn request(
        &self,
        _state: &mut D,
        _client: &wayland_server::Client,
        locked_pointer: &ZwpLockedPointerV1,
        request: zwp_locked_pointer_v1::Request,
        _dh: &DisplayHandle,
        _data_init: &mut wayland_server::DataInit<'_, D>,
    ) {
        match request {
            zwp_locked_pointer_v1::Request::SetCursorPositionHint { surface_x, surface_y } => {
                compositor::with_states(&self.surface, |states| {
                    states
                        .cached_state
                        .get::<ConstraintCachedState>()
                        .pending()
                        .changes
                        .entry(locked_pointer.id())
                        .or_default()
                        .cursor_position_hint = Some((surface_x, surface_y).into());
                });
            }
            zwp_locked_pointer_v1::Request::SetRegion { region } => {
                compositor::with_states(&self.surface, |states| {
                    states
                        .cached_state
                        .get::<ConstraintCachedState>()
                        .pending()
                        .changes
                        .entry(locked_pointer.id())
                        .or_default()
                        .region = Some(region.as_ref().map(compositor::get_region_attributes));
                });
            }
            zwp_locked_pointer_v1::Request::Destroy => {}
            _ => unreachable!(),
        }
    }

    fn destroyed(
        &self,
        state: &mut D,
        _client: wayland_server::backend::ClientId,
        resource: &ZwpLockedPointerV1,
    ) {
        retire_pending(&self.surface, resource.id());
        let Some(pointer) = &self.arc.upgrade().map(|arc| PointerHandle { arc }) else {
            return;
        };

        remove_constraint(state, &self.surface, pointer, resource.id());
    }
}
