# Earthseed MoQ Infrastructure (Draft 14)

MoQ relay infrastructure for Earthseed using Luke's moq-lite (Draft 14) with CloudFlare bridge support.

## Components

### 1. moq-relay (Luke's)
The main relay server from [moq-dev/moq](https://github.com/moq-dev/moq), included as a git submodule pinned to a tested commit. Handles:
- Safari clients via WebSocket
- Chrome clients via WebTransport (when not using CloudFlare)
- Cluster federation with other relays

Built from the included submodule:
```bash
cargo build --release -p moq-relay
# Binary at: target/release/moq-relay
```

### 2. cloudflare-adapter
Bridges your moq-lite relay with CloudFlare's Draft 14 network. Enables:
- Chrome → CloudFlare → Adapter → Your Relay → Safari
- Polls your stream registry for CloudFlare-origin streams
- Subscribes to CloudFlare and republishes to your relay

## Building

```bash
# Clone with submodules
git clone --recursive https://github.com/erikherz/linode-moq-14
cd linode-moq-14

# Build the adapter (stable Rust)
cargo build --release
# Binary at: target/release/cloudflare-adapter

# Build the relay (requires nightly Rust for edition 2024)
cd moq
rustup run nightly cargo build --release -p moq-relay
cd ..
# Binary at: moq/target/release/moq-relay
```

## Deployment

### Prerequisites
- Domain with SSL cert (e.g., us-central.earthseed.live)
- Let's Encrypt certs at `/etc/letsencrypt/live/<domain>/`

### Systemd Services

**moq-earthseed.service** (main relay):
```ini
[Unit]
Description=MOQ Relay (Earthseed - Draft 14)
After=network.target

[Service]
Type=simple
ExecStart=/root/linode-moq-14/moq/target/release/moq-relay \
  --server-bind 0.0.0.0:443 \
  --tls-cert /etc/letsencrypt/live/us-central.earthseed.live/fullchain.pem \
  --tls-key /etc/letsencrypt/live/us-central.earthseed.live/privkey.pem \
  --web-https-listen 0.0.0.0:443 \
  --web-https-cert /etc/letsencrypt/live/us-central.earthseed.live/fullchain.pem \
  --web-https-key /etc/letsencrypt/live/us-central.earthseed.live/privkey.pem \
  --auth-public ""
Restart=always
RestartSec=1
SyslogIdentifier=moq-earthseed

[Install]
WantedBy=multi-user.target
```

**moq-adapter.service** (CloudFlare bridge):
```ini
[Unit]
Description=MOQ CloudFlare Adapter
After=network.target moq-earthseed.service

[Service]
Type=simple
Environment=EARTHSEED_RELAY_URL=https://us-central.earthseed.live
Environment=CLOUDFLARE_RELAY_URL=https://relay-next.cloudflare.mediaoverquic.com
Environment=STREAM_REGISTRY_URL=https://earthseed.live/api/stats/greet
Environment=POLL_INTERVAL=5
ExecStart=/root/linode-moq-14/target/release/cloudflare-adapter
Restart=always
RestartSec=1
SyslogIdentifier=moq-adapter

[Install]
WantedBy=multi-user.target
```

### Enable Services
```bash
sudo systemctl daemon-reload
sudo systemctl enable moq-earthseed moq-adapter
sudo systemctl start moq-earthseed moq-adapter
```

## Architecture

### Pure Linode Mode (Recommended)
All traffic goes through your relay servers. No adapter needed.

```
Chrome ──WebTransport──▶ [moq-lite Relay]
Safari ──WebSocket─────▶ us-central.earthseed.live
```

### CloudFlare Hybrid Mode
Chrome uses CloudFlare CDN, Safari uses your relay. Requires the adapter to bridge streams.

```
Safari ──WebSocket──▶ [moq-lite Relay] ◀──bridge──▶ [CloudFlare Adapter]
                      us-central.earthseed.live              │
                                                             │
                                                             ▼
Chrome ──WebTransport──────────────────────────────▶ [CloudFlare CDN]
                                        relay-next.cloudflare.mediaoverquic.com
```

> **Note:** The adapter is only needed for CloudFlare hybrid mode. For pure Linode mode, only run `moq-earthseed.service`.

## Multi-Region Deployment

To deploy additional relay servers in other regions:

### 1. Provision Servers

| Region | Domain | Location |
|--------|--------|----------|
| US Central | us-central.earthseed.live | Dallas |
| EU Central | eu-central.earthseed.live | Frankfurt |
| AP South | ap-south.earthseed.live | Singapore |

### 2. DNS Setup
Point each domain to its server's IP address.

### 3. SSL Certificates
```bash
# On each server
sudo apt install certbot
sudo certbot certonly --standalone -d <domain>
# Certs will be at /etc/letsencrypt/live/<domain>/
```

### 4. Build on Each Server
```bash
git clone --recursive https://github.com/erikherz/linode-moq-14
cd linode-moq-14/moq
rustup run nightly cargo build --release -p moq-relay
```

### 5. Create Systemd Service
Create `/etc/systemd/system/moq-earthseed.service` with your domain:

```ini
[Unit]
Description=MOQ Relay (Earthseed - Draft 14)
After=network.target

[Service]
Type=simple
ExecStart=/root/linode-moq-14/moq/target/release/moq-relay \
  --server-bind 0.0.0.0:443 \
  --tls-cert /etc/letsencrypt/live/<YOUR-DOMAIN>/fullchain.pem \
  --tls-key /etc/letsencrypt/live/<YOUR-DOMAIN>/privkey.pem \
  --web-https-listen 0.0.0.0:443 \
  --web-https-cert /etc/letsencrypt/live/<YOUR-DOMAIN>/fullchain.pem \
  --web-https-key /etc/letsencrypt/live/<YOUR-DOMAIN>/privkey.pem \
  --auth-public ""
Restart=always
RestartSec=1
SyslogIdentifier=moq-earthseed

[Install]
WantedBy=multi-user.target
```

> **Note:** The `--web-https-*` flags enable HTTPS on port 443 for health checks and latency racing.

### 6. Enable and Start
```bash
sudo systemctl daemon-reload
sudo systemctl enable moq-earthseed
sudo systemctl start moq-earthseed
sudo systemctl status moq-earthseed
```

### 7. Update Earthseed Client
After deploying multiple relays, update `earthseed/src/main.ts` to race connections and pick the fastest:

```typescript
const LINODE_RELAYS = [
  "https://us-central.earthseed.live/anon",
  "https://eu-central.earthseed.live/anon",
  "https://ap-south.earthseed.live/anon",
];
```

See [moq-lite_to_cf.md](./moq-lite_to_cf.md) for detailed design documentation.
