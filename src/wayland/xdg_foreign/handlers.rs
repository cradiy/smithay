use std::{collections::HashSet, sync::Mutex};

use wayland_protocols::xdg::foreign::zv2::server::{
    zxdg_exported_v2::ZxdgExportedV2,
    zxdg_exporter_v2::{self, ZxdgExporterV2},
    zxdg_imported_v2::{self, ZxdgImportedV2},
    zxdg_importer_v2::{self, ZxdgImporterV2},
};
use wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, New, Resource,
    backend::{ClientId, ObjectId},
    protocol::wl_surface::WlSurface,
};

use super::{
    Child, ExportedState, ForeignSurfaceData, XdgExportedUserData, XdgForeignHandle, XdgForeignHandler,
    XdgImportedUserData,
};
use crate::wayland::{
    Dispatch2, GlobalData, GlobalDispatch2,
    compositor::{self, with_states},
    shell::xdg::{ToplevelSurface, XdgShellHandler, XdgToplevelSurfaceData, set_parent},
};

fn toplevel<D: XdgShellHandler>(state: &mut D, surface: &WlSurface) -> Option<ToplevelSurface> {
    state
        .xdg_shell_state()
        .toplevel_surfaces()
        .iter()
        .find(|top| top.wl_surface() == surface && surface.is_alive() && top.xdg_toplevel().is_alive())
        .cloned()
}

fn track_surface<D: XdgForeignHandler + XdgShellHandler>(surface: &WlSurface) {
    let added = with_states(surface, |states| {
        states
            .data_map
            .insert_if_missing_threadsafe(|| ForeignSurfaceData {
                imported_parent: Mutex::new(None),
                destroyed: |state, surface, role| {
                    invalidate_surface(
                        state
                            .downcast_mut::<D>()
                            .expect("foreign surfaces retain their compositor state type"),
                        surface,
                        role,
                    )
                },
            })
    });
    if added {
        compositor::add_destruction_hook::<D, _>(surface, |state, surface| {
            invalidate_surface(state, surface, None)
        });
    }
}

fn invalidate_child<D: XdgForeignHandler + XdgShellHandler>(state: &mut D, child: Child, parent: &WlSurface) {
    let changed = with_states(&child.surface, |states| {
        let data = states.data_map.get::<ForeignSurfaceData>().unwrap();
        let mut owner = data.imported_parent.lock().unwrap();
        if owner.as_ref() != Some(&child.imported.id()) {
            return false;
        }
        *owner = None;
        let Some(data) = states.data_map.get::<XdgToplevelSurfaceData>() else {
            return false;
        };
        let mut data = data.lock().unwrap();
        if data.parent.as_ref() != Some(parent) {
            return false;
        }
        data.parent = None;
        true
    });
    if !changed {
        return;
    }
    if let Some(top) = toplevel(state, &child.surface).filter(|top| top.xdg_toplevel().id() == child.role) {
        state.parent_changed(top);
    }
}

fn revoke_export<D: XdgForeignHandler + XdgShellHandler>(state: &mut D, handle: &XdgForeignHandle) {
    let Some(exported) = state.xdg_foreign_state().exported.remove(handle) else {
        return;
    };
    for imported in exported.imported_by {
        imported.destroyed();
    }
    let children: Vec<_> = state
        .xdg_foreign_state()
        .children
        .iter()
        .filter(|(_, child)| &child.exported == handle)
        .map(|(surface, _)| surface.clone())
        .collect();
    for surface in children {
        let child = state.xdg_foreign_state().children.remove(&surface).unwrap();
        invalidate_child(state, child, &exported.exported_surface);
    }
}

fn invalidate_surface<D: XdgForeignHandler + XdgShellHandler>(
    state: &mut D,
    surface: &WlSurface,
    role: Option<&ObjectId>,
) {
    if state
        .xdg_foreign_state()
        .children
        .get(&surface.id())
        .is_some_and(|child| role.is_none_or(|role| child.role == *role))
    {
        state.xdg_foreign_state().children.remove(&surface.id());
        super::reset_parent_ownership(surface);
    }
    let handles: Vec<_> = state
        .xdg_foreign_state()
        .exported
        .iter()
        .filter(|(_, export)| {
            export.exported_surface == *surface && role.is_none_or(|role| export.role == *role)
        })
        .map(|(handle, _)| handle.clone())
        .collect();
    for handle in handles {
        revoke_export(state, &handle);
    }
}

fn exported_surface<D: XdgForeignHandler + XdgShellHandler>(
    state: &mut D,
    handle: &XdgForeignHandle,
) -> Option<WlSurface> {
    let exported = state.xdg_foreign_state().exported.get(handle)?;
    let (surface, role) = (exported.exported_surface.clone(), exported.role.clone());
    if !toplevel(state, &surface).is_some_and(|top| top.xdg_toplevel().id() == role) {
        revoke_export(state, handle);
        return None;
    }
    Some(surface)
}

impl<D> GlobalDispatch2<ZxdgExporterV2, D> for GlobalData
where
    D: Dispatch<ZxdgExporterV2, GlobalData>,
{
    fn bind(
        &self,
        _: &mut D,
        _: &DisplayHandle,
        _: &Client,
        resource: New<ZxdgExporterV2>,
        data: &mut DataInit<'_, D>,
    ) {
        data.init(resource, GlobalData);
    }
}

impl<D> Dispatch2<ZxdgExporterV2, D> for GlobalData
where
    D: XdgForeignHandler + XdgShellHandler + Dispatch<ZxdgExportedV2, XdgExportedUserData>,
{
    fn request(
        &self,
        state: &mut D,
        _: &Client,
        resource: &ZxdgExporterV2,
        request: zxdg_exporter_v2::Request,
        _: &DisplayHandle,
        data: &mut DataInit<'_, D>,
    ) {
        match request {
            zxdg_exporter_v2::Request::ExportToplevel { id, surface } => {
                let Some(top) = toplevel(state, &surface) else {
                    resource.post_error(
                        zxdg_exporter_v2::Error::InvalidSurface,
                        "exported surface is not a live xdg_toplevel",
                    );
                    return;
                };
                let handle = loop {
                    let handle = XdgForeignHandle::new();
                    if !state.xdg_foreign_state().exported.contains_key(&handle) {
                        break handle;
                    }
                };
                let exported = data.init(
                    id,
                    XdgExportedUserData {
                        handle: handle.clone(),
                    },
                );
                exported.handle(handle.as_str().to_owned());
                track_surface::<D>(&surface);
                state.xdg_foreign_state().exported.insert(
                    handle,
                    ExportedState {
                        exported_surface: surface,
                        role: top.xdg_toplevel().id(),
                        imported_by: HashSet::new(),
                    },
                );
            }
            zxdg_exporter_v2::Request::Destroy => (),
            _ => unreachable!(),
        }
    }
}

impl<D: XdgForeignHandler + XdgShellHandler> Dispatch2<ZxdgExportedV2, D> for XdgExportedUserData {
    fn request(
        &self,
        _: &mut D,
        _: &Client,
        _: &ZxdgExportedV2,
        _: <ZxdgExportedV2 as Resource>::Request,
        _: &DisplayHandle,
        _: &mut DataInit<'_, D>,
    ) {
    }
    fn destroyed(&self, state: &mut D, _: ClientId, _: &ZxdgExportedV2) {
        revoke_export(state, &self.handle);
    }
}

impl<D> GlobalDispatch2<ZxdgImporterV2, D> for GlobalData
where
    D: Dispatch<ZxdgImporterV2, GlobalData>,
{
    fn bind(
        &self,
        _: &mut D,
        _: &DisplayHandle,
        _: &Client,
        resource: New<ZxdgImporterV2>,
        data: &mut DataInit<'_, D>,
    ) {
        data.init(resource, GlobalData);
    }
}

impl<D> Dispatch2<ZxdgImporterV2, D> for GlobalData
where
    D: XdgForeignHandler + XdgShellHandler + Dispatch<ZxdgImportedV2, XdgImportedUserData>,
{
    fn request(
        &self,
        state: &mut D,
        _: &Client,
        _: &ZxdgImporterV2,
        request: zxdg_importer_v2::Request,
        _: &DisplayHandle,
        data: &mut DataInit<'_, D>,
    ) {
        match request {
            zxdg_importer_v2::Request::ImportToplevel { id, handle } => {
                let handle = XdgForeignHandle(handle);
                let imported = data.init(
                    id,
                    XdgImportedUserData {
                        handle: handle.clone(),
                    },
                );
                if exported_surface(state, &handle).is_some() {
                    state
                        .xdg_foreign_state()
                        .exported
                        .get_mut(&handle)
                        .unwrap()
                        .imported_by
                        .insert(imported);
                } else {
                    imported.destroyed();
                }
            }
            zxdg_importer_v2::Request::Destroy => (),
            _ => unreachable!(),
        }
    }
}

impl<D: XdgForeignHandler + XdgShellHandler> Dispatch2<ZxdgImportedV2, D> for XdgImportedUserData {
    fn request(
        &self,
        state: &mut D,
        _: &Client,
        resource: &ZxdgImportedV2,
        request: zxdg_imported_v2::Request,
        _: &DisplayHandle,
        _: &mut DataInit<'_, D>,
    ) {
        match request {
            zxdg_imported_v2::Request::SetParentOf { surface } => {
                let Some(parent) = exported_surface(state, &self.handle) else {
                    return;
                };
                let Some(child) = toplevel(state, &surface) else {
                    resource.post_error(
                        zxdg_imported_v2::Error::InvalidSurface,
                        "child surface is not a live xdg_toplevel",
                    );
                    return;
                };
                let changed = child.parent().as_ref() != Some(&parent);
                if !set_parent(child.xdg_toplevel(), Some(parent)) {
                    resource.post_error(
                        zxdg_imported_v2::Error::InvalidSurface,
                        "invalid parent relationship",
                    );
                    return;
                }
                track_surface::<D>(&surface);
                with_states(&surface, |states| {
                    *states
                        .data_map
                        .get::<ForeignSurfaceData>()
                        .unwrap()
                        .imported_parent
                        .lock()
                        .unwrap() = Some(resource.id());
                });
                state.xdg_foreign_state().children.insert(
                    surface.id(),
                    Child {
                        surface,
                        role: child.xdg_toplevel().id(),
                        exported: self.handle.clone(),
                        imported: resource.clone(),
                    },
                );
                if changed {
                    state.parent_changed(child);
                }
            }
            zxdg_imported_v2::Request::Destroy => (),
            _ => unreachable!(),
        }
    }

    fn destroyed(&self, state: &mut D, _: ClientId, resource: &ZxdgImportedV2) {
        let Some(exported) = state.xdg_foreign_state().exported.get_mut(&self.handle) else {
            return;
        };
        exported.imported_by.remove(resource);
        let parent = exported.exported_surface.clone();
        let children: Vec<_> = state
            .xdg_foreign_state()
            .children
            .iter()
            .filter(|(_, child)| child.imported == *resource)
            .map(|(surface, _)| surface.clone())
            .collect();
        for surface in children {
            let child = state.xdg_foreign_state().children.remove(&surface).unwrap();
            invalidate_child(state, child, &parent);
        }
    }
}
