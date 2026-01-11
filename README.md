# Earthseed MoQ Infrastructure (Draft 14)

MoQ relay infrastructure for Earthseed using Luke's moq-lite (Draft 14) with CloudFlare bridge support.

## Components

### 1. moq-relay (Luke's)
The main relay server from [moq-dev/moq](https://github.com/moq-dev/moq). Handles:
- Safari clients via WebSocket
- Chrome clients via WebTransport (when not using CloudFlare)
- Cluster federation with other relays

Install directly from Luke's repo:
```bash
cargo install --git https://github.com/moq-dev/moq moq-relay
```

### 2. cloudflare-adapter
Bridges your moq-lite relay with CloudFlare's Draft 14 network. Enables:
- Chrome → CloudFlare → Adapter → Your Relay → Safari
- Polls your stream registry for CloudFlare-origin streams
- Subscribes to CloudFlare and republishes to your relay

## Building

```bash
# Build the adapter
cargo build --release

# Binary will be at:
# target/release/cloudflare-adapter
```

## Deployment

### Prerequisites
- Domain with SSL cert (e.g., us-central.earthseed.live)
- Let's Encrypt certs at `/etc/letsencrypt/live/<domain>/`

### Install moq-relay
```bash
cargo install --git https://github.com/moq-dev/moq moq-relay
```

### Systemd Services

**moq-earthseed.service** (main relay):
```ini
[Unit]
Description=MOQ Relay (Earthseed - Draft 14)
After=network.target

[Service]
Type=simple
ExecStart=/root/.cargo/bin/moq-relay \
  --bind 0.0.0.0:443 \
  --tls-cert /etc/letsencrypt/live/us-central.earthseed.live/fullchain.pem \
  --tls-key /etc/letsencrypt/live/us-central.earthseed.live/privkey.pem
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
Environment=CLOUDFLARE_RELAY_URL=https://relay.quic.video
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

```
Safari ──WebSocket──▶ [moq-lite Relay] ◀──cluster──▶ [CloudFlare Adapter]
                      us-central.earthseed.live              │
                                                             │
                                                             ▼
Chrome ──WebTransport──────────────────────────────▶ [CloudFlare CDN]
                                                    relay.quic.video
```

See [moq-lite_to_cf.md](./moq-lite_to_cf.md) for detailed design documentation.
