//! Utilities for handling the security context protocol

use std::{
    os::unix::{io::OwnedFd, net::UnixListener},
    sync::Mutex,
};
use wayland_protocols::wp::security_context::v1::server::{
    wp_security_context_manager_v1::{self, WpSecurityContextManagerV1},
    wp_security_context_v1::{self, WpSecurityContextV1},
};
use wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
    backend::{ClientId, GlobalId},
};

use crate::wayland::{Dispatch2, GlobalData, GlobalDispatch2};

mod listener_source;
pub use listener_source::SecurityContextListenerSource;

const MANAGER_VERSION: u32 = 1;

/// Handler for security context protocol
pub trait SecurityContextHandler {
    /// A new uncommitted context owns the listener and close descriptors.
    fn context_pending(&mut self, _context: &WpSecurityContextV1) {}

    /// The protocol object has been destroyed. Committed listeners remain active.
    fn context_destroyed(&mut self, _context: &WpSecurityContextV1) {}

    /// A context has been committed. `resource` identifies its protocol object;
    /// `source` accepts connections carrying the immutable `context` metadata.
    fn context_created(
        &mut self,
        resource: &WpSecurityContextV1,
        source: SecurityContextListenerSource,
        context: SecurityContext,
    );
}

/// State of the [WpSecurityContextManagerV1] global
#[derive(Debug)]
pub struct SecurityContextState {
    global: GlobalId,
}

/// User data for a `WpSecurityContextV1`
#[derive(Debug)]
pub struct SecurityContextUserData(Mutex<Option<SecurityContextBuilder>>);

#[derive(Debug)]
struct SecurityContextBuilder {
    listen_fd: UnixListener,
    close_fd: OwnedFd,
    sandbox_engine: Option<String>,
    app_id: Option<String>,
    instance_id: Option<String>,
}

/// A security context associated with a listener created through security
/// context protocol, and with clients connected through that listener.
#[derive(Clone, Debug)]
pub struct SecurityContext {
    /// Name of the sandbox engine associated with the security context
    pub sandbox_engine: Option<String>,
    /// Opaque sandbox-specific ID for an application
    pub app_id: Option<String>,
    /// Opaque sandbox-specific ID for an instance of an application
    pub instance_id: Option<String>,
    /// Client that created the security context
    pub creator_client_id: ClientId,
}

impl SecurityContextState {
    /// Register new [WpSecurityContextManagerV1] global
    ///
    /// Filter determines if what clients see the global. It *must* exclude clients
    /// created through a security context for the protocol to be correct and secure.
    pub fn new<D, F>(display: &DisplayHandle, filter: F) -> Self
    where
        D: GlobalDispatch<WpSecurityContextManagerV1, SecurityContextGlobalData>,
        D: Dispatch<WpSecurityContextManagerV1, GlobalData>,
        D: Dispatch<WpSecurityContextV1, SecurityContextUserData>,
        D: 'static,
        F: for<'c> Fn(&'c Client) -> bool + Send + Sync + 'static,
    {
        let global_data = SecurityContextGlobalData {
            filter: Box::new(filter),
        };
        let global = display.create_global::<D, WpSecurityContextManagerV1, _>(MANAGER_VERSION, global_data);

        Self { global }
    }

    /// [WpSecurityContextManagerV1] GlobalId getter
    pub fn global(&self) -> GlobalId {
        self.global.clone()
    }
}

/// User data for a `WpSecurityContextManagerV1` global
#[allow(missing_debug_implementations)]
pub struct SecurityContextGlobalData {
    filter: Box<dyn for<'c> Fn(&'c Client) -> bool + Send + Sync>,
}

impl<D> GlobalDispatch2<WpSecurityContextManagerV1, D> for SecurityContextGlobalData
where
    D: Dispatch<WpSecurityContextManagerV1, GlobalData>,
    D: 'static,
{
    fn bind(
        &self,
        _state: &mut D,
        _dh: &DisplayHandle,
        _client: &Client,
        resource: New<WpSecurityContextManagerV1>,
        data_init: &mut DataInit<'_, D>,
    ) {
        data_init.init(resource, GlobalData);
    }

    fn can_view(&self, client: &Client) -> bool {
        (self.filter)(client)
    }
}

impl<D> Dispatch2<WpSecurityContextManagerV1, D> for GlobalData
where
    D: Dispatch<WpSecurityContextV1, SecurityContextUserData> + SecurityContextHandler,
    D: 'static,
{
    fn request(
        &self,
        state: &mut D,
        _client: &wayland_server::Client,
        manager: &WpSecurityContextManagerV1,
        request: wp_security_context_manager_v1::Request,
        _dh: &DisplayHandle,
        data_init: &mut wayland_server::DataInit<'_, D>,
    ) {
        match request {
            wp_security_context_manager_v1::Request::CreateListener {
                id,
                listen_fd,
                close_fd,
            } => {
                let valid_listener = rustix::net::sockopt::socket_type(&listen_fd)
                    .is_ok_and(|kind| kind == rustix::net::SocketType::STREAM)
                    && rustix::net::sockopt::socket_acceptconn(&listen_fd).unwrap_or(false)
                    && rustix::net::getsockname(&listen_fd)
                        .is_ok_and(|address| address.address_family() == rustix::net::AddressFamily::UNIX);
                if !valid_listener {
                    data_init.init(id, SecurityContextUserData(Mutex::new(None)));
                    manager.post_error(
                        wp_security_context_manager_v1::Error::InvalidListenFd,
                        "listen_fd must be a bound, listening Unix stream socket",
                    );
                    return;
                }
                let data = SecurityContextUserData(Mutex::new(Some(SecurityContextBuilder {
                    listen_fd: UnixListener::from(listen_fd),
                    close_fd,
                    sandbox_engine: None,
                    app_id: None,
                    instance_id: None,
                })));
                let resource = data_init.init(id, data);
                state.context_pending(&resource);
            }
            wp_security_context_manager_v1::Request::Destroy => {}
            _ => unreachable!(),
        }
    }
}

impl<D> Dispatch2<WpSecurityContextV1, D> for SecurityContextUserData
where
    D: SecurityContextHandler + 'static,
{
    fn request(
        &self,
        state: &mut D,
        client: &wayland_server::Client,
        context: &WpSecurityContextV1,
        request: wp_security_context_v1::Request,
        dh: &DisplayHandle,
        _data_init: &mut wayland_server::DataInit<'_, D>,
    ) {
        let mut data = self.0.lock().unwrap();

        if matches!(request, wp_security_context_v1::Request::Destroy) {
            return;
        }

        let Some(builder) = &mut *data else {
            context.post_error(
                wp_security_context_v1::Error::AlreadyUsed,
                "Security context already used",
            );
            return;
        };

        match request {
            wp_security_context_v1::Request::SetSandboxEngine { name } => {
                if builder.sandbox_engine.is_some() {
                    context.post_error(
                        wp_security_context_v1::Error::AlreadySet,
                        "Security context already has a sandbox engine",
                    );
                }
                builder.sandbox_engine = Some(name);
            }
            wp_security_context_v1::Request::SetAppId { app_id } => {
                if builder.app_id.is_some() {
                    context.post_error(
                        wp_security_context_v1::Error::AlreadySet,
                        "Security context already has an app id",
                    );
                }
                builder.app_id = Some(app_id);
            }
            wp_security_context_v1::Request::SetInstanceId { instance_id } => {
                if builder.instance_id.is_some() {
                    context.post_error(
                        wp_security_context_v1::Error::AlreadySet,
                        "Security context already has an instance id",
                    );
                }
                builder.instance_id = Some(instance_id);
            }
            wp_security_context_v1::Request::Commit => {
                let builder = data.take().unwrap();
                let listener_source = SecurityContextListenerSource::new(builder.listen_fd, builder.close_fd);
                let security_context = SecurityContext {
                    sandbox_engine: builder.sandbox_engine,
                    app_id: builder.app_id,
                    instance_id: builder.instance_id,
                    creator_client_id: client.id(),
                };
                match listener_source {
                    Ok(listener_source) => state.context_created(context, listener_source, security_context),
                    Err(e) => {
                        let backend = dh.backend_handle();
                        let mut display_object = None;
                        backend
                            .with_all_objects_for(client.id(), |object| {
                                if object.protocol_id() == 1 {
                                    display_object = Some(object);
                                }
                            })
                            .expect("context error is reported while dispatching a live client");
                        backend.post_error(
                            display_object.expect("live client has wl_display"),
                            2, // wl_display.error.no_memory; wl_display is backend-owned.
                            std::ffi::CString::new(format!(
                                "Cannot monitor security context descriptors: {e}"
                            ))
                            .unwrap(),
                        );
                    }
                }
            }
            _ => unreachable!(),
        }
    }
    fn destroyed(&self, state: &mut D, _client: ClientId, context: &WpSecurityContextV1) {
        state.context_destroyed(context);
    }
}
