use crate::eventloop::{Eventloop, PHOENIX_TOPIC};
use anyhow::{Context, Result};
use backoff::ExponentialBackoffBuilder;
use clap::Parser;
use connlib_shared::{get_user_agent, messages::Interface, LoginUrl, StaticSecret};
use firezone_bin_shared::{
    http_health_check,
    linux::{tcp_socket_factory, udp_socket_factory},
    TunDeviceManager,
};
use firezone_tunnel::{keypair, GatewayTunnel, IPV4_PEERS, IPV6_PEERS};

use futures::channel::mpsc;
use futures::{future, StreamExt, TryFutureExt};
use phoenix_channel::PhoenixChannel;
use secrecy::{Secret, SecretString};
use std::path::Path;
use std::pin::pin;
use std::sync::Arc;
use std::{convert::Infallible, time::Duration};
use tokio::io::AsyncWriteExt;
use tokio::signal::ctrl_c;
use tracing_subscriber::layer;
use url::Url;
use uuid::Uuid;

mod eventloop;
mod messages;

const ID_PATH: &str = "/var/lib/firezone/gateway_id";

#[tokio::main]
async fn main() {
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Calling `install_default` only once per process should always succeed");

    // Enforce errors only being printed on a single line using the technique recommended in the anyhow docs:
    // https://docs.rs/anyhow/latest/anyhow/struct.Error.html#display-representations
    //
    // By default, `anyhow` prints a stacktrace when it exits.
    // That looks like a "crash" but we "just" exit with a fatal error.
    if let Err(e) = try_main().await {
        tracing::error!("{e:#}");
        std::process::exit(1);
    }
}

async fn try_main() -> Result<()> {
    let cli = Cli::parse();
    firezone_logging::setup_global_subscriber(layer::Identity::new());

    let firezone_id = get_firezone_id(cli.firezone_id).await
        .context("Couldn't read FIREZONE_ID or write it to disk: Please provide it through the env variable or provide rw access to /var/lib/firezone/")?;

    let (private_key, public_key) = keypair();
    let login = LoginUrl::gateway(
        cli.api_url,
        &SecretString::new(cli.token),
        firezone_id,
        cli.firezone_name,
        public_key.to_bytes(),
    )?;

    let task = tokio::spawn(run(login, private_key)).err_into();

    let ctrl_c = pin!(ctrl_c().map_err(anyhow::Error::new));

    tokio::spawn(http_health_check::serve(
        cli.health_check.health_check_addr,
        || true,
    ));

    match future::try_select(task, ctrl_c)
        .await
        .map_err(|e| e.factor_first().0)?
    {
        future::Either::Left((res, _)) => {
            res?;
        }
        future::Either::Right(_) => {}
    };

    Ok(())
}

async fn get_firezone_id(env_id: Option<String>) -> Result<String> {
    if let Some(id) = env_id {
        if !id.is_empty() {
            return Ok(id);
        }
    }

    if let Ok(id) = tokio::fs::read_to_string(ID_PATH).await {
        if !id.is_empty() {
            return Ok(id);
        }
    }

    let id_path = Path::new(ID_PATH);
    tokio::fs::create_dir_all(id_path.parent().unwrap()).await?;
    let mut id_file = tokio::fs::File::create(id_path).await?;
    let id = Uuid::new_v4().to_string();
    id_file.write_all(id.as_bytes()).await?;
    Ok(id)
}

async fn run(login: LoginUrl, private_key: StaticSecret) -> Result<Infallible> {
    let mut tunnel = GatewayTunnel::new(
        private_key,
        Arc::new(tcp_socket_factory),
        Arc::new(udp_socket_factory),
        Duration::from_secs(8 * 60 * 60),
    );
    let portal = PhoenixChannel::connect(
        Secret::new(login),
        get_user_agent(None, env!("CARGO_PKG_VERSION")),
        PHOENIX_TOPIC,
        (),
        ExponentialBackoffBuilder::default()
            .with_max_elapsed_time(None)
            .build(),
        Arc::new(tcp_socket_factory),
    )?;

    let (sender, receiver) = mpsc::channel::<Interface>(10);
    let mut tun_device_manager = TunDeviceManager::new(ip_packet::PACKET_SIZE)?;
    let tun = tun_device_manager.make_tun()?;
    tunnel.set_tun(Box::new(tun));

    let update_device_task = update_device_task(tun_device_manager, receiver);

    let mut eventloop = Eventloop::new(tunnel, portal, sender);
    let eventloop_task = future::poll_fn(move |cx| eventloop.poll(cx));

    let ((), result) = futures::join!(update_device_task, eventloop_task);

    result.context("Eventloop failed")?;

    unreachable!()
}

async fn update_device_task(
    mut tun_device: TunDeviceManager,
    mut receiver: mpsc::Receiver<Interface>,
) {
    while let Some(next_interface) = receiver.next().await {
        if let Err(e) = tun_device
            .set_ips(next_interface.ipv4, next_interface.ipv6)
            .await
        {
            tracing::warn!("Failed to set interface: {e:#}");
        }

        if let Err(e) = tun_device
            .set_routes(vec![IPV4_PEERS], vec![IPV6_PEERS])
            .await
        {
            tracing::warn!("Failed to set routes: {e:#}");
        };
    }
}

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Cli {
    #[arg(
        short = 'u',
        long,
        hide = true,
        env = "FIREZONE_API_URL",
        default_value = "wss://api.firezone.dev"
    )]
    api_url: Url,
    /// Token generated by the portal to authorize websocket connection.
    #[arg(env = "FIREZONE_TOKEN")]
    token: String,
    /// Friendly name to display in the UI
    #[arg(short = 'n', long, env = "FIREZONE_NAME")]
    firezone_name: Option<String>,

    #[command(flatten)]
    health_check: http_health_check::HealthCheckArgs,

    /// Identifier generated by the portal to identify and display the device.
    #[arg(short = 'i', long, env = "FIREZONE_ID")]
    pub firezone_id: Option<String>,
}
