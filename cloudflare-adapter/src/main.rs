//! CloudFlare Adapter Service
//!
//! Bridges moq-lite relay with CloudFlare's Draft 14 MoQ network.
//! - Connects to your moq-lite relay as a cluster node
//! - Connects to CloudFlare as a subscriber
//! - Polls your stream registry for CloudFlare-origin streams
//! - Bridges streams by subscribing to CloudFlare and republishing to your relay

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use clap::Parser;
use moq_lite::{Origin, OriginConsumer, OriginProducer};
use moq_native::ClientConfig;
use tokio::sync::RwLock;
use url::Url;

#[derive(Parser, Clone, Debug)]
#[command(name = "cloudflare-adapter")]
#[command(about = "Bridges moq-lite relay with CloudFlare Draft 14 network")]
pub struct Config {
    /// Your moq-lite relay URL (e.g., https://us-central.earthseed.live)
    #[arg(long, env = "EARTHSEED_RELAY_URL")]
    pub relay_url: String,

    /// CloudFlare relay URL (e.g., https://relay.quic.video)
    #[arg(long, env = "CLOUDFLARE_RELAY_URL", default_value = "https://relay.quic.video")]
    pub cloudflare_url: String,

    /// Your stream registry API (e.g., https://earthseed.live/api/stats/greet)
    #[arg(long, env = "STREAM_REGISTRY_URL")]
    pub registry_url: String,

    /// JWT token for connecting to your relay as a cluster node
    #[arg(long, env = "RELAY_TOKEN")]
    pub relay_token: Option<String>,

    /// How often to poll the registry for new CF streams (seconds)
    #[arg(long, default_value = "5", env = "POLL_INTERVAL")]
    pub poll_interval: u64,
}

/// Tracks which streams we're currently bridging
struct BridgeState {
    active_bridges: HashSet<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("cloudflare_adapter=info".parse()?)
                .add_directive("moq_lite=info".parse()?)
                .add_directive("moq_native=info".parse()?),
        )
        .init();

    let config = Config::parse();

    tracing::info!(
        relay_url = %config.relay_url,
        cloudflare_url = %config.cloudflare_url,
        registry_url = %config.registry_url,
        poll_interval = config.poll_interval,
        "Starting CloudFlare adapter"
    );

    let client = ClientConfig::default().init()?;

    // Origin for broadcasts we'll publish TO your relay
    let to_relay = Arc::new(Origin::produce());

    // Origin for broadcasts we receive FROM CloudFlare
    let from_cloudflare = Arc::new(Origin::produce());

    let state = Arc::new(RwLock::new(BridgeState {
        active_bridges: HashSet::new(),
    }));

    tokio::select! {
        res = run_relay_connection(
            client.clone(),
            &config,
            to_relay.clone()
        ) => {
            res.context("relay connection failed")?;
        }
        res = run_cloudflare_connection(
            client.clone(),
            &config,
            from_cloudflare.clone()
        ) => {
            res.context("cloudflare connection failed")?;
        }
        res = run_bridge_manager(
            &config,
            state.clone(),
            from_cloudflare.consumer.clone(),
            to_relay.producer.clone()
        ) => {
            res.context("bridge manager failed")?;
        }
    }

    Ok(())
}

/// Connect to YOUR relay as a cluster node
/// Publishes CF streams into your relay's `secondary` origin
async fn run_relay_connection(
    client: moq_native::Client,
    config: &Config,
    to_relay: Arc<moq_lite::Produce<OriginProducer, OriginConsumer>>,
) -> anyhow::Result<()> {
    let token = config.relay_token.as_deref().unwrap_or("");
    let url = Url::parse(&format!("{}/?jwt={}", config.relay_url, token))?;

    loop {
        tracing::info!(%url, "connecting to earthseed relay");

        // We publish TO the relay (CF streams we're bridging)
        // We don't subscribe FROM it (we get streams from CF directly)
        let publish = Some(to_relay.consumer.consume());
        let subscribe: Option<OriginProducer> = None;

        match client.connect(url.clone(), publish, subscribe).await {
            Ok(session) => {
                tracing::info!("connected to relay");
                let _ = session.closed().await;
                tracing::warn!("relay connection closed");
            }
            Err(err) => {
                tracing::error!(%err, "failed to connect to relay");
            }
        }

        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

/// Connect to CloudFlare as a subscriber
/// Receives PUBLISH_NAMESPACE for streams (if CF sends them)
/// More importantly: allows us to SUBSCRIBE to specific streams
async fn run_cloudflare_connection(
    client: moq_native::Client,
    config: &Config,
    from_cloudflare: Arc<moq_lite::Produce<OriginProducer, OriginConsumer>>,
) -> anyhow::Result<()> {
    let url = Url::parse(&config.cloudflare_url)?;

    loop {
        tracing::info!(%url, "connecting to cloudflare");

        // We subscribe FROM CloudFlare
        // We don't publish TO it (Safari streams go via your relay)
        let publish: Option<OriginConsumer> = None;
        let subscribe = Some(from_cloudflare.producer.clone());

        match client.connect(url.clone(), publish, subscribe).await {
            Ok(session) => {
                tracing::info!("connected to cloudflare");
                let _ = session.closed().await;
                tracing::warn!("cloudflare connection closed");
            }
            Err(err) => {
                tracing::error!(%err, "failed to connect to cloudflare");
            }
        }

        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

/// Polls your registry for CF streams and bridges them
async fn run_bridge_manager(
    config: &Config,
    state: Arc<RwLock<BridgeState>>,
    from_cloudflare: OriginConsumer,
    to_relay: OriginProducer,
) -> anyhow::Result<()> {
    let http_client = reqwest::Client::new();

    loop {
        match fetch_cloudflare_streams(&http_client, &config.registry_url).await {
            Ok(streams) => {
                for stream in streams {
                    let mut state_guard = state.write().await;

                    // Skip if already bridging
                    if state_guard.active_bridges.contains(&stream.stream_id) {
                        continue;
                    }

                    tracing::info!(stream_id = %stream.stream_id, "bridging new CF stream");

                    // Start bridging this stream
                    state_guard.active_bridges.insert(stream.stream_id.clone());
                    drop(state_guard); // Release lock before spawning

                    let stream_id = stream.stream_id.clone();
                    let from_cf = from_cloudflare.clone();
                    let to_relay = to_relay.clone();
                    let state_clone = state.clone();

                    // Spawn a task to bridge this specific stream
                    tokio::spawn(async move {
                        if let Err(err) = bridge_stream(&stream_id, from_cf, to_relay).await {
                            tracing::warn!(%err, stream_id = %stream_id, "bridge failed");
                        }

                        // Remove from active bridges when done
                        let mut state_guard = state_clone.write().await;
                        state_guard.active_bridges.remove(&stream_id);
                    });
                }
            }
            Err(err) => {
                tracing::warn!(%err, "failed to fetch stream registry");
            }
        }

        tokio::time::sleep(Duration::from_secs(config.poll_interval)).await;
    }
}

/// Bridge a single stream from CloudFlare to your relay
async fn bridge_stream(
    stream_id: &str,
    from_cloudflare: OriginConsumer,
    to_relay: OriginProducer,
) -> anyhow::Result<()> {
    tracing::info!(stream_id, "starting bridge");

    // Subscribe to the broadcast on CloudFlare
    // This sends SUBSCRIBE (not SUBSCRIBE_NAMESPACE) for the specific stream
    let broadcast = from_cloudflare
        .consume_broadcast(stream_id)
        .context("stream not found on cloudflare")?;

    // Publish it to your relay
    // This makes it appear in your relay's `secondary` origin
    to_relay.publish_broadcast(stream_id, broadcast.clone());

    // Keep the bridge alive until the broadcast ends
    broadcast.closed().await;

    tracing::info!(stream_id, "bridge closed");
    Ok(())
}

/// Fetch active CloudFlare streams from your registry
async fn fetch_cloudflare_streams(
    client: &reqwest::Client,
    registry_url: &str,
) -> anyhow::Result<Vec<StreamInfo>> {
    let response = client
        .get(registry_url)
        .send()
        .await?
        .json::<RegistryResponse>()
        .await?;

    // Filter to only CloudFlare-origin streams
    Ok(response
        .broadcasts
        .into_iter()
        .filter(|s| s.origin == "cloudflare")
        .collect())
}

#[derive(Debug, serde::Deserialize)]
struct RegistryResponse {
    broadcasts: Vec<StreamInfo>,
}

#[derive(Debug, serde::Deserialize)]
struct StreamInfo {
    stream_id: String,
    #[serde(default = "default_origin")]
    origin: String,
    #[serde(default)]
    viewer_count: u32,
}

fn default_origin() -> String {
    "cloudflare".to_string()
}
