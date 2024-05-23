//! Implementation of headless Client and IPC service for Windows
//!
//! Try not to panic in the IPC service. Windows doesn't consider the
//! service to be stopped even if its only process ends, for some reason.
//! We must tell Windows explicitly when our service is stopping.

use crate::{CliCommon, SignalKind};
use anyhow::{anyhow, Context as _, Result};
use connlib_client_shared::file_logger;
use connlib_shared::{Cidrv4, Cidrv6, BUNDLE_ID};
use ip_network::{IpNetwork, Ipv4Network, Ipv6Network};
use std::{
    collections::HashSet,
    ffi::{c_void, OsString},
    future::Future,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddrV4, SocketAddrV6},
    path::{Path, PathBuf},
    pin::pin,
    str::FromStr,
    task::{Context, Poll},
    time::Duration,
};
use tokio::{net::windows::named_pipe, sync::mpsc};
use tracing::subscriber::set_global_default;
use tracing_subscriber::{layer::SubscriberExt as _, EnvFilter, Layer, Registry};
use windows::Win32::{
    NetworkManagement::IpHelper::{
        CreateIpForwardEntry2, DeleteIpForwardEntry2, InitializeIpForwardEntry, MIB_IPFORWARD_ROW2,
    },
    Security as WinSec,
};
use windows_service::{
    service::{
        ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus,
        ServiceType,
    },
    service_control_handler::{self, ServiceControlHandlerResult},
};

mod wintun_install;

#[cfg(debug_assertions)]
const SERVICE_RUST_LOG: &str = "firezone_headless_client=debug,firezone_tunnel=trace,phoenix_channel=debug,connlib_shared=debug,connlib_client_shared=debug,boringtun=debug,snownet=debug,str0m=info,info";

#[cfg(not(debug_assertions))]
const SERVICE_RUST_LOG: &str = "str0m=warn,info";

const SERVICE_NAME: &str = "firezone_client_ipc";
const SERVICE_TYPE: ServiceType = ServiceType::OWN_PROCESS;

pub(crate) struct Signals {
    sigint: tokio::signal::windows::CtrlC,
}

impl Signals {
    pub(crate) fn new() -> Result<Self> {
        let sigint = tokio::signal::windows::ctrl_c()?;
        Ok(Self { sigint })
    }

    pub(crate) fn poll(&mut self, cx: &mut Context) -> Poll<SignalKind> {
        if self.sigint.poll_recv(cx).is_ready() {
            return Poll::Ready(SignalKind::Interrupt);
        }
        Poll::Pending
    }
}

// The return value is useful on Linux
#[allow(clippy::unnecessary_wraps)]
pub(crate) fn check_token_permissions(_path: &Path) -> Result<()> {
    // TODO: For Headless Client, make sure the token is only readable by admin / our service user on Windows
    Ok(())
}

pub(crate) fn default_token_path() -> std::path::PathBuf {
    // TODO: For Headless Client, system-wide default token path for Windows
    PathBuf::from("token.txt")
}

/// Cross-platform entry point for systemd / Windows services
///
/// Linux uses the CLI args from here, Windows does not
pub(crate) fn run_ipc_service(_cli: CliCommon) -> Result<()> {
    windows_service::service_dispatcher::start(SERVICE_NAME, ffi_service_run).context("windows_service::service_dispatcher failed. This isn't running in an interactive terminal, right?")
}

// Generates `ffi_service_run` from `service_run`
windows_service::define_windows_service!(ffi_service_run, windows_service_run);

fn windows_service_run(arguments: Vec<OsString>) {
    if let Err(error) = fallible_windows_service_run(arguments) {
        tracing::error!(?error, "fallible_windows_service_run returned an error");
    }
}

// Most of the Windows-specific service stuff should go here
//
// The arguments don't seem to match the ones passed to the main thread at all.
fn fallible_windows_service_run(arguments: Vec<OsString>) -> Result<()> {
    let log_path =
        crate::known_dirs::ipc_service_logs().context("Can't compute IPC service logs dir")?;
    std::fs::create_dir_all(&log_path)?;
    let (layer, _handle) = file_logger::layer(&log_path);
    let filter = EnvFilter::from_str(SERVICE_RUST_LOG)?;
    let subscriber = Registry::default().with(layer.with_filter(filter));
    set_global_default(subscriber)?;
    tracing::info!(git_version = crate::GIT_VERSION);
    tracing::info!(?arguments, "fallible_windows_service_run");

    let rt = tokio::runtime::Runtime::new()?;
    let (shutdown_tx, mut shutdown_rx) = mpsc::channel(1);

    let event_handler = move |control_event| -> ServiceControlHandlerResult {
        tracing::debug!(?control_event);
        match control_event {
            // TODO
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            ServiceControl::Stop => {
                tracing::info!("Got stop signal from service controller");
                shutdown_tx.blocking_send(()).unwrap();
                ServiceControlHandlerResult::NoError
            }
            ServiceControl::UserEvent(_) => ServiceControlHandlerResult::NoError,
            ServiceControl::Continue
            | ServiceControl::NetBindAdd
            | ServiceControl::NetBindDisable
            | ServiceControl::NetBindEnable
            | ServiceControl::NetBindRemove
            | ServiceControl::ParamChange
            | ServiceControl::Pause
            | ServiceControl::Preshutdown
            | ServiceControl::Shutdown
            | ServiceControl::HardwareProfileChange(_)
            | ServiceControl::PowerEvent(_)
            | ServiceControl::SessionChange(_)
            | ServiceControl::TimeChange
            | ServiceControl::TriggerEvent => ServiceControlHandlerResult::NotImplemented,
            _ => ServiceControlHandlerResult::NotImplemented,
        }
    };

    // Fixes <https://github.com/firezone/firezone/issues/4899>,
    // DNS rules persisting after reboot
    connlib_shared::deactivate_dns_control().ok();

    // Tell Windows that we're running (equivalent to sd_notify in systemd)
    let status_handle = service_control_handler::register(SERVICE_NAME, event_handler)?;
    status_handle.set_service_status(ServiceStatus {
        service_type: SERVICE_TYPE,
        current_state: ServiceState::Running,
        controls_accepted: ServiceControlAccept::STOP,
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    })?;

    let mut ipc_service = pin!(super::ipc_listen());
    let result = rt.block_on(async {
        std::future::poll_fn(|cx| {
            match shutdown_rx.poll_recv(cx) {
                Poll::Ready(Some(())) => {
                    tracing::info!("Got shutdown signal");
                    return Poll::Ready(Ok(()));
                }
                Poll::Ready(None) => {
                    return Poll::Ready(Err(anyhow!(
                        "shutdown channel unexpectedly dropped, shutting down"
                    )))
                }
                Poll::Pending => {}
            }

            match ipc_service.as_mut().poll(cx) {
                Poll::Ready(Ok(())) => {
                    return Poll::Ready(Err(anyhow!("Impossible, ipc_listen can't return Ok")))
                }
                Poll::Ready(Err(error)) => {
                    return Poll::Ready(Err(error.context("ipc_listen failed")))
                }
                Poll::Pending => {}
            }

            Poll::Pending
        })
        .await
    });

    // Tell Windows that we're stopping
    status_handle.set_service_status(ServiceStatus {
        service_type: SERVICE_TYPE,
        current_state: ServiceState::Stopped,
        controls_accepted: ServiceControlAccept::empty(),
        exit_code: ServiceExitCode::Win32(if result.is_ok() { 0 } else { 1 }),
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    })?;
    result
}

pub(crate) struct IpcServer {
    // On Linux this has some fields
}

/// Opaque wrapper around platform-specific IPC stream
pub(crate) type IpcStream = named_pipe::NamedPipeServer;

impl IpcServer {
    /// Platform-specific setup
    ///
    /// This is async on Linux
    #[allow(clippy::unused_async)]
    pub(crate) async fn new() -> Result<Self> {
        setup_before_connlib()?;
        Ok(Self {})
    }

    pub(crate) async fn next_client(&mut self) -> Result<IpcStream> {
        let server = create_pipe_server()?;
        tracing::info!("Listening for GUI to connect over IPC...");
        server
            .connect()
            .await
            .context("Couldn't accept IPC connection from GUI")?;
        Ok(server)
    }
}

fn create_pipe_server() -> Result<named_pipe::NamedPipeServer> {
    let mut server_options = named_pipe::ServerOptions::new();
    server_options.first_pipe_instance(true);

    // This will allow non-admin clients to connect to us even though we're running with privilege
    let mut sd = WinSec::SECURITY_DESCRIPTOR::default();
    let psd = WinSec::PSECURITY_DESCRIPTOR(&mut sd as *mut _ as *mut c_void);
    // SAFETY: Unsafe needed to call Win32 API. There shouldn't be any threading or lifetime problems, because we only pass pointers to our local vars to Win32, and Win32 shouldn't sae them anywhere.
    unsafe {
        // ChatGPT pointed me to these functions
        WinSec::InitializeSecurityDescriptor(
            psd,
            windows::Win32::System::SystemServices::SECURITY_DESCRIPTOR_REVISION,
        )
        .context("InitializeSecurityDescriptor failed")?;
        WinSec::SetSecurityDescriptorDacl(psd, true, None, false)
            .context("SetSecurityDescriptorDacl failed")?;
    }

    let mut sa = WinSec::SECURITY_ATTRIBUTES {
        nLength: 0,
        lpSecurityDescriptor: psd.0,
        bInheritHandle: false.into(),
    };
    sa.nLength = std::mem::size_of_val(&sa)
        .try_into()
        .context("Size of SECURITY_ATTRIBUTES struct is not right")?;

    let sa_ptr = &mut sa as *mut _ as *mut c_void;
    // SAFETY: Unsafe needed to call Win32 API. We only pass pointers to local vars, and Win32 shouldn't store them, so there shouldn't be any threading of lifetime problems.
    let server = unsafe { server_options.create_with_security_attributes_raw(pipe_path(), sa_ptr) }
        .context("Failed to listen on named pipe")?;
    Ok(server)
}

/// Named pipe for IPC between GUI client and IPC service
pub fn pipe_path() -> String {
    named_pipe_path(&format!("{BUNDLE_ID}.ipc_service"))
}

pub fn system_resolvers() -> Result<Vec<IpAddr>> {
    let resolvers = ipconfig::get_adapters()?
        .iter()
        .flat_map(|adapter| adapter.dns_servers())
        .filter(|ip| match ip {
            IpAddr::V4(_) => true,
            // Filter out bogus DNS resolvers on my dev laptop that start with fec0:
            IpAddr::V6(ip) => !ip.octets().starts_with(&[0xfe, 0xc0]),
        })
        .copied()
        .collect();
    // This is private, so keep it at `debug` or `trace`
    tracing::debug!(?resolvers);
    Ok(resolvers)
}

/// Returns a valid name for a Windows named pipe
///
/// # Arguments
///
/// * `id` - BUNDLE_ID, e.g. `dev.firezone.client`
pub fn named_pipe_path(id: &str) -> String {
    format!(r"\\.\pipe\{}", id)
}

pub(crate) fn setup_before_connlib() -> Result<()> {
    wintun_install::ensure_dll()?;
    Ok(())
}

pub(crate) struct InterfaceManager {
    iface_idx: u32,
    routes: HashSet<IpNetwork>,
}

impl Drop for InterfaceManager {
    fn drop(&mut self) {
        todo!()
    }
}

impl InterfaceManager {
    pub(crate) fn new() -> Result<Self> {
        todo!()
    }

    #[tracing::instrument(level = "trace", skip(self))]
    pub(crate) async fn on_set_interface_config(
        &mut self,
        ipv4: Ipv4Addr,
        ipv6: Ipv6Addr,
        dns_config: Vec<IpAddr>,
    ) -> Result<()> {
        todo!()
    }

    #[tracing::instrument(level = "trace", skip(self))]
    pub(crate) async fn on_update_routes(
        &mut self,
        ipv4: Vec<Cidrv4>,
        ipv6: Vec<Cidrv6>,
    ) -> Result<()> {
        let new_routes: HashSet<IpNetwork> = ipv4
            .into_iter()
            .map(|x| Into::<Ipv4Network>::into(x).into())
            .chain(
                ipv6.into_iter()
                    .map(|x| Into::<Ipv6Network>::into(x).into()),
            )
            .collect();
        if new_routes == self.routes {
            return Ok(());
        }

        for new_route in new_routes.difference(&self.routes) {
            self.add_route(*new_route)?;
        }

        for old_route in self.routes.difference(&new_routes) {
            self.remove_route(*old_route)?;
        }

        // TODO: Might be calling this more often than it needs
        connlib_shared::windows::dns::flush().expect("Should be able to flush Windows' DNS cache");
        self.routes = new_routes;
        Ok(())
    }

    // It's okay if this blocks until the route is added in the OS.
    fn add_route(&self, route: IpNetwork) -> Result<()> {
        const DUPLICATE_ERR: u32 = 0x80071392;
        let entry = self.forward_entry(route);

        // SAFETY: Windows shouldn't store the reference anywhere, it's just a way to pass lots of arguments at once. And no other thread sees this variable.
        match unsafe { CreateIpForwardEntry2(&entry) }.ok() {
            Ok(()) => Ok(()),
            Err(e) if e.code().0 as u32 == DUPLICATE_ERR => {
                tracing::debug!(%route, "Failed to add duplicate route, ignoring");
                Ok(())
            }
            Err(e) => Err(e.into()),
        }
    }

    // It's okay if this blocks until the route is removed in the OS.
    fn remove_route(&self, route: IpNetwork) -> Result<()> {
        let entry = self.forward_entry(route);

        // SAFETY: Windows shouldn't store the reference anywhere, it's just a way to pass lots of arguments at once. And no other thread sees this variable.
        unsafe { DeleteIpForwardEntry2(&entry) }.ok()?;
        Ok(())
    }

    fn forward_entry(&self, route: IpNetwork) -> MIB_IPFORWARD_ROW2 {
        let mut row = MIB_IPFORWARD_ROW2::default();
        // SAFETY: Windows shouldn't store the reference anywhere, it's just setting defaults
        unsafe { InitializeIpForwardEntry(&mut row) };

        let prefix = &mut row.DestinationPrefix;
        match route {
            IpNetwork::V4(x) => {
                prefix.PrefixLength = x.netmask();
                prefix.Prefix.Ipv4 = SocketAddrV4::new(x.network_address(), 0).into();
            }
            IpNetwork::V6(x) => {
                prefix.PrefixLength = x.netmask();
                prefix.Prefix.Ipv6 = SocketAddrV6::new(x.network_address(), 0, 0, 0).into();
            }
        }

        row.InterfaceIndex = self.iface_idx;
        row.Metric = 0;

        row
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn named_pipe_path() {
        assert_eq!(
            super::named_pipe_path("dev.firezone.client"),
            r"\\.\pipe\dev.firezone.client"
        );
    }
}
