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

## Implementation

### 1. Add `announce_remote()` to Subscriber

**File**: `moq/rs/moq-lite/src/ietf/subscriber.rs`

```rust
/// Manually announce a remote broadcast without waiting for PUBLISH_NAMESPACE.
/// Use this when you know a broadcast exists on the remote but they won't announce it.
pub fn announce_remote(&mut self, path: impl AsPath) -> Result<(), Error> {
    self.start_announce(path.as_path().to_owned())?;
    Ok(())
}
```

### 2. Add command channel to Session

**File**: `moq/rs/moq-lite/src/session.rs`

```rust
pub struct Session {
    session: Arc<dyn SessionInner>,
    subscriber_commands: Option<tokio::sync::mpsc::Sender<SubscriberCommand>>,
}

pub enum SubscriberCommand {
    AnnounceRemote(PathOwned),
}

impl Session {
    /// Manually trigger a subscription to a known remote broadcast.
    /// Use when the remote doesn't send PUBLISH_NAMESPACE.
    pub async fn announce_remote(&self, path: impl AsPath) -> Result<(), Error> {
        if let Some(tx) = &self.subscriber_commands {
            tx.send(SubscriberCommand::AnnounceRemote(path.as_path().to_owned()))
                .await
                .map_err(|_| Error::Closed)?;
            Ok(())
        } else {
            Err(Error::InvalidRole)
        }
    }
}
```

### 3. Wire up in ietf::start()

**File**: `moq/rs/moq-lite/src/ietf/mod.rs`

- Create a command channel (tokio mpsc)
- Pass the receiver to the Subscriber
- Return the sender to be stored in Session
- Subscriber listens for commands and calls `start_announce()` when received

### 4. Handle commands in Subscriber

**File**: `moq/rs/moq-lite/src/ietf/subscriber.rs`

Add to the Subscriber's run loop:

```rust
pub async fn run(mut self, mut commands: mpsc::Receiver<SubscriberCommand>) -> Result<(), Error> {
    loop {
        tokio::select! {
            // Existing: accept uni streams
            stream = self.session.accept_uni() => {
                // ... existing code ...
            }
            // New: handle commands
            Some(cmd) = commands.recv() => {
                match cmd {
                    SubscriberCommand::AnnounceRemote(path) => {
                        if let Err(e) = self.start_announce(path) {
                            tracing::warn!(%e, "failed to announce remote");
                        }
                    }
                }
            }
        }
    }
}
```

## Files to Modify

| File | Changes |
|------|---------|
| `moq/rs/moq-lite/src/ietf/subscriber.rs` | Add `announce_remote()`, modify `run()` to handle commands |
| `moq/rs/moq-lite/src/ietf/mod.rs` | Create command channel in `start()`, pass to Subscriber |
| `moq/rs/moq-lite/src/session.rs` | Add `announce_remote()` method, store command sender |
| `moq/rs/moq-lite/src/lib.rs` | Export `SubscriberCommand` if needed |

## Adapter Usage

```rust
async fn bridge_cloudflare_stream(
    session: &moq_lite::Session,
    stream_id: &str,
) -> anyhow::Result<()> {
    let namespace = format!("earthseed.live/{}", stream_id);

    // This triggers start_announce() internally, which:
    // 1. Creates a Broadcast and publishes to our origin
    // 2. Spawns run_broadcast() which listens for track requests
    // 3. When tracks are requested, sends SUBSCRIBE to CloudFlare
    session.announce_remote(&namespace).await?;

    tracing::info!(%stream_id, "announced remote stream from CloudFlare");
    Ok(())
}
```

## Fork Setup (Completed)

We're using a fork of Luke's moq repo to hold our patches:

```
GitHub:
  upstream:    github.com/moq-dev/moq (Luke's repo)
  fork:        github.com/erikherz/moq (our patched version)

linode-moq-14/
  └── moq/     submodule → erikherz/moq @ cloudflare-patch branch
```

### Current State

| Item | Value |
|------|-------|
| Fork | `github.com/erikherz/moq` |
| Branch | `cloudflare-patch` |
| Base commit | `fe72aeb` (Jan 9, 2026 - stable, no nightly Rust features) |
| Patch status | **Not yet implemented** - branch exists but no changes yet |

### Why a Fork?

If we only patch the submodule locally:
- Changes exist only on your machine
- `git clone --recursive` fails for others (commit doesn't exist in Luke's repo)
- CI/CD can't build

With a fork:
- Changes live on GitHub at `erikherz/moq`
- Fresh clones work: `git clone --recursive` gets our patched code
- Linode server can `git pull && cargo build` without issues

### Cloning Fresh

```bash
git clone --recursive https://github.com/erikherz/linode-moq-14
cd linode-moq-14

# moq/ submodule automatically pulls from erikherz/moq, cloudflare-patch branch
```

### Adding the Patch

```bash
cd moq

# Make changes to files in rs/moq-lite/src/...
# (see "Files to Modify" section above)

git add -A
git commit -m "Add announce_remote() for CloudFlare bridge"
git push erikherz cloudflare-patch

cd ..
git add moq
git commit -m "Update moq submodule with announce_remote patch"
git push
```

### Syncing with Upstream (Future)

If Luke adds features we want:

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

## Complexity

- **Lines of code**: ~50-100 lines across 4 files
- **Risk**: Medium - modifying internal session handling
- **Testing**: Requires live CloudFlare connection to verify

## Alternative: Skip CloudFlare

Luke's recommendation: "You should rely on Linode only until you actually need scale (and Cloudflare actually implements features)."

The moq-relay on Linode handles both Safari (WebSocket) and Chrome (WebTransport) directly. CloudFlare bridging can wait until they implement `PUBLISH_NAMESPACE` or `SUBSCRIBE_NAMESPACE`.
