# snap/2 (EIP-8189)

snap/2 is a satellite `RLPx` sub-protocol for fast state sync. It is negotiated as its own capability and multiplexed alongside `eth` on the same connection. Messages `0x00..0x05` (account / storage / byte-code ranges) are unchanged from snap/1; trie nodes (`0x06`/`0x07`) are removed; block access lists (BALs) are added as `0x08`/`0x09`. State is acquired by downloading a snapshot at a frozen pivot and then applying verified BAL diffs up to the head, instead of re-executing history.

Spec: <https://eips.ethereum.org/EIPS/eip-8189>

## Network / protocol layer

```
        BEFORE (eth-only)                       AFTER (eth + snap/2 satellite)
┌──────────────────────────┐      ┌─────────────────────────────────────────────────┐
│ one RLPx conn / peer     │      │ one RLPx conn / peer (multiplexed)              │
│  ┌────────────────────┐  │      │  ┌──────────┐     ┌───────────────────────────┐ │
│  │ eth/6x..7x         │  │      │  │ eth (prim)│     │ snap/2 satellite         │ │
│  └────────────────────┘  │      │  └──────────┘     └───────────────────────────┘ │
│  snap = scaffolding,     │      │   RlpxSatelliteStream routes by capability      │
│  never negotiated        │      │      server: serve ranges + BAL  → bal_store    │
│                          │      │      client: SnapClient, request_id ↔ response  │
│  advertised: [eth]       │      │   advertised: [eth, snap/2]  (default-off)      │
└──────────────────────────┘      └─────────────────────────────────────────────────┘
```

## State acquisition / sync pipeline

```
   BEFORE — full execution sync            AFTER — snap/2 state sync (SnapSyncStage)

   Headers                                 Headers
   Bodies                                  Bodies
   SenderRecovery                          ┌────────────── SnapSync stage ──────────────┐
   Execution  ◄ replay EVERY tx            │ 1. freeze pivot (recent block)             │
      │        genesis→head to             │ 2. download account/storage/code ranges    │
      │        BUILD state trie            │    @ pivot state-root, verify range proofs  │
   MerkleExecute (state root)              │ 3. persist snapshot                         │
   Hashing / History / TxLookup           │ 4. BAL catch-up  pivot+1 ..= head, chunked: │
   Finish                                  │    GetBlockAccessLists → verify vs header   │
                                           │    block_access_list_hash → apply in order  │
   ▶ state = re-execute all history        │ 5. verify final state root                  │
     (CPU-bound)                           └─────────────────────────────────────────────┘
                                           Hashing / History / TxLookup ▶ Finish
                                           ▶ state = snapshot @ pivot + verified BAL diffs
                                             (network / IO-bound)
```

snap/1 healed the moving state target with trie-node requests (`0x06`/`0x07`); snap/2 replaces that with compact BAL diffs (`0x08`/`0x09`) applied in strict block order, each verified against the header's `block_access_list_hash`. Orchestration — pivot selection, restart on reorg, progress checkpoints — is owned by the staged sync pipeline, not the protocol, mirroring how geth drives its snap syncer from the downloader.

## Module map

| Path | Role |
| --- | --- |
| `mod.rs` | `SnapProtocolHandler` / `SnapConnectionHandler`: advertise + install the satellite |
| `connection.rs` | `SnapConnection`: serve inbound requests, correlate inbound responses by `request_id` |
| `peers.rs` | registry of connected snap/2 peers, shared with the client |
| `client.rs` | `SnapClient`: dispatch requests to peers, future per request |
| `bal.rs` | `fetch_and_verify_bals`: fetch BALs and verify them against headers |
| `verify.rs` | BAL hash + strict-order verification |
| `sync.rs` | pure sync helpers: progress descriptor, BAL catch-up chunking |
| `../../../../stages/stages/src/stages/snap_sync.rs` | `SnapSyncStage`: pipeline-driven sync stage |

Capability advertisement lives in `eth-wire`'s `HelloMessageWithProtocols::with_snap` and is toggled by `NetworkConfigBuilder::with_snap` (default-off).

## Status

Implemented and tested: capability advertisement, satellite negotiation/routing, the BAL server, the client with request/response correlation, BAL fetch + verification, and catch-up chunking. The `SnapSyncStage` is wired into the pipeline as a skeleton that makes no progress yet.

Remaining (inside `SnapSyncStage::execute` / `poll_execute_ready`): the account/storage range download state machine, range-proof verification, persisting the snapshot, applying BAL diffs to the trie/DB, and the final state-root check.
