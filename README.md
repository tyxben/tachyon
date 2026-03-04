# Tachyon

**High-Performance Matching Engine in Rust**

A Rust-based, ultra-low-latency order matching engine inspired by [Hyperliquid](https://hyperliquid.xyz/)'s architecture. Designed for building exchange infrastructure with sub-microsecond performance.

## Benchmark Results

| Metric | Measured |
|--------|----------|
| Order placement | ~54ns |
| Mixed workload throughput | ~6.7M ops/sec |
| BBO query | sub-nanosecond |
| SPSC push/pop | ~11ns |

## Key Features

- **Price-Time Priority CLOB** -- Central Limit Order Book with deterministic matching
- **Lock-Free Architecture** -- SPSC queues, atomic operations, zero mutex on hot path
- **Zero-Allocation Hot Path** -- Slab allocators, SmallVec, pre-allocated buffers
- **Thread-per-Symbol** -- Each trading pair runs on a dedicated, CPU-pinned OS thread
- **Deterministic Replay** -- WAL stores commands (input), not events (output); same input sequence = identical state
- **Crash Recovery** -- Snapshots + WAL replay for fast, correct recovery
- **Full API Stack** -- REST (axum), WebSocket (real-time depth/trades/ticker), TCP binary protocol
- **Embeddable** -- Use as a Rust crate or deploy as a standalone server

## Architecture

```
  Client ──> Gateway (tokio) ──> SPSC Queue ──> Engine Thread (CPU-pinned)
              REST/WS/TCP          lock-free      SymbolEngine (pure state machine)
                                                   │
                                                   ├─> Events ──> Client Response
                                                   └─> WAL (write-ahead, pre-processing)
```

**Core design**: The engine is a pure state machine -- `Command in, Events out`. No internal clocks, no shared mutable state. This enables deterministic replay: given the same command sequence, the engine always produces identical output and identical book state.

See [Architecture Document](docs/ARCHITECTURE.md) for detailed technical design.

## Supported Order Types

| Type | Status |
|------|--------|
| Limit Order (GTC) | Implemented |
| Market Order | Implemented |
| IOC (Immediate-or-Cancel) | Implemented |
| FOK (Fill-or-Kill) | Implemented |
| Post-Only | Implemented |
| Self-Trade Prevention | Implemented (CancelNewest, CancelOldest, CancelBoth) |

## Project Structure

```
thelight/
├── crates/
│   ├── tachyon-core/        # Core types (Price, Quantity, Order, Event, SymbolConfig)
│   ├── tachyon-book/        # OrderBook (BTreeMap + Slab + intrusive linked list)
│   ├── tachyon-engine/      # SymbolEngine (matcher + risk + STP) + Command types
│   ├── tachyon-io/          # Lock-free SPSC + MPSC queues
│   ├── tachyon-proto/       # Compact binary wire protocol (TCP)
│   ├── tachyon-gateway/     # REST + WebSocket + TCP server + EngineBridge
│   ├── tachyon-persist/     # WAL (v2 command-based) + Snapshots + Recovery
│   └── tachyon-bench/       # Benchmark suite (Criterion)
├── bin/
│   └── tachyon-server/      # Standalone server binary (config, metrics, Dockerfile)
└── docs/
    ├── PRD.md               # Product requirements
    ├── ARCHITECTURE.md      # Technical architecture (detailed, Chinese + English)
    ├── ROADMAP.md           # Development phases and status
    └── COMPETITIVE_ANALYSIS.md
```

## Getting Started

```bash
# Build
cargo build --release

# Run all tests (356 tests)
cargo test --all

# Run benchmarks
cargo bench --all

# Start server (default config)
cargo run --release --bin tachyon-server

# Start server (custom config)
cargo run --release --bin tachyon-server -- config/default.toml
```

## Persistence & Recovery

Tachyon uses a write-ahead log for durability:

1. **WAL writes commands before processing** -- true write-ahead semantics
2. **Periodic snapshots** capture full engine state (orders, counters)
3. **Recovery** loads latest snapshot, then replays WAL commands deterministically

WAL v2 format stores `WalCommand` (input commands) instead of the legacy v1 format that stored `EngineEvent` (output events). This makes recovery simple and deterministic -- just replay the commands through the engine.

## Documentation

- [Product Requirements](docs/PRD.md)
- [Architecture](docs/ARCHITECTURE.md)
- [Roadmap](docs/ROADMAP.md)
- [Competitive Analysis](docs/COMPETITIVE_ANALYSIS.md)

## License

[MIT](LICENSE)
