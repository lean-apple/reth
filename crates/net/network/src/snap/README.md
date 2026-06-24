# snap/2 (EIP-8189)

`snap/2` is a state-sync sub-protocol that is negotiated alongside `eth` on the same `RLPx`
connection. Messages `0x00..0x05` (account / storage / byte-code ranges) are unchanged from snap/1;
trie nodes (`0x06`/`0x07`) are removed; block access lists (BALs) are added as `0x08`/`0x09`. A node
acquires state by downloading a snapshot at a frozen pivot block and then applying verified BAL
diffs forward to the head, instead of re-executing history.

Spec: <https://eips.ethereum.org/EIPS/eip-8189>

This document describes the architecture of the reth implementation: how `snap/2` is transported
(the dedicated `eth`+`snap` stream), how it is negotiated and routed through the session, how the
sync client and server are wired, and how state is acquired. It is meant to stay accurate as the
feature evolves.

---

## 1. Transport: a dedicated `eth`+`snap` stream

### Why a dedicated stream

`snap/2` only ever rides alongside `eth`, so reth carries the pair with a purpose-built two-protocol
stream, `EthSnapStream`, instead of the general-purpose sub-protocol multiplexer
(`RlpxSatelliteStream`). It owns the raw `P2PStream` directly and knows about exactly `eth` and
`snap`:

- **Typed at the boundary.** Inbound snap frames are decoded once into `SnapProtocolMessage` and
  validated against the negotiated version; they are never passed around as opaque bytes.
- **One obvious code path.** A peer that speaks snap is one stream object that owns both
  capabilities — closer to how geth models a snap-capable peer — rather than two decoupled streams
  joined by a side registry.
- **Small, auditable demux.** Multiplexing two known protocols is a handful of branches, not a
  general scheduler.

```
        eth-only peer                          eth + snap/2 peer
┌──────────────────────────┐      ┌─────────────────────────────────────────────┐
│ one RLPx conn / peer     │      │ one RLPx conn / peer                         │
│  ┌────────────────────┐  │      │  ┌───────────────┐  ┌──────────────────────┐ │
│  │ eth/6x..7x         │  │      │  │ eth (primary) │  │ snap/2 (typed)       │ │
│  └────────────────────┘  │      │  └───────────────┘  └──────────────────────┘ │
│                          │      │            EthSnapStream  (one owner)        │
│ EthRlpxConnection::      │      │   EthRlpxConnection::EthSnap                 │
│        EthOnly           │      │   one Stream/Sink of EthSnapMessage          │
│ advertised: [eth]        │      │   advertised: [eth, snap/2]   (default-off)  │
└──────────────────────────┘      └─────────────────────────────────────────────┘
```

### Wire framing and message-id offsets

`P2PStream` handles the `p2p` reserved message-id range (`0x00..=0x0f`) and, for sub-protocol
messages, yields/accepts frames whose first byte is a **combined-capability id** = `wire_id - 0x10`.
Capabilities are laid out contiguously in that combined space in negotiation order. Crucially, `eth`
is always the first shared capability, so its relative offset is `0` and its ids pass through
unchanged. `snap/2` sits immediately after `eth`, at `snap_offset = eth.num_messages` (e.g. `17`
when the negotiated eth version reserves 17 ids).

`EthSnapStream` therefore demultiplexes with a single comparison:

```
inbound  (from P2PStream, combined id `c`):
    c < snap_offset                       -> eth   (pass through, eth id == c)
    snap_offset <= c < snap_offset + N    -> snap  (snap id = c - snap_offset)
    c >= snap_offset + N                  -> protocol breach (out of range)

outbound:
    eth  message               -> framed with its id unchanged
    snap message (snap id `s`) -> framed at combined id `s + snap_offset`
```

where `N` is the number of ids the negotiated `snap/2` capability reserves. Because the connection
carries only `eth` and `snap/2`, an id past that range cannot belong to it and is rejected.

`snap/2` message validity is **not** a contiguous range (trie nodes `0x06`/`0x07` are removed). The
single source of truth is `SnapVersion::supports_message_id` and the helper
`SnapProtocolMessage::decode_versioned(version, bytes)`, which rejects ids invalid for the version
before decoding. Inbound frames with an id invalid for the negotiated version (e.g. `0x06`/`0x07`)
or a malformed payload are a protocol violation by the peer and surface as a protocol-breach error,
not silently dropped.

### `EthSnapStream` internals

`EthSnapStream<St, N>` (in `eth-wire/src/eth_snap.rs`) owns:

- the raw `P2PStream<St>` (the wire);
- the `eth` primary as `EthStream<EthProxy, N>`, where `EthProxy` is a tiny channel-backed,
  pass-through byte view (eth offset is `0`, so no masking) implementing the same
  `Stream`/`Sink<Bytes>`/`CanDisconnect` surface that `EthStream` expects;
- two channels for the typed snap side-channel (inbound snap frames up, outbound snap frames down).

It exposes both protocols through **one** `Stream`/`Sink` of `EthSnapMessage<N>`:

```rust
pub enum EthSnapMessage<N> {
    Eth(EthMessage<N>),
    Snap(SnapProtocolMessage),
}
```

`poll_next` drives the shared wire on every poll: it surfaces a ready decoded `eth` message, then a
ready decoded `snap` message, then services outbound (flush queued `eth`/`snap` frames) and inbound
(pull frames off `P2PStream` and route them by offset to the eth proxy or the snap channel). Because
both protocols share one connection, a single poll services both — the owner drives one stream, not
two.

### Handshake

`EthSnapStream::handshake(p2p_stream, status, fork_filter, handshake, eth_max_message_size)` performs
the `eth` status handshake over the multiplexed connection and returns the established stream plus
the peer's `UnifiedStatus`. It runs the injected `EthRlpxHandshake` against an `UnauthEthProxy`
(mapping the proxy's `io::Error` to `P2PStreamError`) while concurrently draining the wire, so any
`snap/2` frames that arrive mid-handshake are buffered and surfaced once the stream is polled.

---

## 2. Session integration

### Connection variants

`EthRlpxConnection` (in `network/src/session/conn.rs`) is the per-session connection enum. It yields
a unified `EthSnapMessage` and accepts `EthMessage` (eth-only and satellite variants lift their
messages into `EthSnapMessage::Eth`; snap outbound goes through `start_send_snap`):

| Variant | Carries | Built when |
| --- | --- | --- |
| `EthOnly` | `eth` only | only `eth` is shared |
| `EthSnap` | `eth` + `snap/2` (`EthSnapStream`) | the shared capabilities are **exactly** `eth` + `snap/2` |
| `Satellite` | `eth` + one or more generic extras | any other extra is shared, **including** `snap/2` alongside other extras |

The `EthSnap` branch is deliberately scoped to exactly `eth` + `snap/2`. The dedicated stream only
composes those two protocols, so a peer that negotiates `snap/2` *and* another sub-protocol goes
through the general `Satellite` multiplexer instead (which does not give `snap/2` dedicated
treatment) — `EthSnap` never steals peers that need the generic path.

### Negotiation

Capability advertisement lives in `eth-wire`'s `HelloMessageWithProtocols::with_snap` and is toggled
by `NetworkConfigBuilder::with_snap` (default-off). During session establishment
(`network/src/session/mod.rs`), once the shared capabilities are known:

- only `eth` shared → plain `EthStream` (`EthOnly`);
- exactly `eth` + `snap/2` shared → `EthSnapStream::handshake` on the raw `P2PStream` → `EthSnap`
  (this branch bypasses `RlpxProtocolMultiplexer` entirely);
- otherwise (any other extra, including `snap/2` alongside other extras) → the general satellite
  multiplexer (`Satellite`).

### Message dispatch

The active session (`network/src/session/active.rs`) polls the connection as a single stream and
dispatches on the unified message:

```
EthSnapMessage::Eth(msg)  -> on_incoming_message(msg)        // existing eth handling
EthSnapMessage::Snap(msg) -> on_incoming_snap_message(msg)   // snap serve + response correlation
```

Outbound `eth` messages use the existing `EthMessage` sink path; outbound `snap` messages use
`EthRlpxConnection::start_send_snap` (a no-op on non-snap connections).

---

## 3. Sync client and server

`snap/2` is request/response with a `request_id`, like `eth` `GetX`/`X` pairs. Because the dedicated
stream is owned by the active session — not a global registry — request routing and response
correlation are owned by the session layer:

```
download side (client)                                 serve side (server)
──────────────────────                                 ───────────────────
SnapClient.get_*()                                      peer -> GetAccountRange / GetBlockAccessLists
  └─ mpsc ─> SessionManager                             ActiveSession.on_incoming_snap_message
       picks any snap-capable ActiveSession               └─ serve from BAL / state provider
       └─ SessionCommand::SnapRequest ─> ActiveSession      └─ start_send_snap(response)
            ├─ start_send_snap(request)
            └─ inflight: request_id -> oneshot
peer response -> on_incoming_snap_message
  └─ correlate request_id -> oneshot -> SnapClient
```

- **Client.** `SnapClient` (`network/src/snap/client.rs`, implementing the
  `reth_network_p2p::snap::client::SnapClient` trait) hands a request to the `SessionManager`, which
  forwards it to any snap-capable session (round-robin / first-available). The session sends it via
  `start_send_snap`, records `request_id -> oneshot`, and resolves the oneshot when the matching
  response arrives. Missing/empty entries must not trigger endless refetch.
- **Server.** Inbound requests are served from reth's existing stores: BALs from the BAL store
  (`bal.rs`), and account/storage/code ranges from state. Per EIP-8189, missing BALs are returned as
  empty entries, responses preserve request order, and the tail may be truncated to respect the
  response byte / QoS limit.

---

## 4. State acquisition / sync pipeline

```
   BEFORE — full execution sync            AFTER — snap/2 state sync (SnapSyncStage)

   Headers                                 Headers
   Bodies                                  Bodies
   SenderRecovery                          ┌────────────── SnapSync stage ──────────────┐
   Execution  ◄ replay EVERY tx            │ 1. freeze pivot (recent block)             │
      │        genesis→head to             │ 2. download account/storage/code ranges    │
      │        BUILD state trie            │    @ pivot state-root, verify range proofs  │
   MerkleExecute (state root)              │ 3. persist snapshot                         │
   Hashing / History / TxLookup            │ 4. BAL catch-up  pivot+1 ..= head, chunked: │
   Finish                                  │    GetBlockAccessLists → verify vs header   │
                                           │    block_access_list_hash → apply in order  │
   ▶ state = re-execute all history        │ 5. verify final state root                  │
     (CPU-bound)                           └─────────────────────────────────────────────┘
                                           Hashing / History / TxLookup ▶ Finish
                                           ▶ state = snapshot @ pivot + verified BAL diffs
                                             (network / IO-bound)
```

snap/1 healed the moving state target with trie-node requests (`0x06`/`0x07`); snap/2 replaces that
with compact BAL diffs (`0x08`/`0x09`) applied in strict block order, each verified against the
header's `block_access_list_hash`. Orchestration — pivot selection and freeze, restart on reorg or
unavailable BALs, chunked catch-up to bound memory, progress checkpoints, final state-root check — is
owned by the staged sync pipeline (`SnapSyncStage`), not the protocol, mirroring how geth drives its
snap syncer from the downloader.

---

## 5. Module map

Transport (`eth-wire`):

| Path | Role |
| --- | --- |
| `../../../eth-wire/src/eth_snap.rs` | `EthSnapStream`, `EthSnapMessage`, the `EthProxy` bridge, and `handshake`: the dedicated `eth`+`snap` stream |
| `../../../eth-wire-types/src/snap.rs` | `SnapProtocolMessage`, `SnapVersion`, `decode_versioned` (version-aware validity) |

Session (`network/src/session`):

| Path | Role |
| --- | --- |
| `../session/conn.rs` | `EthRlpxConnection::EthSnap` variant; `start_send_snap` |
| `../session/mod.rs` | branch to `EthSnapStream::handshake` when `snap/2` is shared |
| `../session/active.rs` | poll one stream, dispatch `EthSnapMessage`; `on_incoming_snap_message` |

Sync (`network/src/snap` and stages):

| Path | Role |
| --- | --- |
| `client.rs` | `SnapClient`: dispatch requests, future per request |
| `bal.rs` | `fetch_and_verify_bals`: fetch BALs and verify them against headers |
| `verify.rs` | BAL hash + strict-order verification |
| `sync.rs` | pure sync helpers: progress descriptor, BAL catch-up chunking |
| `../../../../stages/stages/src/stages/snap_sync.rs` | `SnapSyncStage`: pipeline-driven sync stage |

---

## Status

**Wired and tested.** Capability advertisement (`with_snap`, default-off); the dedicated
`EthSnapStream` (standalone demux + concurrent handshake) and the `EthSnapMessage` enum;
version-aware decode (`decode_versioned`, rejecting `0x06`/`0x07`); negotiation into
`EthRlpxConnection::EthSnap` and per-message dispatch in the active session; BAL serve helpers, BAL
fetch + verification, and catch-up chunking.

**Planned.** Routing `SnapClient` requests through the `SessionManager` to a snap-capable session and
correlating responses (`on_incoming_snap_message` currently logs and drops); serving inbound requests
from the BAL/state provider inside the session; the account/storage range download state machine,
range-proof verification, snapshot persistence, ordered BAL application, and the final state-root
check inside `SnapSyncStage`.
