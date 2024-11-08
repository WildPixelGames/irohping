use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use futures_lite::StreamExt;
use iroh_net::{
    endpoint::Connection,
    key::SecretKey,
    relay::{RelayMode, RelayUrl},
    Endpoint, NodeAddr, NodeId,
};
use std::{
    net::SocketAddr,
    time::{Duration, Instant},
};
use tracing::{debug, error, info};
use tracing_subscriber::EnvFilter;

pub const PING_PONG_ALPN: &[u8] = b"golem/ping-pong/0";
const TIMEOUT_DURATION: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Parser)]
struct ConnectArgs {
    #[clap(long)]
    node_id: iroh_net::NodeId,

    #[clap(long, value_parser, num_args = 1.., value_delimiter = ' ')]
    addrs: Vec<SocketAddr>,

    #[clap(long)]
    relay_url: Option<RelayUrl>,
}

#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {
    #[clap(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Clone, Debug)]
enum Commands {
    Listen,
    Ping(ConnectArgs),
}

pub fn setup_logging() {
    if std::env::var("RUST_LOG").is_err() {
        std::env::set_var("RUST_LOG", "info");
    }

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_target(false)
        .with_thread_ids(false)
        .with_file(false)
        .with_line_number(false)
        .without_time()
        .init();
}

async fn handle_connection(connection: Connection, node_id: NodeId) -> Result<()> {
    let remote_addr = connection.remote_address();
    let latency = connection.rtt().as_millis();
    let conn_type = if remote_addr.ip().is_loopback() {
        "Local"
    } else if remote_addr.port() == 443 {
        "Relay"
    } else {
        "Direct UDP"
    };

    info!("Connection from node: {}", node_id);
    info!(" - Remote address: {}", remote_addr);
    info!(" - Latency: {}ms", latency);
    info!(" - Connection type: {}", conn_type);
    debug!(" - Connection details: {:?}", connection);

    let start = Instant::now();
    debug!("Accepting bidirectional stream...");
    let (mut send, mut recv) = connection.accept_bi().await?;
    debug!("Bidirectional stream accepted");

    debug!("Waiting for ping message...");
    let msg = recv.read_to_end(100).await?;
    debug!("Received raw message of {} bytes", msg.len());
    let msg = String::from_utf8(msg)?;
    info!("📨 Received from {}: {}", node_id, msg);

    debug!("Preparing pong response...");
    let response = "pong".to_string();
    debug!("Sending pong...");
    send.write_all(response.as_bytes()).await?;
    debug!("Finishing send stream...");
    send.finish()?;
    info!("📤 Sent to {}: {}", node_id, response);

    let elapsed = start.elapsed();
    info!(
        "Round-trip completed in {:.2}ms",
        elapsed.as_secs_f64() * 1000.0
    );

    if (tokio::time::timeout(TIMEOUT_DURATION, connection.closed()).await).is_err() {
        debug!(
            "Connection didn't close within {}s timeout",
            TIMEOUT_DURATION.as_secs()
        );
    }

    Ok(())
}

async fn run_listener() -> Result<()> {
    info!("Ping-Pong Listener starting...");

    let secret_key = SecretKey::generate();
    info!(" - Secret key: {secret_key}");

    let endpoint = Endpoint::builder()
        .secret_key(secret_key)
        .alpns(vec![PING_PONG_ALPN.to_vec()])
        .relay_mode(RelayMode::Default)
        .bind()
        .await?;

    let me = endpoint.node_id();
    info!(" - Node ID: {me}");
    info!(" - Listening addresses:");

    let local_addrs = endpoint
        .direct_addresses()
        .next()
        .await
        .context("no endpoints")?
        .into_iter()
        .map(|endpoint| {
            let addr = endpoint.addr.to_string();
            info!("\t\t{addr}");
            addr
        })
        .collect::<Vec<_>>()
        .join(" ");

    let relay_url = endpoint
        .home_relay()
        .expect("should be connected to a relay server");
    info!(" - Relay server URL: {relay_url}");
    info!(" - To connect, run:");
    info!("# With relay (mixed mode):");
    info!("RUST_LOG=\"irohping=info,iroh_net=none\" cargo run -- ping --addrs \"{local_addrs}\" --relay-url {relay_url} --node-id {me}");
    info!("# Without relay (direct UDP only):");
    info!("RUST_LOG=\"irohping=info,iroh_net=none\" cargo run -- ping --addrs \"{local_addrs}\" --node-id {me}");

    while let Some(incoming) = endpoint.accept().await {
        let connecting = match incoming.accept() {
            Ok(connecting) => connecting,
            Err(err) => {
                error!("Incoming connection failed: {:#}", err);
                continue;
            }
        };

        let connection = connecting.await?;
        let node_id = iroh_net::endpoint::get_remote_node_id(&connection)?;

        tokio::spawn(async move {
            if let Err(e) = handle_connection(connection, node_id).await {
                error!("Connection error: {:#}", e);
            }
        });
    }

    Ok(())
}

async fn run_ping(args: ConnectArgs) -> Result<()> {
    info!("Ping-Pong Ping sender starting...");
    let secret_key = SecretKey::generate();

    let endpoint = Endpoint::builder()
        .secret_key(secret_key)
        .alpns(vec![PING_PONG_ALPN.to_vec()])
        .relay_mode(RelayMode::Default)
        .bind()
        .await?;

    let me = endpoint.node_id();
    info!(" - Our node ID: {me}");

    debug!(" - Connecting to addresses: {:?}", args.addrs);

    let addr = NodeAddr::from_parts(args.node_id, args.relay_url.clone(), args.addrs);
    info!(
        " - Connection mode: {}",
        if args.relay_url.is_some() {
            "Mixed (UDP + Relay)"
        } else {
            "Direct (UDP only)"
        }
    );

    debug!("Establishing connection...");
    let connection = endpoint.connect(addr, PING_PONG_ALPN).await?;
    debug!("Connection established");

    let node_id = iroh_net::endpoint::get_remote_node_id(&connection)?;
    let remote_addr = connection.remote_address();
    let latency = connection.rtt().as_millis();

    info!(" - Connected to {}", args.node_id);
    info!(" - Remote address: {}", remote_addr);
    info!(" - Latency: {}ms", latency);
    debug!("Connection details: {:?}", connection);

    let start = Instant::now();
    debug!("Opening bidirectional stream...");
    let (mut send, mut recv) = connection.open_bi().await?;
    debug!("Bidirectional stream opened");

    let message = "ping".to_string();
    debug!("Sending ping...");
    send.write_all(message.as_bytes()).await?;
    debug!("Finishing send stream...");
    send.finish()?;
    info!("📤 Sent to {}: {}", node_id, message);

    debug!("Waiting for pong response...");
    let response = recv.read_to_end(100).await?;
    debug!("Received raw response of {} bytes", response.len());
    let response = String::from_utf8(response)?;
    info!("📨 Received from {}: {}", node_id, response);

    let elapsed = start.elapsed();
    info!(
        "Round-trip completed in {:.2}ms",
        elapsed.as_secs_f64() * 1000.0
    );

    debug!("Closing endpoint...");
    endpoint.close(0u8.into(), b"bye").await?;

    if (tokio::time::timeout(TIMEOUT_DURATION, connection.closed()).await).is_err() {
        debug!(
            "Connection didn't close within {}s timeout",
            TIMEOUT_DURATION.as_secs()
        );
    }

    debug!("Endpoint closed");
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    setup_logging();
    let cli = Cli::parse();

    match cli.command {
        Commands::Listen => run_listener().await,
        Commands::Ping(args) => run_ping(args).await,
    }
}
