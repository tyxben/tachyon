# Tachyon ⚡

**高性能撮合交易引擎 | High-Performance Matching Engine**

A Rust-based, ultra-low-latency order matching engine inspired by [Hyperliquid](https://hyperliquid.xyz/)'s architecture. Designed for building exchange infrastructure with microsecond-level performance.

---

## Performance Targets

| Metric | Target |
|--------|--------|
| Matching Latency (median) | < 100ns |
| Matching Latency (P99) | < 10μs |
| Matching Latency (P99.9) | < 50μs |
| Throughput (single symbol) | 5M+ ops/sec |
| Throughput (total) | 20M+ ops/sec |

## Key Features

- **Price-Time Priority CLOB** — Central Limit Order Book with deterministic matching
- **Lock-Free Architecture** — SPSC queues, atomic operations, zero mutex on hot path
- **Zero-Allocation Hot Path** — Object pools, arena allocators, pre-allocated buffers
- **Thread-per-Symbol** — Each trading pair runs on a dedicated, CPU-pinned thread
- **Event Sourcing** — All state changes expressed as events, supporting replay and recovery
- **Embeddable** — Use as a Rust crate or deploy as a standalone server

## Supported Order Types

| Type | Status |
|------|--------|
| Limit Order (GTC) | Phase 1 |
| Market Order | Phase 1 |
| IOC (Immediate-or-Cancel) | Phase 1 |
| FOK (Fill-or-Kill) | Phase 1 |
| Post-Only | Phase 2 |
| Stop Limit / Stop Market | Phase 2 |
| Iceberg, Trailing Stop | Phase 5 |
| TWAP, Scaled Order | Phase 5 |

## Architecture

```
  Orders ──► Gateway ──► Sequencer ──► Matching Engine ──► Events
              (async)    (lock-free)    (per-symbol,       (WAL +
                                         CPU-pinned)        Stream)
```

See [Architecture Document](docs/ARCHITECTURE.md) for detailed technical design.

## Project Structure

```
thelight/
├── crates/
│   ├── tachyon-core/        # Core types (Price, Order, Event, etc.)
│   ├── tachyon-book/        # Order book implementation
│   ├── tachyon-engine/      # Matching engine + sequencer
│   ├── tachyon-io/          # Lock-free queues and ring buffers
│   ├── tachyon-gateway/     # gRPC, WebSocket, REST APIs
│   ├── tachyon-persist/     # WAL + Snapshot persistence
│   └── tachyon-bench/       # Benchmark suite
├── bin/
│   └── tachyon-server/      # Standalone server binary
└── docs/
    ├── PRD.md               # Product requirements
    ├── ARCHITECTURE.md      # Technical architecture
    ├── ROADMAP.md           # Development roadmap
    └── COMPETITIVE_ANALYSIS.md  # Competitive analysis
```

## Getting Started

```bash
# Build
cargo build --release

# Run tests
cargo test --all

# Run benchmarks
cargo bench --all

# Start server
cargo run --release --bin tachyon-server
```

## Documentation

- [Product Requirements](docs/PRD.md) — What we're building and why
- [Architecture](docs/ARCHITECTURE.md) — How it works under the hood
- [Roadmap](docs/ROADMAP.md) — Development phases and milestones
- [Competitive Analysis](docs/COMPETITIVE_ANALYSIS.md) — How Tachyon compares to alternatives

## License

[MIT](LICENSE)
