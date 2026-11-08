#![cfg(feature = "wayland_frontend")]

use std::{
    net::Shutdown,
    os::unix::net::UnixStream,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use rustix::event::{PollFd, PollFlags, Timespec, poll};
use smithay::{
    reexports::wayland_server::{
        Client, Display, Resource, backend::ClientData, protocol::wl_surface::WlSurface,
    },
    utils::Serial,
    wayland::{
        compositor::{CompositorClientState, CompositorHandler, CompositorState},
        shell::xdg::{PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState},
        xdg_foreign::{XdgForeignHandler, XdgForeignState},
    },
};
use wayland_client::{
    Connection, Dispatch, EventQueue, Proxy, QueueHandle, delegate_noop,
    protocol::{wl_callback, wl_compositor, wl_registry, wl_surface},
};
use wayland_protocols::xdg::{
    foreign::zv2::client::{
        zxdg_exported_v2 as exported, zxdg_exporter_v2 as exporter, zxdg_imported_v2 as imported,
        zxdg_importer_v2 as importer,
    },
    shell::client::{xdg_popup, xdg_surface, xdg_toplevel, xdg_wm_base},
};

#[derive(Debug, Default)]
struct ClientState(CompositorClientState);
impl ClientData for ClientState {}
struct State {
    compositor: CompositorState,
    shell: XdgShellState,
    foreign: XdgForeignState,
}
impl CompositorHandler for State {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor
    }
    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        &client.get_data::<ClientState>().unwrap().0
    }
    fn commit(&mut self, _: &WlSurface) {}
}
impl XdgShellHandler for State {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.shell
    }
    fn new_toplevel(&mut self, _: ToplevelSurface) {}
    fn new_popup(&mut self, _: PopupSurface, _: PositionerState) {}
    fn grab(
        &mut self,
        _: PopupSurface,
        _: smithay::reexports::wayland_server::protocol::wl_seat::WlSeat,
        _: Serial,
    ) {
    }
    fn reposition_request(&mut self, _: PopupSurface, _: PositionerState, _: u32) {}
}
impl XdgForeignHandler for State {
    fn xdg_foreign_state(&mut self) -> &mut XdgForeignState {
        &mut self.foreign
    }
}
smithay::delegate_dispatch2!(State);

#[derive(Default)]
struct Probe {
    compositor: Option<wl_compositor::WlCompositor>,
    shell: Option<xdg_wm_base::XdgWmBase>,
    exporter: Option<exporter::ZxdgExporterV2>,
    importer: Option<importer::ZxdgImporterV2>,
}
impl Dispatch<wl_registry::WlRegistry, ()> for Probe {
    fn event(
        probe: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            match interface.as_str() {
                "wl_compositor" => probe.compositor = Some(registry.bind(name, version.min(5), qh, ())),
                "xdg_wm_base" => probe.shell = Some(registry.bind(name, version.min(7), qh, ())),
                "zxdg_exporter_v2" => probe.exporter = Some(registry.bind(name, 1, qh, ())),
                "zxdg_importer_v2" => probe.importer = Some(registry.bind(name, 1, qh, ())),
                _ => (),
            }
        }
    }
}
impl Dispatch<wl_callback::WlCallback, Arc<AtomicBool>> for Probe {
    fn event(
        _: &mut Self,
        _: &wl_callback::WlCallback,
        _: wl_callback::Event,
        done: &Arc<AtomicBool>,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        done.store(true, Ordering::Relaxed);
    }
}
type Handle = Arc<Mutex<String>>;
impl Dispatch<exported::ZxdgExportedV2, Handle> for Probe {
    fn event(
        _: &mut Self,
        _: &exported::ZxdgExportedV2,
        event: exported::Event,
        handle: &Handle,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let exported::Event::Handle { handle: value } = event {
            *handle.lock().unwrap() = value;
        }
    }
}
impl Dispatch<imported::ZxdgImportedV2, Arc<AtomicUsize>> for Probe {
    fn event(
        _: &mut Self,
        _: &imported::ZxdgImportedV2,
        event: imported::Event,
        destroyed: &Arc<AtomicUsize>,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let imported::Event::Destroyed = event {
            destroyed.fetch_add(1, Ordering::Relaxed);
        }
    }
}
delegate_noop!(Probe: wl_compositor::WlCompositor);
delegate_noop!(Probe: wl_surface::WlSurface);
delegate_noop!(Probe: ignore xdg_wm_base::XdgWmBase);
delegate_noop!(Probe: ignore xdg_surface::XdgSurface);
delegate_noop!(Probe: ignore xdg_toplevel::XdgToplevel);
delegate_noop!(Probe: ignore xdg_popup::XdgPopup);
delegate_noop!(Probe: exporter::ZxdgExporterV2);
delegate_noop!(Probe: importer::ZxdgImporterV2);

struct Runtime {
    display: Display<State>,
    state: State,
}
impl Runtime {
    fn new() -> Self {
        let display = Display::new().unwrap();
        let state = State {
            compositor: CompositorState::new::<State>(&display.handle()),
            shell: XdgShellState::new::<State>(&display.handle()),
            foreign: XdgForeignState::new::<State>(&display.handle()),
        };
        Self { display, state }
    }
    fn dispatch(&mut self) {
        self.display.dispatch_clients(&mut self.state).unwrap();
        self.display.flush_clients().unwrap();
    }
}
struct Wire {
    client: Client,
    socket: UnixStream,
    connection: Connection,
    queue: EventQueue<Probe>,
    probe: Probe,
}
struct Top {
    surface: wl_surface::WlSurface,
    _xdg: xdg_surface::XdgSurface,
    top: xdg_toplevel::XdgToplevel,
}
impl Wire {
    fn new(runtime: &mut Runtime) -> Self {
        let (server, socket) = UnixStream::pair().unwrap();
        let client = runtime
            .display
            .handle()
            .insert_client(server, Arc::new(ClientState::default()))
            .unwrap();
        let connection = Connection::from_socket(socket.try_clone().unwrap()).unwrap();
        let queue = connection.new_event_queue();
        connection.display().get_registry(&queue.handle(), ());
        let mut wire = Self {
            client,
            socket,
            connection,
            queue,
            probe: Probe::default(),
        };
        wire.sync(runtime);
        wire
    }
    fn sync(&mut self, runtime: &mut Runtime) {
        let done = Arc::new(AtomicBool::new(false));
        self.connection.display().sync(&self.queue.handle(), done.clone());
        let deadline = Instant::now() + Duration::from_secs(2);
        while !done.load(Ordering::Relaxed) {
            self.connection.flush().unwrap();
            runtime.dispatch();
            self.queue.dispatch_pending(&mut self.probe).unwrap();
            if done.load(Ordering::Relaxed) {
                break;
            }
            let Some(read) = self.queue.prepare_read() else {
                continue;
            };
            let fd = read.connection_fd();
            let timeout = Timespec::try_from(
                deadline
                    .checked_duration_since(Instant::now())
                    .expect("roundtrip timed out"),
            )
            .unwrap();
            assert_ne!(
                poll(&mut [PollFd::new(&fd, PollFlags::IN)], Some(&timeout)).unwrap(),
                0
            );
            read.read().unwrap();
            self.queue.dispatch_pending(&mut self.probe).unwrap();
        }
    }
    fn top(&mut self, runtime: &mut Runtime) -> Top {
        let surface = self
            .probe
            .compositor
            .as_ref()
            .unwrap()
            .create_surface(&self.queue.handle(), ());
        let xdg = self
            .probe
            .shell
            .as_ref()
            .unwrap()
            .get_xdg_surface(&surface, &self.queue.handle(), ());
        let top = xdg.get_toplevel(&self.queue.handle(), ());
        self.sync(runtime);
        Top {
            surface,
            _xdg: xdg,
            top,
        }
    }
    fn native(&self, runtime: &Runtime, top: &Top) -> ToplevelSurface {
        runtime
            .state
            .shell
            .toplevel_surfaces()
            .iter()
            .find(|surface| {
                surface.wl_surface().client().as_ref() == Some(&self.client)
                    && surface.wl_surface().id().protocol_id() == top.surface.id().protocol_id()
            })
            .unwrap()
            .clone()
    }
    fn export(&mut self, runtime: &mut Runtime, top: &Top) -> (exported::ZxdgExportedV2, String) {
        let handle = Handle::default();
        let export = self.probe.exporter.as_ref().unwrap().export_toplevel(
            &top.surface,
            &self.queue.handle(),
            handle.clone(),
        );
        self.sync(runtime);
        let value = handle.lock().unwrap().clone();
        assert!(!value.is_empty());
        (export, value)
    }
    fn import(
        &mut self,
        runtime: &mut Runtime,
        handle: &str,
    ) -> (imported::ZxdgImportedV2, Arc<AtomicUsize>) {
        let destroyed = Arc::new(AtomicUsize::new(0));
        let imported = self.probe.importer.as_ref().unwrap().import_toplevel(
            handle.into(),
            &self.queue.handle(),
            destroyed.clone(),
        );
        self.sync(runtime);
        (imported, destroyed)
    }
}

#[test]
fn exporters_reject_surfaces_without_a_live_toplevel_role() {
    for inert in [false, true] {
        let mut runtime = Runtime::new();
        let mut wire = Wire::new(&mut runtime);
        let mut healthy = Wire::new(&mut runtime);
        let surface = if inert {
            let top = wire.top(&mut runtime);
            top.top.destroy();
            wire.sync(&mut runtime);
            top.surface
        } else {
            wire.probe
                .compositor
                .as_ref()
                .unwrap()
                .create_surface(&wire.queue.handle(), ())
        };
        let _export = wire.probe.exporter.as_ref().unwrap().export_toplevel(
            &surface,
            &wire.queue.handle(),
            Handle::default(),
        );
        wire.connection.flush().unwrap();
        runtime.dispatch();
        assert!(wire.queue.blocking_dispatch(&mut wire.probe).is_err());
        let error = wire.connection.protocol_error().unwrap();
        assert_eq!(error.object_interface, "zxdg_exporter_v2");
        assert_eq!(error.code, exporter::Error::InvalidSurface as u32);
        healthy.sync(&mut runtime);
    }
}

#[test]
fn multiple_imports_and_children_revoke_independently() {
    let mut runtime = Runtime::new();
    let mut a = Wire::new(&mut runtime);
    let mut b = Wire::new(&mut runtime);
    let mut c = Wire::new(&mut runtime);
    let parent = a.top(&mut runtime);
    let (export, handle) = a.export(&mut runtime, &parent);
    a.probe.exporter.as_ref().unwrap().destroy();
    a.sync(&mut runtime);
    let (first, first_dead) = b.import(&mut runtime, &handle);
    let (second, second_dead) = c.import(&mut runtime, &handle);
    b.probe.importer.as_ref().unwrap().destroy();
    b.sync(&mut runtime);
    let b1 = b.top(&mut runtime);
    let b2 = b.top(&mut runtime);
    let c1 = c.top(&mut runtime);
    first.set_parent_of(&b1.surface);
    first.set_parent_of(&b2.surface);
    second.set_parent_of(&c1.surface);
    b.sync(&mut runtime);
    c.sync(&mut runtime);
    let expected = Some(a.native(&runtime, &parent).wl_surface().clone());
    assert_eq!(b.native(&runtime, &b1).parent(), expected);
    assert_eq!(b.native(&runtime, &b2).parent(), expected);
    assert_eq!(c.native(&runtime, &c1).parent(), expected);
    first.destroy();
    b.sync(&mut runtime);
    assert!(b.native(&runtime, &b1).parent().is_none());
    assert!(b.native(&runtime, &b2).parent().is_none());
    assert_eq!(c.native(&runtime, &c1).parent(), expected);
    export.destroy();
    a.sync(&mut runtime);
    c.sync(&mut runtime);
    assert!(c.native(&runtime, &c1).parent().is_none());
    assert_eq!(first_dead.load(Ordering::Relaxed), 0);
    assert_eq!(second_dead.load(Ordering::Relaxed), 1);
    let (_invalid, invalid) = c.import(&mut runtime, &handle);
    assert_eq!(invalid.load(Ordering::Relaxed), 1);
}

#[test]
fn export_revocation_surface_destruction_and_disconnect_invalidate_every_import_once() {
    for reason in 0..4 {
        let mut runtime = Runtime::new();
        let mut a = Wire::new(&mut runtime);
        let mut b = Wire::new(&mut runtime);
        let parent = a.top(&mut runtime);
        let (export, handle) = a.export(&mut runtime, &parent);
        let (one, one_dead) = b.import(&mut runtime, &handle);
        let (_two, two_dead) = b.import(&mut runtime, &handle);
        let child = b.top(&mut runtime);
        one.set_parent_of(&child.surface);
        b.sync(&mut runtime);
        match reason {
            0 => {
                export.destroy();
                a.sync(&mut runtime);
            }
            1 => {
                parent.surface.destroy();
                a.sync(&mut runtime);
            }
            2 => {
                a.socket.shutdown(Shutdown::Both).unwrap();
                runtime.dispatch();
            }
            3 => {
                parent.top.destroy();
                a.sync(&mut runtime);
            }
            _ => unreachable!(),
        }
        b.sync(&mut runtime);
        assert_eq!(one_dead.load(Ordering::Relaxed), 1, "revocation reason {reason}");
        assert_eq!(two_dead.load(Ordering::Relaxed), 1, "revocation reason {reason}");
        assert!(b.native(&runtime, &child).parent().is_none());
        one.set_parent_of(&child.surface);
        b.sync(&mut runtime);
        assert!(b.native(&runtime, &child).parent().is_none());
        assert_eq!(one_dead.load(Ordering::Relaxed), 1);
    }
}

#[test]
fn role_replacement_does_not_reuse_export_authority_or_revoke_the_new_role() {
    let mut runtime = Runtime::new();
    let mut a = Wire::new(&mut runtime);
    let mut b = Wire::new(&mut runtime);
    let mut parent = a.top(&mut runtime);
    let (_old_export, old_handle) = a.export(&mut runtime, &parent);
    let (_old_import, old_dead) = b.import(&mut runtime, &old_handle);
    parent.top.destroy();
    parent.top = parent._xdg.get_toplevel(&a.queue.handle(), ());
    let token = Handle::default();
    let _fresh_export =
        a.probe
            .exporter
            .as_ref()
            .unwrap()
            .export_toplevel(&parent.surface, &a.queue.handle(), token.clone());
    a.sync(&mut runtime);
    b.sync(&mut runtime);
    assert_eq!(old_dead.load(Ordering::Relaxed), 1);
    let fresh_handle = token.lock().unwrap().clone();
    assert!(!fresh_handle.is_empty() && fresh_handle != old_handle);
    let (fresh, fresh_dead) = b.import(&mut runtime, &fresh_handle);
    let child = b.top(&mut runtime);
    fresh.set_parent_of(&child.surface);
    b.sync(&mut runtime);
    assert_eq!(fresh_dead.load(Ordering::Relaxed), 0);
    assert_eq!(
        b.native(&runtime, &child).parent(),
        Some(a.native(&runtime, &parent).wl_surface().clone())
    );
    let (_invalid, invalid) = b.import(&mut runtime, &old_handle);
    assert_eq!(invalid.load(Ordering::Relaxed), 1);
}

#[test]
fn non_toplevel_children_and_cyclic_relationships_disconnect_only_the_offender() {
    for cycle in [false, true] {
        let mut runtime = Runtime::new();
        let mut a = Wire::new(&mut runtime);
        let mut b = Wire::new(&mut runtime);
        let parent = a.top(&mut runtime);
        let (_export, handle) = a.export(&mut runtime, &parent);
        let (import, _) = b.import(&mut runtime, &handle);
        if cycle {
            let child = b.top(&mut runtime);
            import.set_parent_of(&child.surface);
            b.sync(&mut runtime);
            let (_export, handle) = b.export(&mut runtime, &child);
            let (back, _) = a.import(&mut runtime, &handle);
            back.set_parent_of(&parent.surface);
            a.connection.flush().unwrap();
            runtime.dispatch();
            assert!(a.queue.blocking_dispatch(&mut a.probe).is_err());
            assert_eq!(
                a.connection.protocol_error().unwrap().object_interface,
                "zxdg_imported_v2"
            );
            b.sync(&mut runtime);
        } else {
            let raw = b
                .probe
                .compositor
                .as_ref()
                .unwrap()
                .create_surface(&b.queue.handle(), ());
            import.set_parent_of(&raw);
            b.connection.flush().unwrap();
            runtime.dispatch();
            assert!(b.queue.blocking_dispatch(&mut b.probe).is_err());
            let error = b.connection.protocol_error().unwrap();
            assert_eq!(error.object_interface, "zxdg_imported_v2");
            assert_eq!(error.code, imported::Error::InvalidSurface as u32);
            a.sync(&mut runtime);
        }
    }
}

#[test]
fn superseded_imports_and_same_parent_native_assignments_do_not_lose_the_current_relationship() {
    let mut runtime = Runtime::new();
    let mut wire = Wire::new(&mut runtime);
    let parent = wire.top(&mut runtime);
    let child = wire.top(&mut runtime);
    let (first_export, first_handle) = wire.export(&mut runtime, &parent);
    let (_second_export, second_handle) = wire.export(&mut runtime, &parent);
    let (first, _) = wire.import(&mut runtime, &first_handle);
    let (second, _) = wire.import(&mut runtime, &second_handle);
    first.set_parent_of(&child.surface);
    second.set_parent_of(&child.surface);
    wire.sync(&mut runtime);
    first_export.destroy();
    wire.sync(&mut runtime);
    let expected = Some(wire.native(&runtime, &parent).wl_surface().clone());
    assert_eq!(wire.native(&runtime, &child).parent(), expected);
    child.top.set_parent(Some(&parent.top));
    wire.sync(&mut runtime);
    second.destroy();
    wire.sync(&mut runtime);
    assert_eq!(wire.native(&runtime, &child).parent(), expected);
}
