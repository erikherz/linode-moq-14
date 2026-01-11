# Bridging moq-lite Relay to CloudFlare Draft 14

This document describes a strategy for bridging Luke's moq-lite relay servers with CloudFlare's Draft 14 MoQ network, enabling Safari (WebSocket) and Chrome (QUIC/WebTransport) interoperability.

## Problem Statement

### Goal
- Safari users publish via WebSocket to a moq-lite relay (us-central.earthseed.live)
- Chrome users publish via QUIC to CloudFlare's free MoQ CDN
- Both need to be able to view each other's streams

### Architecture Challenge

```
Safari ──WebSocket──▶ [moq-lite Relay] ◀──???──▶ [CloudFlare] ◀──QUIC── Chrome
```

**The bidirectional requirement:**
1. Safari broadcast → viewable by Chrome (via CloudFlare)
2. Chrome broadcast → viewable by Safari (via moq-lite relay)

### Protocol Differences: Draft 7 vs Draft 14

| Concept | Draft 7 | Draft 14 |
|---------|---------|----------|
| Announce streams | `ANNOUNCE` | `PUBLISH_NAMESPACE` |
| Discover streams | (implicit) | `SUBSCRIBE_NAMESPACE` |

### CloudFlare's Current Support

From CloudFlare's moq-rs README:

| Feature | Status |
|---------|--------|
| `PUBLISH_NAMESPACE` | ✅ Supported |
| `SUBSCRIBE_NAMESPACE` | ❌ Not Supported (Soon) |

CloudFlare's session handler returns `SessionError::unimplemented("SUBSCRIBE_NAMESPACE")`.

---

## How moq-lite Handles Remote Streams

### Luke's Cluster Architecture

```rust
pub struct Cluster {
    // Broadcasts announced by local clients (users)
    pub primary: Arc<moq_lite::Produce<OriginProducer, OriginConsumer>>,

    // Broadcasts announced by remote servers (cluster)
    pub secondary: Arc<moq_lite::Produce<OriginProducer, OriginConsumer>>,

    // Broadcasts announced by local clients and remote servers (merged)
    pub combined: Arc<moq_lite::Produce<OriginProducer, OriginConsumer>>,
}
```

### Flow for Regular Clients
1. Client connects to relay
2. Client gets access to `combined` (all broadcasts)
3. `run_combined()` continuously merges `primary` + `secondary` → `combined`

### Flow for Cluster Nodes
From `cluster.rs:263-277`:
```rust
async fn run_remote_once(&mut self, url: &Url) -> anyhow::Result<()> {
    // Connect to the remote node.
    let publish = Some(self.primary.consumer.consume());   // OUR broadcasts → THEM
    let subscribe = Some(self.secondary.producer.clone()); // THEIR broadcasts → US

    let session = self.client
        .connect(url.clone(), publish, subscribe)
        .await?;

    session.closed().await.map_err(Into::into)
}
```

### The Assumption

Luke's model assumes **both sides proactively push `PUBLISH_NAMESPACE`** for all their broadcasts. When you connect to a remote node:
- You send your broadcasts via `publish` parameter
- They send their broadcasts via `subscribe` parameter

**CloudFlare doesn't do this** - they wait for `SUBSCRIBE_NAMESPACE` (which they don't implement).

---

## Solution: CloudFlare Adapter Service

### Concept

Run a separate service that:
1. Acts as a **cluster node** to your moq-lite relay
2. Acts as a **subscriber client** to CloudFlare
3. Uses your **stream registry DB** to know what's on CloudFlare
4. **Bridges streams** by subscribing to CloudFlare and republishing to your relay

### Architecture

```
┌──────────────────────────────────────────────────────────────────────┐
│  Your Infrastructure                                                 │
│                                                                      │
│  ┌─────────────────────────┐      ┌─────────────────────────┐       │
│  │  moq-lite Relay         │      │  CloudFlare Adapter     │       │
│  │  (us-central.earthseed) │◀────▶│  (new service)          │       │
│  │                         │      │                         │       │
│  │  primary: Safari streams│      │  Connects to your relay │       │
│  │  secondary: from adapter│      │  as a "cluster node"    │       │
│  │  combined: merged       │      │                         │       │
│  └─────────────────────────┘      │  Also connects to CF    │       │
│           ▲                       │  as a subscriber        │       │
│           │                       │                         │       │
│      Safari (WS)                  │  Watches your DB for    │       │
│                                   │  CF streams, subscribes,│       │
│                                   │  republishes to relay   │       │
│                                   └───────────┬─────────────┘       │
│                                               │                      │
│                                               ▼                      │
│                                        CloudFlare                    │
│                                        (Chrome streams)              │
└──────────────────────────────────────────────────────────────────────┘
```

### Key Insight: Bypass SUBSCRIBE_NAMESPACE

The `SUBSCRIBE` message (for individual tracks) doesn't require `PUBLISH_NAMESPACE` or `SUBSCRIBE_NAMESPACE` first. If you **already know** a stream exists (from your DB), you can subscribe directly:

```
Your Adapter ──SUBSCRIBE(namespace="meeting/abc", track="video")──▶ CloudFlare
             ◀──SUBSCRIBE_OK + media streams──
```

---

## Stream Registry

Earthseed already has a stream tracking database (Cloudflare D1) with:

### Existing Schema
```sql
CREATE TABLE broadcast_events (
  id INTEGER PRIMARY KEY,
  user_id INTEGER NOT NULL,
  stream_id TEXT NOT NULL,
  started_at TEXT DEFAULT (datetime('now')),
  ended_at TEXT,
  geo_country TEXT,
  geo_city TEXT,
  -- ...
);
```

### Required Addition
```sql
ALTER TABLE broadcast_events ADD COLUMN origin TEXT DEFAULT 'cloudflare';
-- Values: 'cloudflare' | 'earthseed'
```

### API Endpoint
`GET /api/stats/greet` already returns active broadcasts. Add the `origin` field:

```typescript
// Filter for CloudFlare streams
const cfStreams = broadcasts.filter(b => b.origin === 'cloudflare');
```

---

## Adapter Service Code

### main.rs

```rust
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use moq_lite::{Origin, OriginConsumer, OriginProducer, BroadcastConsumer};
use tokio::sync::RwLock;
use url::Url;

#[derive(clap::Parser, Clone, Debug)]
pub struct Config {
    /// Your moq-lite relay URL (e.g., https://us-central.earthseed.live)
    #[arg(long, env = "EARTHSEED_RELAY_URL")]
    pub relay_url: String,

    /// CloudFlare relay URL (e.g., https://relay.quic.video)
    #[arg(long, env = "CLOUDFLARE_RELAY_URL")]
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
    tracing_subscriber::fmt::init();

    let config: Config = clap::Parser::parse();
    let client = moq_native::Client::new()?;

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
                    let mut state = state.write().await;

                    // Skip if already bridging
                    if state.active_bridges.contains(&stream.stream_id) {
                        continue;
                    }

                    tracing::info!(stream_id = %stream.stream_id, "bridging new CF stream");

                    // Start bridging this stream
                    state.active_bridges.insert(stream.stream_id.clone());

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
                        let mut state = state_clone.write().await;
                        state.active_bridges.remove(&stream_id);
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
    origin: String,  // "cloudflare" or "earthseed"
    #[serde(default)]
    viewer_count: u32,
}
```

### Cargo.toml

```toml
[package]
name = "cloudflare-adapter"
version = "0.1.0"
edition = "2021"

[dependencies]
moq-lite = { git = "https://github.com/moq-dev/moq", package = "moq-lite" }
moq-native = { git = "https://github.com/moq-dev/moq", package = "moq-native" }
tokio = { version = "1", features = ["full"] }
anyhow = "1"
clap = { version = "4", features = ["derive", "env"] }
tracing = "0.1"
tracing-subscriber = "0.3"
reqwest = { version = "0.11", features = ["json"] }
serde = { version = "1", features = ["derive"] }
url = "2"
```

---

## Running the Adapter

```bash
cloudflare-adapter \
  --relay-url https://us-central.earthseed.live \
  --cloudflare-url https://relay.quic.video \
  --registry-url https://earthseed.live/api/stats/greet \
  --relay-token "your-cluster-jwt" \
  --poll-interval 5
```

Or with environment variables:

```bash
export EARTHSEED_RELAY_URL=https://us-central.earthseed.live
export CLOUDFLARE_RELAY_URL=https://relay.quic.video
export STREAM_REGISTRY_URL=https://earthseed.live/api/stats/greet
export RELAY_TOKEN=your-cluster-jwt
export POLL_INTERVAL=5

cloudflare-adapter
```

---

## Complete Data Flow

### Safari Broadcasting (Safari → Chrome)

```
1. Safari connects to moq-lite relay via WebSocket
2. Safari sends PUBLISH_NAMESPACE for "meeting/abc123"
3. Relay stores in `primary` origin
4. Relay's cluster connection sends PUBLISH_NAMESPACE to CloudFlare
5. CloudFlare receives and responds with PUBLISH_NAMESPACE_OK
6. Chrome can now subscribe through CloudFlare
```

### Chrome Broadcasting (Chrome → Safari)

```
1. Chrome publishes to CloudFlare
2. Your web app calls POST /api/stats/broadcast
   { stream_id: "meeting/xyz789", origin: "cloudflare" }
3. Adapter polls GET /api/stats/greet, sees new CF stream
4. Adapter connects to CloudFlare, sends SUBSCRIBE for that stream
5. Adapter republishes to your relay via cluster connection
6. Stream appears in relay's `secondary` origin
7. run_combined() merges it into `combined`
8. Safari can now subscribe through your relay
```

---

## Questions for Luke

1. **Is this the right pattern?** Using the adapter as a cluster node that republishes streams?

2. **Connection parameters**: When connecting to CloudFlare, should we use specific JWT claims or parameters?

3. **Subscription without announcement**: Is `consume_broadcast()` the right API for subscribing to a known stream without receiving `PUBLISH_NAMESPACE` first?

4. **Bidirectional on same connection**: Should the adapter use one connection to CloudFlare for both subscribing (Chrome→Safari) and publishing (Safari→Chrome), or separate connections?

5. **Any CloudFlare-specific quirks** you're aware of in their Draft 14 implementation?

---

## References

- moq-lite cluster code: `rs/moq-relay/src/cluster.rs`
- moq-lite session handling: `rs/moq-lite/src/session.rs`
- moq-lite IETF publisher: `rs/moq-lite/src/ietf/publisher.rs`
- CloudFlare moq-rs: https://github.com/cloudflare/moq-rs
- Luke's advice: "clients will announce via the OriginProducer, so you can connect to CF and provide it the same OriginProducer used to gossip with the other cluster nodes"
