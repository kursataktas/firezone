use crate::CliCommon;
use anyhow::{bail, Context as _, Result};
use firezone_bin_shared::platform::DnsControlMethod;
use firezone_logging::anyhow_dyn_err;
use futures::future::{self, Either};
use std::{
    ffi::{c_void, OsString},
    mem::size_of,
    pin::pin,
    time::Duration,
};
use tokio::sync::mpsc;
use windows::Win32::{
    Foundation::{CloseHandle, HANDLE},
    Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY},
    System::Threading::{GetCurrentProcess, OpenProcessToken},
};
use windows_service::{
    service::{
        ServiceAccess, ServiceControl, ServiceControlAccept, ServiceErrorControl, ServiceExitCode,
        ServiceInfo, ServiceStartType, ServiceState, ServiceStatus, ServiceType,
    },
    service_control_handler::{self, ServiceControlHandlerResult},
    service_manager::{ServiceManager, ServiceManagerAccess},
};

const SERVICE_NAME: &str = "firezone_client_ipc";
const SERVICE_TYPE: ServiceType = ServiceType::OWN_PROCESS;

/// Returns true if the IPC service can run properly
pub(crate) fn elevation_check() -> Result<bool> {
    let token = ProcessToken::our_process()?;
    let elevated = token.is_elevated()?;
    Ok(elevated)
}

// https://stackoverflow.com/questions/8046097/how-to-check-if-a-process-has-the-administrative-rights/8196291#8196291
struct ProcessToken {
    inner: HANDLE,
}

impl ProcessToken {
    fn our_process() -> Result<Self> {
        // SAFETY: Calling C APIs is unsafe
        // `GetCurrentProcess` returns a pseudo-handle which does not need to be closed.
        // Docs say nothing about thread safety. <https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-getcurrentprocess>
        let our_proc = unsafe { GetCurrentProcess() };
        let mut inner = HANDLE::default();
        // SAFETY: We just created `inner`, and moving a `HANDLE` is safe.
        // We assume that if `OpenProcessToken` fails, we don't need to close the `HANDLE`.
        // Docs say nothing about threads or safety: <https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-openprocesstoken>
        unsafe { OpenProcessToken(our_proc, TOKEN_QUERY, &mut inner) }
            .context("`OpenProcessToken` failed")?;
        Ok(Self { inner })
    }

    fn is_elevated(&self) -> Result<bool> {
        let mut elevation = TOKEN_ELEVATION::default();
        let token_elevation_sz = u32::try_from(size_of::<TOKEN_ELEVATION>())
            .context("Failed to convert `TOKEN_ELEVATION` to u32")?;
        let mut return_size = 0u32;
        // SAFETY: Docs say nothing about threads or safety <https://learn.microsoft.com/en-us/windows/win32/api/securitybaseapi/nf-securitybaseapi-gettokeninformation>
        // The type of `elevation` varies based on the 2nd parameter, but we hard-coded that.
        // It should be fine.
        unsafe {
            GetTokenInformation(
                self.inner,
                TokenElevation,
                Some(&mut elevation as *mut _ as *mut c_void),
                token_elevation_sz,
                &mut return_size as *mut _,
            )
        }?;
        Ok(elevation.TokenIsElevated == 1)
    }
}

impl Drop for ProcessToken {
    fn drop(&mut self) {
        // SAFETY: We got `inner` from `OpenProcessToken` and didn't mutate it after that.
        // Closing a pseudo-handle is a harmless no-op, though this is a real handle.
        // <https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-getcurrentprocess>
        // > The pseudo handle need not be closed when it is no longer needed. Calling the CloseHandle function with a pseudo handle has no effect. If the pseudo handle is duplicated by DuplicateHandle, the duplicate handle must be closed.
        unsafe { CloseHandle(self.inner) }.expect("`CloseHandle` should always succeed");
        self.inner = HANDLE::default();
    }
}

pub(crate) fn install_ipc_service() -> Result<()> {
    let manager_access = ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE;
    let service_manager = ServiceManager::local_computer(None::<&str>, manager_access)?;

    let name = OsString::from("FirezoneClientIpcServiceDebug");

    // Un-install existing one first if needed
    {
        let service_access = ServiceAccess::DELETE;
        let service = service_manager.open_service(&name, service_access)?;
        service.delete()?;
    }

    let executable_path = std::env::current_exe()?;
    let service_info = ServiceInfo {
        name,
        display_name: OsString::from("Firezone Client IPC (Debug)"),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path,
        launch_arguments: vec!["run".into()],
        dependencies: vec![],
        account_name: None,
        account_password: None,
    };
    let service = service_manager.create_service(&service_info, ServiceAccess::CHANGE_CONFIG)?;
    service.set_description("Description")?;
    Ok(())
}

/// Cross-platform entry point for systemd / Windows services
///
/// Linux uses the CLI args from here, Windows does not
pub(crate) fn run_ipc_service(_cli: CliCommon) -> Result<()> {
    windows_service::service_dispatcher::start(SERVICE_NAME, ffi_service_run).context("windows_service::service_dispatcher failed. This isn't running in an interactive terminal, right?")
}

// Generates `ffi_service_run` from `service_run`
windows_service::define_windows_service!(ffi_service_run, service_run);

fn service_run(arguments: Vec<OsString>) {
    // `arguments` doesn't seem to work right when running as a Windows service
    // (even though it's meant for that) so just use the default log dir.
    let (handle, log_filter_reloader) =
        super::setup_logging(None).expect("Should be able to set up logging");
    if let Err(error) = fallible_service_run(arguments, handle, log_filter_reloader) {
        tracing::error!(
            error = anyhow_dyn_err(&error),
            "`fallible_windows_service_run` returned an error"
        );
    }
}

// Most of the Windows-specific service stuff should go here
//
// The arguments don't seem to match the ones passed to the main thread at all.
//
// If Windows stops us gracefully, this function may never return.
fn fallible_service_run(
    arguments: Vec<OsString>,
    logging_handle: firezone_logging::file::Handle,
    log_filter_reloader: crate::LogFilterReloader,
) -> Result<()> {
    tracing::info!(?arguments, "fallible_windows_service_run");
    if !elevation_check()? {
        bail!("IPC service failed its elevation check, try running as admin / root");
    }

    let rt = tokio::runtime::Runtime::new()?;
    let (shutdown_tx, shutdown_rx) = mpsc::channel(1);

    let event_handler = move |control_event| -> ServiceControlHandlerResult {
        tracing::debug!(?control_event);
        match control_event {
            // TODO
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            ServiceControl::PowerEvent(event) => {
                tracing::info!(?event, "Power event");
                ServiceControlHandlerResult::NoError
            }
            ServiceControl::Shutdown | ServiceControl::Stop => {
                if shutdown_tx.blocking_send(()).is_err() {
                    tracing::error!("Should be able to send shutdown signal");
                }
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
            | ServiceControl::HardwareProfileChange(_)
            | ServiceControl::SessionChange(_)
            | ServiceControl::TimeChange
            | ServiceControl::TriggerEvent => {
                tracing::warn!(?control_event, "Unhandled service control event");
                ServiceControlHandlerResult::NotImplemented
            }
            _ => ServiceControlHandlerResult::NotImplemented,
        }
    };

    // Tell Windows that we're running (equivalent to sd_notify in systemd)
    let status_handle = service_control_handler::register(SERVICE_NAME, event_handler)?;
    status_handle.set_service_status(ServiceStatus {
        service_type: SERVICE_TYPE,
        current_state: ServiceState::Running,
        controls_accepted: ServiceControlAccept::POWER_EVENT
            | ServiceControlAccept::SHUTDOWN
            | ServiceControlAccept::STOP,
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    })?;

    // Add new features in `service_run_async` if possible.
    // We don't want to bail out of `fallible_service_run` and forget to tell
    // Windows that we're shutting down.
    let result = rt.block_on(service_run_async(&log_filter_reloader, shutdown_rx));
    if let Err(error) = &result {
        tracing::error!(error = anyhow_dyn_err(error));
    }

    // Drop the logging handle so it flushes the logs before we let Windows kill our process.
    // There is no obvious and elegant way to do this, since the logging and `ServiceState`
    // changes are interleaved, not nested:
    // - Start logging
    // - ServiceState::Running
    // - Stop logging
    // - ServiceState::Stopped
    std::mem::drop(logging_handle);

    // Tell Windows that we're stopping
    // Per Windows docs, this will cause Windows to kill our process eventually.
    status_handle
        .set_service_status(ServiceStatus {
            service_type: SERVICE_TYPE,
            current_state: ServiceState::Stopped,
            controls_accepted: ServiceControlAccept::empty(),
            exit_code: ServiceExitCode::Win32(if result.is_ok() { 0 } else { 1 }),
            checkpoint: 0,
            wait_hint: Duration::default(),
            process_id: None,
        })
        .context("Should be able to tell Windows we're stopping")?;
    // Generally unreachable. Windows typically kills the process first,
    // but doesn't guarantee it.
    Ok(())
}

/// The main loop for the Windows service
///
/// This is split off from other functions because we don't want to accidentally
/// bail out of a fallible function and not tell Windows that we're stopping
/// the service. So it's okay to bail out of `service_run_async`, but not
/// out of its caller.
///
/// Logging must already be set up before calling this.
async fn service_run_async(
    log_filter_reloader: &crate::LogFilterReloader,
    mut shutdown_rx: mpsc::Receiver<()>,
) -> Result<()> {
    // Useless - Windows will never send us Ctrl+C when running as a service
    // This just keeps the signatures simpler
    let mut signals = crate::signals::Terminate::new()?;
    let listen_fut = pin!(super::ipc_listen(
        DnsControlMethod::Nrpt,
        log_filter_reloader,
        &mut signals
    ));
    match future::select(listen_fut, pin!(shutdown_rx.recv())).await {
        Either::Left((Err(error), _)) => Err(error).context("`ipc_listen` threw an error"),
        Either::Left((Ok(()), _)) => {
            bail!("Impossible - Shouldn't catch Ctrl+C when running as a Windows service")
        }
        Either::Right((None, _)) => bail!("Shutdown channel failed"),
        Either::Right((Some(()), _)) => {
            tracing::info!("Caught shutdown signal, stopping IPC listener");
            Ok(())
        }
    }
}
