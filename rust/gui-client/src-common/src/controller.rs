use crate::{
    auth, deep_link,
    errors::Error,
    ipc, logging,
    settings::{self, AdvancedSettings},
    system_tray::{self, Event as TrayMenuEvent},
    updates,
};
use anyhow::{anyhow, Context, Result};
use connlib_model::ResourceView;
use firezone_bin_shared::{new_dns_notifier, new_network_notifier};
use firezone_headless_client::{
    IpcClientMsg::{self, SetDisabledResources},
    IpcServerMsg, IpcServiceError, LogFilterReloader,
};
use firezone_logging::{anyhow_dyn_err, std_dyn_err};
use firezone_telemetry::Telemetry;
use secrecy::{ExposeSecret as _, SecretString};
use std::{collections::BTreeSet, ops::ControlFlow, path::PathBuf, time::Instant};
use tokio::sync::{mpsc, oneshot};
use url::Url;

use ControllerRequest as Req;

mod ran_before;

pub type CtlrTx = mpsc::Sender<ControllerRequest>;

pub struct Controller<'a, I: GuiIntegration> {
    /// Debugging-only settings like API URL, auth URL, log filter
    advanced_settings: AdvancedSettings,
    // Sign-in state with the portal / deep links
    auth: auth::Auth,
    clear_logs_callback: Option<oneshot::Sender<Result<(), String>>>,
    ctlr_tx: CtlrTx,
    ipc_client: ipc::Client,
    ipc_rx: mpsc::Receiver<ipc::Event>,
    integration: I,
    log_filter_reloader: LogFilterReloader,
    /// A release that's ready to download
    release: Option<updates::Release>,
    rx: mpsc::Receiver<ControllerRequest>,
    status: Status,
    telemetry: &'a mut Telemetry,
    updates_rx: mpsc::Receiver<Option<updates::Notification>>,
    uptime: crate::uptime::Tracker,
}

pub struct Builder<'a, I: GuiIntegration> {
    pub advanced_settings: AdvancedSettings,
    pub ctlr_tx: CtlrTx,
    pub integration: I,
    pub log_filter_reloader: LogFilterReloader,
    pub rx: mpsc::Receiver<ControllerRequest>,
    pub telemetry: &'a mut Telemetry,
    pub updates_rx: mpsc::Receiver<Option<updates::Notification>>,
}

impl<'a, I: GuiIntegration> Builder<'a, I> {
    pub async fn build(self) -> Result<Controller<'a, I>> {
        let Builder {
            advanced_settings,
            ctlr_tx,
            integration,
            log_filter_reloader,
            rx,
            telemetry,
            updates_rx,
        } = self;

        let (ipc_tx, ipc_rx) = mpsc::channel(1);
        let ipc_client = ipc::Client::new(ipc_tx).await?;

        Ok(Controller {
            advanced_settings,
            auth: auth::Auth::new()?,
            clear_logs_callback: None,
            ctlr_tx,
            ipc_client,
            ipc_rx,
            integration,
            log_filter_reloader,
            release: None,
            rx,
            status: Default::default(),
            telemetry,
            updates_rx,
            uptime: Default::default(),
        })
    }
}

pub trait GuiIntegration {
    fn set_welcome_window_visible(&self, visible: bool) -> Result<()>;

    /// Also opens non-URLs
    fn open_url<P: AsRef<str>>(&self, url: P) -> Result<()>;

    fn set_tray_icon(&mut self, icon: system_tray::Icon) -> Result<()>;
    fn set_tray_menu(&mut self, app_state: system_tray::AppState) -> Result<()>;
    fn show_notification(&self, title: &str, body: &str) -> Result<()>;
    fn show_update_notification(&self, ctlr_tx: CtlrTx, title: &str, url: url::Url) -> Result<()>;

    /// Shows a window that the system tray knows about, e.g. not Welcome.
    fn show_window(&self, window: system_tray::Window) -> Result<()>;
}

// Allow dead code because `UpdateNotificationClicked` doesn't work on Linux yet
pub enum ControllerRequest {
    /// The GUI wants us to use these settings in-memory, they've already been saved to disk
    ApplySettings(Box<AdvancedSettings>),
    /// Clear the GUI's logs and await the IPC service to clear its logs
    ClearLogs(oneshot::Sender<Result<(), String>>),
    /// The same as the arguments to `client::logging::export_logs_to`
    ExportLogs {
        path: PathBuf,
        stem: PathBuf,
    },
    Fail(Failure),
    GetAdvancedSettings(oneshot::Sender<AdvancedSettings>),
    SchemeRequest(SecretString),
    SignIn,
    SystemTrayMenu(TrayMenuEvent),
    UpdateNotificationClicked(Url),
}

// The failure flags are all mutually exclusive
// TODO: I can't figure out from the `clap` docs how to do this:
// `app --fail-on-purpose crash-in-wintun-worker`
// So the failure should be an `Option<Enum>` but _not_ a subcommand.
// You can only have one subcommand per container, I've tried
#[derive(Debug)]
pub enum Failure {
    Crash,
    Error,
    Panic,
}

pub enum Status {
    /// Firezone is disconnected.
    Disconnected,
    /// At least one connection request has failed, due to failing to reach the Portal, and we are waiting for a network change before we try again
    RetryingConnection {
        /// The token to log in to the Portal, for retrying the connection request.
        token: SecretString,
    },
    Quitting, // The user asked to quit and we're waiting for the tunnel daemon to gracefully disconnect so we can flush telemetry.
    /// Firezone is ready to use.
    TunnelReady {
        resources: Vec<ResourceView>,
    },
    /// Firezone is signing in to the Portal.
    WaitingForPortal {
        /// The instant when we sent our most recent connect request.
        start_instant: Instant,
        /// The token to log in to the Portal, in case we need to retry the connection request.
        token: SecretString,
    },
    /// Firezone has connected to the Portal and is raising the tunnel.
    WaitingForTunnel {
        /// The instant when we sent our most recent connect request.
        start_instant: Instant,
    },
}

impl Default for Status {
    fn default() -> Self {
        Self::Disconnected
    }
}

impl Status {
    /// True if we want to hear about DNS and network changes.
    fn needs_network_changes(&self) -> bool {
        match self {
            Status::Disconnected | Status::RetryingConnection { .. } => false,
            Status::Quitting => false,
            Status::TunnelReady { .. }
            | Status::WaitingForPortal { .. }
            | Status::WaitingForTunnel { .. } => true,
        }
    }

    /// True if we should react to `OnUpdateResources`
    fn needs_resource_updates(&self) -> bool {
        match self {
            Status::Disconnected
            | Status::RetryingConnection { .. }
            | Status::Quitting
            | Status::WaitingForPortal { .. } => false,
            Status::TunnelReady { .. } | Status::WaitingForTunnel { .. } => true,
        }
    }

    fn internet_resource(&self) -> Option<ResourceView> {
        #[expect(clippy::wildcard_enum_match_arm)]
        match self {
            Status::TunnelReady { resources } => {
                resources.iter().find(|r| r.is_internet_resource()).cloned()
            }
            _ => None,
        }
    }
}

impl<'a, I: GuiIntegration> Controller<'a, I> {
    pub async fn main_loop(mut self) -> Result<(), Error> {
        let account_slug = self.auth.session().map(|s| s.account_slug.to_owned());

        // Start telemetry
        {
            const VERSION: &str = env!("CARGO_PKG_VERSION");

            let environment = self.advanced_settings.api_url.to_string();
            self.telemetry
                .start(&environment, VERSION, firezone_telemetry::GUI_DSN);
            if let Some(account_slug) = account_slug.clone() {
                self.telemetry.set_account_slug(account_slug);
            }

            self.ipc_client
                .send_msg(&IpcClientMsg::StartTelemetry {
                    environment,
                    version: VERSION.to_owned(),
                    account_slug,
                })
                .await?;
        }

        if let Some(token) = self
            .auth
            .token()
            .context("Failed to load token from disk during app start")?
        {
            self.start_session(token).await?;
        } else {
            tracing::info!("No token / actor_name on disk, starting in signed-out state");
            self.refresh_system_tray_menu()?;
        }

        if !ran_before::get().await? {
            self.integration.set_welcome_window_visible(true)?;
        }

        let tokio_handle = tokio::runtime::Handle::current();
        let dns_control_method = Default::default();

        let mut dns_notifier = new_dns_notifier(tokio_handle.clone(), dns_control_method).await?;
        let mut network_notifier =
            new_network_notifier(tokio_handle.clone(), dns_control_method).await?;
        drop(tokio_handle);

        loop {
            // TODO: Add `ControllerRequest::NetworkChange` and `DnsChange` and replace
            // `tokio::select!` with a `poll_*` function
            tokio::select! {
                result = network_notifier.notified() => {
                    result?;
                    if self.status.needs_network_changes() {
                        tracing::debug!("Internet up/down changed, calling `Session::reset`");
                        self.ipc_client.reset().await?
                    }
                    self.try_retry_connection().await?
                }
                result = dns_notifier.notified() => {
                    result?;
                    if self.status.needs_network_changes() {
                        let resolvers = firezone_headless_client::dns_control::system_resolvers_for_gui()?;
                        tracing::debug!(?resolvers, "New DNS resolvers, calling `Session::set_dns`");
                        self.ipc_client.set_dns(resolvers).await?;
                    }
                    self.try_retry_connection().await?
                }
                event = self.ipc_rx.recv() => {
                    let event = event.context("IPC task stopped")?;
                    if let ControlFlow::Break(()) = self.handle_ipc_event(event).await? {
                        break;
                    }
                }
                req = self.rx.recv() => {
                    let Some(req) = req else {
                        tracing::warn!("Controller channel closed, breaking main loop.");
                        break;
                    };

                    #[expect(clippy::wildcard_enum_match_arm)]
                    match req {
                        // SAFETY: Crashing is unsafe
                        Req::Fail(Failure::Crash) => {
                            tracing::error!("Crashing on purpose");
                            unsafe { sadness_generator::raise_segfault() }
                        },
                        Req::Fail(Failure::Error) => Err(anyhow!("Test error"))?,
                        Req::Fail(Failure::Panic) => panic!("Test panic"),
                        Req::SystemTrayMenu(TrayMenuEvent::Quit) => {
                            tracing::info!("User clicked Quit in the menu");
                            self.status = Status::Quitting;
                            self.ipc_client.send_msg(&IpcClientMsg::Disconnect).await?;
                            self.refresh_system_tray_menu()?;
                        }
                        // TODO: Should we really skip cleanup if a request fails?
                        req => self.handle_request(req).await?,
                    }
                }
                notification = self.updates_rx.recv() => self.handle_update_notification(notification.context("Update checker task stopped")?)?,
            }
            // Code down here may not run because the `select` sometimes `continue`s.
        }

        tracing::debug!("Closing...");

        if let Err(error) = dns_notifier.close() {
            tracing::error!(error = anyhow_dyn_err(&error), "dns_notifier");
        }
        if let Err(error) = network_notifier.close() {
            tracing::error!(error = anyhow_dyn_err(&error), "network_notifier");
        }

        if let Err(error) = self.ipc_client.disconnect_from_ipc().await {
            tracing::error!(error = anyhow_dyn_err(&error), "ipc_client");
        }

        // Don't close telemetry here, `run` will close it.

        Ok(())
    }

    async fn start_session(&mut self, token: SecretString) -> Result<(), Error> {
        match self.status {
            Status::Disconnected | Status::RetryingConnection { .. } => {}
            Status::Quitting => Err(anyhow!("Can't connect to Firezone, we're quitting"))?,
            Status::TunnelReady { .. } => Err(anyhow!(
                "Can't connect to Firezone, we're already connected."
            ))?,
            Status::WaitingForPortal { .. } | Status::WaitingForTunnel { .. } => Err(anyhow!(
                "Can't connect to Firezone, we're already connecting."
            ))?,
        }

        let api_url = self.advanced_settings.api_url.clone();
        tracing::info!(api_url = api_url.to_string(), "Starting connlib...");

        // Count the start instant from before we connect
        let start_instant = Instant::now();
        self.ipc_client
            .connect_to_firezone(api_url.as_str(), token.expose_secret().clone().into())
            .await?;
        // Change the status after we begin connecting
        self.status = Status::WaitingForPortal {
            start_instant,
            token,
        };
        self.refresh_system_tray_menu()?;
        Ok(())
    }

    async fn handle_deep_link(&mut self, url: &SecretString) -> Result<(), Error> {
        let auth_response =
            deep_link::parse_auth_callback(url).context("Couldn't parse scheme request")?;

        tracing::info!("Received deep link over IPC");
        // Uses `std::fs`
        let token = self
            .auth
            .handle_response(auth_response)
            .context("Couldn't handle auth response")?;
        self.start_session(token).await?;
        Ok(())
    }

    async fn handle_request(&mut self, req: ControllerRequest) -> Result<(), Error> {
        match req {
            Req::ApplySettings(settings) => {
                let filter = firezone_logging::try_filter(&self.advanced_settings.log_filter)
                        .context("Couldn't parse new log filter directives")?;
                self.advanced_settings = *settings;
                self.log_filter_reloader
                    .reload(filter)
                    .context("Couldn't reload log filter")?;
                self.ipc_client.send_msg(&IpcClientMsg::ReloadLogFilter).await?;
                tracing::debug!(
                    "Applied new settings. Log level will take effect immediately."
                );
                // Refresh the menu in case the favorites were reset.
                self.refresh_system_tray_menu()?;
            }
            Req::ClearLogs(completion_tx) => {
                if self.clear_logs_callback.is_some() {
                    tracing::error!("Can't clear logs, we're already waiting on another log-clearing operation");
                }
                if let Err(error) = logging::clear_gui_logs().await {
                    tracing::error!(error = anyhow_dyn_err(&error), "Failed to clear GUI logs");
                }
                self.ipc_client.send_msg(&IpcClientMsg::ClearLogs).await?;
                self.clear_logs_callback = Some(completion_tx);
            }
            Req::ExportLogs { path, stem } => logging::export_logs_to(path, stem)
                .await
                .context("Failed to export logs to zip")?,
            Req::Fail(_) => Err(anyhow!(
                "Impossible error: `Fail` should be handled before this"
            ))?,
            Req::GetAdvancedSettings(tx) => {
                tx.send(self.advanced_settings.clone()).ok();
            }
            Req::SchemeRequest(url) => {
                if let Err(error) = self.handle_deep_link(&url).await {
                    tracing::error!(error = std_dyn_err(&error), "`handle_deep_link` failed");
                }
            }
            Req::SignIn | Req::SystemTrayMenu(TrayMenuEvent::SignIn) => {
                if let Some(req) = self
                    .auth
                    .start_sign_in()
                    .context("Couldn't start sign-in flow")?
                {
                    let url = req.to_url(&self.advanced_settings.auth_base_url);
                    self.refresh_system_tray_menu()?;
                    self.integration.open_url(url.expose_secret())
                        .context("Couldn't open auth page")?;
                    self.integration.set_welcome_window_visible(false)?;
                }
            }
            Req::SystemTrayMenu(TrayMenuEvent::AddFavorite(resource_id)) => {
                self.advanced_settings.favorite_resources.insert(resource_id);
                self.refresh_favorite_resources().await?;
            },
            Req::SystemTrayMenu(TrayMenuEvent::AdminPortal) => self.integration.open_url(
                &self.advanced_settings.auth_base_url,
            )
            .context("Couldn't open auth page")?,
            Req::SystemTrayMenu(TrayMenuEvent::Copy(s)) => arboard::Clipboard::new()
                .context("Couldn't access clipboard")?
                .set_text(s)
                .context("Couldn't copy resource URL or other text to clipboard")?,
            Req::SystemTrayMenu(TrayMenuEvent::CancelSignIn) => {
                match &self.status {
                    Status::Disconnected | Status::RetryingConnection { .. } | Status::WaitingForPortal { .. } => {
                        tracing::info!("Calling `sign_out` to cancel sign-in");
                        self.sign_out().await?;
                    }
                    Status::Quitting => tracing::error!("Can't cancel sign-in while already quitting"),
                    Status::TunnelReady{..} => tracing::error!("Can't cancel sign-in, the tunnel is already up. This is a logic error in the code."),
                    Status::WaitingForTunnel { .. } => {
                        tracing::debug!(
                            "Connlib is already raising the tunnel, calling `sign_out` anyway"
                        );
                        self.sign_out().await?;
                    }
                }
            }
            Req::SystemTrayMenu(TrayMenuEvent::RemoveFavorite(resource_id)) => {
                self.advanced_settings.favorite_resources.remove(&resource_id);
                self.refresh_favorite_resources().await?;
            }
            Req::SystemTrayMenu(TrayMenuEvent::RetryPortalConnection) => self.try_retry_connection().await?,
            Req::SystemTrayMenu(TrayMenuEvent::EnableInternetResource) => {
                self.advanced_settings.internet_resource_enabled = Some(true);
                self.update_disabled_resources().await?;
            }
            Req::SystemTrayMenu(TrayMenuEvent::DisableInternetResource) => {
                self.advanced_settings.internet_resource_enabled = Some(false);
                self.update_disabled_resources().await?;
            }
            Req::SystemTrayMenu(TrayMenuEvent::ShowWindow(window)) => {
                self.integration.show_window(window)?;
                // When the About or Settings windows are hidden / shown, log the
                // run ID and uptime. This makes it easy to check client stability on
                // dev or test systems without parsing the whole log file.
                let uptime_info = self.uptime.info();
                tracing::debug!(
                    uptime_s = uptime_info.uptime.as_secs(),
                    run_id = uptime_info.run_id.to_string(),
                    "Uptime info"
                );
            }
            Req::SystemTrayMenu(TrayMenuEvent::SignOut) => {
                tracing::info!("User asked to sign out");
                self.sign_out().await?;
            }
            Req::SystemTrayMenu(TrayMenuEvent::Url(url)) => {
                self.integration.open_url(&url)
                    .context("Couldn't open URL from system tray")?
            }
            Req::SystemTrayMenu(TrayMenuEvent::Quit) => Err(anyhow!(
                "Impossible error: `Quit` should be handled before this"
            ))?,
            Req::UpdateNotificationClicked(download_url) => {
                tracing::info!("UpdateNotificationClicked in run_controller!");
                self.integration.open_url(&download_url)
                    .context("Couldn't open update page")?;
            }
        }
        Ok(())
    }

    async fn handle_ipc_event(&mut self, event: ipc::Event) -> Result<ControlFlow<()>, Error> {
        match event {
            ipc::Event::Message(msg) => match self.handle_ipc_msg(msg).await {
                Ok(flow) => Ok(flow),
                // Handles <https://github.com/firezone/firezone/issues/6547> more gracefully so we can still export logs even if we crashed right after sign-in
                Err(Error::ConnectToFirezoneFailed(error)) => {
                    tracing::error!("Failed to connect to Firezone: {error}");
                    self.sign_out().await?;
                    Ok(ControlFlow::Continue(()))
                }
                Err(error) => Err(error)?,
            },
            ipc::Event::ReadFailed(error) => {
                // IPC errors are always fatal
                tracing::error!(error = anyhow_dyn_err(&error), "IPC read failure");
                Err(Error::IpcRead)?
            }
            ipc::Event::Closed => Err(Error::IpcClosed)?,
        }
    }

    async fn handle_ipc_msg(&mut self, msg: IpcServerMsg) -> Result<ControlFlow<()>, Error> {
        match msg {
            IpcServerMsg::ClearedLogs(result) => {
                let Some(tx) = self.clear_logs_callback.take() else {
                    return Err(Error::Other(anyhow!("Can't handle `IpcClearedLogs` when there's no callback waiting for a `ClearLogs` result")));
                };
                tx.send(result).map_err(|_| {
                    Error::Other(anyhow!("Couldn't send `ClearLogs` result to Tauri task"))
                })?;
            }
            IpcServerMsg::ConnectResult(result) => {
                return self
                    .handle_connect_result(result)
                    .await
                    .map(|_| ControlFlow::Continue(()))
            }
            IpcServerMsg::DisconnectedGracefully => {
                if let Status::Quitting = self.status {
                    return Ok(ControlFlow::Break(()));
                }
            }
            IpcServerMsg::OnDisconnect {
                error_msg,
                is_authentication_error,
            } => {
                self.sign_out().await?;
                if is_authentication_error {
                    tracing::info!(?error_msg, "Auth error");
                    self.integration.show_notification(
                        "Firezone disconnected",
                        "To access resources, sign in again.",
                    )?;
                } else {
                    tracing::error!("Connlib disconnected: {error_msg}");
                    native_dialog::MessageDialog::new()
                        .set_title("Firezone Error")
                        .set_text(&error_msg)
                        .set_type(native_dialog::MessageType::Error)
                        .show_alert()
                        .context("Couldn't show Disconnected alert")?;
                }
            }
            IpcServerMsg::OnUpdateResources(resources) => {
                if !self.status.needs_resource_updates() {
                    return Ok(ControlFlow::Continue(()));
                }
                tracing::debug!(len = resources.len(), "Got new Resources");
                self.status = Status::TunnelReady { resources };
                if let Err(error) = self.refresh_system_tray_menu() {
                    tracing::error!(error = anyhow_dyn_err(&error), "Failed to refresh menu");
                }

                self.update_disabled_resources().await?;
            }
            IpcServerMsg::TerminatingGracefully => {
                tracing::info!("Caught TerminatingGracefully");
                self.integration
                    .set_tray_icon(system_tray::icon_terminating())
                    .ok();
                Err(Error::IpcServiceTerminating)?
            }
            IpcServerMsg::TunnelReady => {
                if self.auth.session().is_none() {
                    // This could maybe happen if the user cancels the sign-in
                    // before it completes. This is because the state machine
                    // between the GUI, the IPC service, and connlib isn't  perfectly synced.
                    tracing::error!("Got `TunnelReady` while signed out");
                    return Ok(ControlFlow::Continue(()));
                }
                if let Status::WaitingForTunnel { start_instant } = self.status {
                    tracing::info!(elapsed = ?start_instant.elapsed(), "Tunnel ready");
                    self.status = Status::TunnelReady { resources: vec![] };
                    self.integration.show_notification(
                        "Firezone connected",
                        "You are now signed in and able to access resources.",
                    )?;
                }
                if let Err(error) = self.refresh_system_tray_menu() {
                    tracing::error!(error = anyhow_dyn_err(&error), "Failed to refresh menu");
                }
            }
        }
        Ok(ControlFlow::Continue(()))
    }

    async fn handle_connect_result(
        &mut self,
        result: Result<(), IpcServiceError>,
    ) -> Result<(), Error> {
        let (start_instant, token) = match &self.status {
            Status::Disconnected
            | Status::RetryingConnection { .. }
            | Status::TunnelReady { .. }
            | Status::WaitingForTunnel { .. } => {
                tracing::error!("Impossible logic error, received `ConnectResult` when we weren't waiting on the Portal connection.");
                return Ok(());
            }
            Status::Quitting => {
                tracing::debug!("Ignoring `ConnectResult`, we are quitting");
                return Ok(());
            }
            Status::WaitingForPortal {
                start_instant,
                token,
            } => (*start_instant, token.expose_secret().clone().into()),
        };

        match result {
            Ok(()) => {
                ran_before::set().await?;
                self.status = Status::WaitingForTunnel { start_instant };
                if let Err(error) = self.refresh_system_tray_menu() {
                    tracing::error!(error = anyhow_dyn_err(&error), "Failed to refresh menu");
                }
                Ok(())
            }
            Err(IpcServiceError::Io(error)) => {
                // This is typically something like, we don't have Internet access so we can't
                // open the PhoenixChannel's WebSocket.
                tracing::info!(
                    error,
                    "Failed to connect to Firezone Portal, will try again when the network changes"
                );
                self.status = Status::RetryingConnection { token };
                if let Err(error) = self.refresh_system_tray_menu() {
                    tracing::error!(error = anyhow_dyn_err(&error), "Failed to refresh menu");
                }
                Ok(())
            }
            Err(IpcServiceError::Other(error)) => Err(Error::ConnectToFirezoneFailed(error)),
        }
    }

    /// Set (or clear) update notification
    fn handle_update_notification(
        &mut self,
        notification: Option<updates::Notification>,
    ) -> Result<()> {
        let Some(notification) = notification else {
            self.release = None;
            self.refresh_system_tray_menu()?;
            return Ok(());
        };

        let release = notification.release;
        self.release = Some(release.clone());
        self.refresh_system_tray_menu()?;

        if notification.tell_user {
            let title = format!("Firezone {} available for download", release.version);

            // We don't need to route through the controller here either, we could
            // use the `open` crate directly instead of Tauri's wrapper
            // `tauri::api::shell::open`
            self.integration.show_update_notification(
                self.ctlr_tx.clone(),
                &title,
                release.download_url,
            )?;
        }
        Ok(())
    }

    async fn update_disabled_resources(&mut self) -> Result<()> {
        settings::save(&self.advanced_settings).await?;

        let Some(internet_resource) = self.status.internet_resource() else {
            return Ok(());
        };

        let mut disabled_resources = BTreeSet::new();

        if !self.advanced_settings.internet_resource_enabled() {
            disabled_resources.insert(internet_resource.id());
        }

        self.ipc_client
            .send_msg(&SetDisabledResources(disabled_resources))
            .await?;
        self.refresh_system_tray_menu()?;

        Ok(())
    }

    /// Saves the current settings (including favorites) to disk and refreshes the tray menu
    async fn refresh_favorite_resources(&mut self) -> Result<()> {
        settings::save(&self.advanced_settings).await?;
        self.refresh_system_tray_menu()?;
        Ok(())
    }

    /// Builds a new system tray menu and applies it to the app
    fn refresh_system_tray_menu(&mut self) -> Result<()> {
        // TODO: Refactor `Controller` and the auth module so that "Are we logged in?"
        // doesn't require such complicated control flow to answer.
        let connlib = if let Some(auth_session) = self.auth.session() {
            match &self.status {
                Status::Disconnected => {
                    tracing::error!("We have an auth session but no connlib session");
                    system_tray::ConnlibState::SignedOut
                }
                Status::Quitting => system_tray::ConnlibState::Quitting,
                Status::RetryingConnection { .. } => system_tray::ConnlibState::RetryingConnection,
                Status::TunnelReady { resources } => {
                    system_tray::ConnlibState::SignedIn(system_tray::SignedIn {
                        actor_name: auth_session.actor_name.clone(),
                        favorite_resources: self.advanced_settings.favorite_resources.clone(),
                        internet_resource_enabled: self.advanced_settings.internet_resource_enabled,
                        resources: resources.clone(),
                    })
                }
                Status::WaitingForPortal { .. } => system_tray::ConnlibState::WaitingForPortal,
                Status::WaitingForTunnel { .. } => system_tray::ConnlibState::WaitingForTunnel,
            }
        } else if self.auth.ongoing_request().is_ok() {
            // Signing in, waiting on deep link callback
            system_tray::ConnlibState::WaitingForBrowser
        } else {
            system_tray::ConnlibState::SignedOut
        };
        self.integration.set_tray_menu(system_tray::AppState {
            connlib,
            release: self.release.clone(),
        })?;
        Ok(())
    }

    /// If we're in the `RetryingConnection` state, use the token to retry the Portal connection
    async fn try_retry_connection(&mut self) -> Result<()> {
        let token = match &self.status {
            Status::Disconnected
            | Status::Quitting
            | Status::TunnelReady { .. }
            | Status::WaitingForPortal { .. }
            | Status::WaitingForTunnel { .. } => return Ok(()),
            Status::RetryingConnection { token } => token,
        };
        tracing::debug!("Retrying Portal connection...");
        self.start_session(token.expose_secret().clone().into())
            .await?;
        Ok(())
    }

    /// Deletes the auth token, stops connlib, and refreshes the tray menu
    async fn sign_out(&mut self) -> Result<()> {
        match self.status {
            Status::Quitting => return Ok(()),
            Status::Disconnected
            | Status::RetryingConnection { .. }
            | Status::TunnelReady { .. }
            | Status::WaitingForPortal { .. }
            | Status::WaitingForTunnel { .. } => {}
        }
        self.auth.sign_out()?;
        self.status = Status::Disconnected;
        tracing::debug!("disconnecting connlib");
        // This is redundant if the token is expired, in that case
        // connlib already disconnected itself.
        self.ipc_client.send_msg(&IpcClientMsg::Disconnect).await?;
        self.refresh_system_tray_menu()?;
        Ok(())
    }
}
