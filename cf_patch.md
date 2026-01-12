# CloudFlare Bridge Patch for moq-lite

## Problem

CloudFlare's Draft 14 MoQ implementation doesn't send `PUBLISH_NAMESPACE` messages. This breaks the standard moq-lite subscription flow:

```
Normal flow (works with Luke's relay):
  Remote sends PUBLISH_NAMESPACE("earthseed.live/abc123")
      ↓
  Subscriber::recv_publish_namespace() called
      ↓
  Subscriber::start_announce() creates Broadcast, spawns subscription handler
      ↓
  Handler waits for track requests, sends SUBSCRIBE to remote
      ↓
  Remote sends data back

CloudFlare flow (broken):
  CloudFlare never sends PUBLISH_NAMESPACE
      ↓
  start_announce() never called
      ↓
  No subscription handler spawned
      ↓
  consume_broadcast() returns None
```

## Solution

Expose a way to manually trigger `start_announce()` without receiving `PUBLISH_NAMESPACE`. This allows us to "announce" streams we know about from our registry, which wires up the subscription machinery.

## Implementation Status: COMPLETED

### Patch Applied (2026-01-12)

The `announce_remote()` patch has been implemented and pushed to `erikherz/moq` on the `cloudflare-patch` branch.

**Files Modified:**

| File | Changes |
|------|---------|
| `moq/rs/moq-lite/src/command.rs` | NEW: `SubscriberCommand` enum |
| `moq/rs/moq-lite/src/lib.rs` | Export `command` module |
| `moq/rs/moq-lite/src/ietf/session.rs` | Pass command channel to Subscriber |
| `moq/rs/moq-lite/src/ietf/subscriber.rs` | Handle commands in `run()` loop via `tokio::select!` |
| `moq/rs/moq-lite/src/session.rs` | Add `subscriber_commands` field and `announce_remote()` method |

**Commit:** `e0d1598` - "Add announce_remote() for CloudFlare bridge"

### Adapter Updated

The `cloudflare-adapter` has been updated to use the new `announce_remote()` API:

- Stores CloudFlare session in shared state
- Calls `session.announce_remote(namespace)` before consuming broadcast
- Uses `earthseed.live/{stream_id}` namespace pattern

**Commits:**
- `3618546` - "Update adapter to use announce_remote() for CloudFlare bridge"
- `d41836f` - "Use earthseed.live/{stream_id} namespace pattern"

### Relay Configuration

The moq-relay needs both QUIC and HTTPS (WebSocket) enabled:

```ini
ExecStart=/root/linode-moq-14/moq/target/release/moq-relay \
  --server-bind 0.0.0.0:443 \
  --tls-cert /etc/letsencrypt/live/us-central.earthseed.live/fullchain.pem \
  --tls-key /etc/letsencrypt/live/us-central.earthseed.live/privkey.pem \
  --web-https-listen 0.0.0.0:443 \
  --web-https-cert /etc/letsencrypt/live/us-central.earthseed.live/fullchain.pem \
  --web-https-key /etc/letsencrypt/live/us-central.earthseed.live/privkey.pem \
  --auth-public ""
```

Note: QUIC uses UDP, HTTPS uses TCP - both can use port 443.

## Current Architecture

```
Chrome ──WebTransport──▶ [CloudFlare CDN]
                              │
                              ▼
                    [cloudflare-adapter]
                      - polls registry for CF streams
                      - calls announce_remote(namespace)
                      - bridges to Linode relay
                              │
                              ▼
Safari ──WebSocket───▶ [moq-relay @ Linode]
                       us-central.earthseed.live
```

## Remaining Issue: Stream Registration

**Status:** Safari connects to Linode relay successfully, but streams aren't being bridged.

**Root cause:** New streams from Chrome aren't appearing in the registry (`/api/stats/greet`).

The `logBroadcastStart()` function in `earthseed/src/auth.ts` should register streams with `origin: "cloudflare"`, but it's not being triggered.

**Debug steps needed:**
1. Check Chrome console for `[Broadcast Status Check]` logs when streaming
2. Verify the hang-publish component status text matches expected patterns ("🟢", "Live", "Audio Only")
3. Check Network tab for `/api/stats/broadcast` API calls

## Fork Setup

```
GitHub:
  upstream:    github.com/moq-dev/moq (Luke's repo)
  fork:        github.com/erikherz/moq (our patched version)

linode-moq-14/
  └── moq/     submodule → erikherz/moq @ cloudflare-patch branch
```

| Item | Value |
|------|-------|
| Fork | `github.com/erikherz/moq` |
| Branch | `cloudflare-patch` |
| Patch commit | `e0d1598` |
| Patch status | **Implemented and deployed** |

### Cloning Fresh

```bash
git clone --recursive https://github.com/erikherz/linode-moq-14
cd linode-moq-14

# Build adapter
cargo build --release -p cloudflare-adapter

# Build relay (requires nightly)
cd moq && rustup run nightly cargo build --release -p moq-relay && cd ..
```

### Syncing with Upstream (Future)

```bash
cd moq
git fetch origin                          # Fetch from Luke's repo
git merge origin/main                     # Merge upstream changes
git push erikherz cloudflare-patch        # Push to our fork
cd ..
git add moq
git commit -m "Sync moq with upstream"
git push
```

## Testing Checklist

- [x] moq-lite patch compiles
- [x] cloudflare-adapter compiles and runs
- [x] moq-relay serves WebSocket on port 443
- [x] Safari connects to Linode relay via WebSocket
- [x] Safari establishes moq-lite session
- [ ] Chrome streams register in database with origin: "cloudflare"
- [ ] Adapter bridges stream from CloudFlare to Linode
- [ ] Safari can watch Chrome stream via bridge
