# 第八章：持久化与恢复 — 确定性重放

> **源码路径**: `crates/tachyon-persist/src/`
>
> **前置阅读**: [第4章：撮合引擎](./04-matching.md), [第7章：系统集成](./07-integration.md)

交易系统的持久化不是可选项——每一笔订单、每一次撮合都必须经得起崩溃的考验。本章将深入 Tachyon 的 WAL（Write-Ahead Log）、快照、恢复机制，以及为什么我们选择"命令重放"而非"事件存储"来实现确定性恢复。

---

## 目录

- [8.1 WAL 设计 — 为什么先写后处理](#81-wal-设计--为什么先写后处理)
- [8.2 WAL v1 vs v2 — 事件到命令的演进](#82-wal-v1-vs-v2--事件到命令的演进)
- [8.3 WAL 二进制格式](#83-wal-二进制格式)
- [8.4 CRC32 完整性校验](#84-crc32-完整性校验)
- [8.5 WAL 轮转](#85-wal-轮转)
- [8.6 Fsync 策略 — 持久性与延迟的权衡](#86-fsync-策略--持久性与延迟的权衡)
- [8.7 快照 — 原子写入与状态捕获](#87-快照--原子写入与状态捕获)
- [8.8 恢复流程 — 从快照到重放](#88-恢复流程--从快照到重放)
- [8.9 确定性重放 — 为什么必须是命令](#89-确定性重放--为什么必须是命令)
- [8.10 历史交易存储 — TradeStore](#810-历史交易存储--tradestore)

---

## 8.1 WAL 设计 — 为什么先写后处理

WAL（Write-Ahead Log）的核心原则：**先持久化命令，再执行命令**。

```
命令到达 → WAL.append() → fsync → engine.process() → 返回结果
           ▲                       ▲
           │                       │
     如果这里崩溃：              如果这里崩溃：
     命令已持久化，              命令已持久化，
     重启后可重放               重启后可重放
```

如果不用 WAL，执行顺序是 `process → response`。一旦进程在 process 和 response 之间崩溃，客户端不知道订单是否已被执行。使用 WAL 后：

1. 命令写入磁盘（append + 可选 fsync）
2. 引擎处理命令
3. 返回结果给客户端

崩溃恢复时，重放 WAL 中的命令即可恢复到崩溃前的精确状态。

在引擎主循环中的实际代码：

```rust
// bin/tachyon-server/src/main.rs:642-658
for cmd in batch {
    // 先写 WAL
    if let Some(ref wal) = ctx.wal_writer {
        if let Ok(ref mut writer) = wal.lock() {
            let seq = ctx.wal_sequence.fetch_add(1, Ordering::Relaxed) + 1;
            let wal_cmd = WalCommand {
                sequence: seq,
                symbol_id: cmd.symbol.raw(),
                command: cmd.command.clone(),
                account_id: cmd.account_id,
                timestamp: cmd.timestamp,
            };
            writer.append_command(&wal_cmd)?;
        }
    }

    // 再处理
    let events = ctx.engine.process_command(cmd.command, cmd.account_id, cmd.timestamp);
}
```

### 全局序列号

所有品种共享一个全局 WAL 序列号，使用原子计数器保证唯一递增：

```rust
// main.rs:157
let wal_sequence = Arc::new(AtomicU64::new(0));

// 引擎线程中
let seq = ctx.wal_sequence.fetch_add(1, Ordering::Relaxed) + 1;
```

`Ordering::Relaxed` 已经足够——序列号只需要全局唯一递增，不需要和其他变量建立 happens-before 关系。

---

## 8.2 WAL v1 vs v2 — 事件到命令的演进

Tachyon 经历了一次重要的 WAL 格式升级：

| | v1（旧） | v2（当前） |
|---|---------|-----------|
| 存储内容 | `EngineEvent`（事件） | `Command`（命令） |
| 重放方式 | 推断式恢复 | 确定性重放 |
| 格式标志 | 无版本字节 | 首字节 = `0x02` |
| 序列化 | bincode | bincode |

### 为什么从事件升级到命令？

**v1 的问题：存储事件（Output）**

```
PlaceOrder → [OrderAccepted, Trade, Trade, BookUpdate]
```

事件是引擎的输出。恢复时需要"逆向工程"这些事件来重建订单簿，逻辑复杂且容易出错：

```rust
// main.rs:54-113 — v1 的 legacy 恢复代码（复杂）
fn replay_legacy_wal_event(engines: &mut HashMap<u32, SymbolEngine>, event: &EngineEvent) {
    match event {
        EngineEvent::OrderAccepted { .. } => {
            // 从事件推断订单属性，插入订单簿
        }
        EngineEvent::Trade { trade } => {
            // 从成交推断 maker 剩余数量，更新订单簿
        }
        EngineEvent::OrderCancelled { order_id, .. } => {
            // 遍历所有引擎查找该订单
        }
    }
}
```

**v2 的方案：存储命令（Input）**

```
WAL 记录: PlaceOrder(order) with timestamp T
恢复时: engine.process_command(PlaceOrder(order), account_id, T)
```

重放命令就像"重新运行"引擎，天然保证结果一致。代码简洁到只有一行：

```rust
// main.rs:191-198
for wal_cmd in &state.wal_commands {
    engine.process_command(wal_cmd.command.clone(), wal_cmd.account_id, wal_cmd.timestamp);
}
```

### 向后兼容

WalReader 通过首字节区分版本：

```rust
// wal.rs:301-317
let mut first_byte = [0u8; 1];
self.reader.read_exact(&mut first_byte)?;

if first_byte[0] == 2 {  // WAL_VERSION_COMMANDS
    self.read_v2_entry()  // 新格式
} else {
    self.read_v1_entry(first_byte[0])  // 首字节是 length 的一部分
}
```

v1 条目没有版本字节，它的首字节是 `u32 length` 字段的低位。由于 WAL 条目长度远大于 2，这个字节不会是 `0x02`，因此可以安全区分。

---

## 8.3 WAL 二进制格式

### v2 格式（当前）

```
┌────────┬─────────┬──────────┬───────────────────┬──────────┐
│version │ length  │ sequence │     payload        │  crc32   │
│  u8    │ u32 LE  │ u64 LE   │  bincode(WalCmd)   │ u32 LE   │
│  =0x02 │         │          │                    │          │
├────────┼─────────┼──────────┼───────────────────┼──────────┤
│ 1 byte │ 4 bytes │ 8 bytes  │  N bytes           │ 4 bytes  │
└────────┴─────────┴──────────┴───────────────────┴──────────┘
         ◀────────── length 覆盖范围 ────────────▶
         (= 8 + N + 4)

CRC32 校验范围: version + sequence + payload
```

每个字段的含义：

| 字段 | 大小 | 说明 |
|------|------|------|
| `version` | 1 byte | 固定 `0x02` |
| `length` | 4 bytes (LE) | sequence + payload + crc 的总长度 |
| `sequence` | 8 bytes (LE) | 全局递增序列号 |
| `payload` | N bytes | bincode 序列化的 `WalCommand` |
| `crc32` | 4 bytes (LE) | CRC32 校验码 |

### v1 格式（遗留）

```
┌─────────┬──────────┬───────────────────┬──────────┐
│ length  │ sequence │     payload        │  crc32   │
│ u32 LE  │ u64 LE   │ bincode(Event)     │ u32 LE   │
├─────────┼──────────┼───────────────────┼──────────┤
│ 4 bytes │ 8 bytes  │  N bytes           │ 4 bytes  │
└─────────┴──────────┴───────────────────┴──────────┘

CRC32 校验范围: sequence + payload（注意：不含 version 字节）
```

### WalCommand 结构

```rust
// crates/tachyon-persist/src/wal.rs:33-40
pub struct WalCommand {
    pub sequence: u64,      // 全局序列号
    pub symbol_id: u32,     // 品种 ID
    pub command: Command,   // PlaceOrder / CancelOrder / ModifyOrder
    pub account_id: u64,    // 账户 ID
    pub timestamp: u64,     // 纳秒时间戳
}
```

---

## 8.4 CRC32 完整性校验

每条 WAL 条目都附带 CRC32 校验码，用于检测磁盘损坏、部分写入等问题。

### 写入时计算

```rust
// wal.rs:136-140
let mut hasher = crc32fast::Hasher::new();
hasher.update(&[WAL_VERSION_COMMANDS]);    // 版本字节
hasher.update(&wal_cmd.sequence.to_le_bytes()); // 序列号
hasher.update(&payload);                        // 有效载荷
let crc = hasher.finalize();
```

### 读取时验证

```rust
// wal.rs:354-366
let mut hasher = crc32fast::Hasher::new();
hasher.update(&[WAL_VERSION_COMMANDS]);
hasher.update(&data[..8]);  // sequence bytes
hasher.update(payload);
let computed_crc = hasher.finalize();

if stored_crc != computed_crc {
    return Err(WalError::CrcMismatch {
        expected: stored_crc,
        actual: computed_crc,
    });
}
```

### CRC 能检测什么？

| 场景 | 能否检测 |
|------|---------|
| 单 bit 翻转 | 能 |
| 断电导致的部分写入 | 能（length 和 CRC 不匹配） |
| 磁盘坏道导致的数据损坏 | 能 |
| 恶意篡改 | 不能（CRC 不是加密哈希） |

CRC32 使用 `crc32fast` 库，在 x86 上利用 SSE 4.2 的硬件 CRC 指令，单次校验只需几纳秒。

---

## 8.5 WAL 轮转

当 WAL 文件超过配置的最大大小时，会自动轮转：

```rust
// wal.rs:248-277
fn rotate(&mut self) -> Result<()> {
    self.current_file.flush()?;
    self.current_file.get_ref().sync_data()?;

    // 重命名: wal_active.log → wal_{first_seq}_{last_seq}.log
    let rotated_name = format!("wal_{}_{}.log", self.first_seq, self.last_seq);
    fs::rename(&active_path, &rotated_path)?;

    // 打开新的 active 文件
    let file = OpenOptions::new().create(true).append(true).open(&active_path)?;
    self.current_file = BufWriter::new(file);
    self.current_size = 0;
}
```

文件命名规则：
```
wal/
├── wal_1_1000.log       # 序列号 1-1000
├── wal_1001_2000.log    # 序列号 1001-2000
└── wal_active.log       # 当前写入中
```

### 读取顺序

`list_wal_files()` 按首序列号排序轮转文件，`wal_active.log` 始终排在最后：

```rust
// wal.rs:453-486
pub fn list_wal_files(dir: impl AsRef<Path>) -> Result<Vec<PathBuf>> {
    // 1. 收集轮转文件，解析首序列号
    // 2. 按首序列号排序
    // 3. active 文件排在最后
    rotated.sort_by_key(|(seq, _)| *seq);
    if let Some(a) = active { result.push(a); }
    Ok(result)
}
```

---

## 8.6 Fsync 策略 — 持久性与延迟的权衡

Tachyon 提供三种 fsync 策略，让用户根据场景选择持久性与性能的平衡点：

| 策略 | 行为 | 延迟 | 持久性 |
|------|------|------|--------|
| `Sync` | 每条命令后 fsync | ~10-100us | 最高：不丢失任何命令 |
| `Batched { count: N }` | 每 N 条命令后 fsync | ~1-10us 摊薄 | 中等：最多丢失 N-1 条命令 |
| `Async` | 不主动 fsync | ~1us | 最低：依赖 OS 页缓存 |

```rust
// wal.rs:160-174
match &self.fsync_strategy {
    FsyncStrategy::Sync => {
        self.current_file.flush()?;
        self.current_file.get_ref().sync_data()?;  // fdatasync
        self.writes_since_sync = 0;
    }
    FsyncStrategy::Batched { count } => {
        if self.writes_since_sync >= *count {
            self.current_file.flush()?;
            self.current_file.get_ref().sync_data()?;
            self.writes_since_sync = 0;
        }
    }
    FsyncStrategy::Async => {}  // 什么都不做
}
```

### 选择建议

- **生产环境（资金安全）**: `Sync` 或 `Batched { count: 10 }`
- **回测/模拟**: `Async`（最大吞吐）
- **折中**: `Batched { count: 100 }`（每 100 条 fsync 一次，极端情况最多丢 99 条）

注意 `sync_data()` 调用的是 `fdatasync`（只刷数据，不刷 metadata），比 `sync_all()`（= `fsync`）更快。

---

## 8.7 快照 — 原子写入与状态捕获

快照捕获某一时刻的完整订单簿状态，用于加速恢复（不需要从头重放所有 WAL）。

### 快照内容

```rust
// crates/tachyon-persist/src/snapshot.rs:37-58
pub struct Snapshot {
    pub sequence: u64,              // 对应的 WAL 序列号
    pub timestamp: u64,             // 快照时间（纳秒）
    pub symbols: Vec<SymbolSnapshot>,
}

pub struct SymbolSnapshot {
    pub symbol: Symbol,
    pub config: SymbolConfig,
    pub orders: Vec<Order>,         // 所有活跃订单
    pub next_order_id: u64,         // 下一个订单 ID
    pub sequence: u64,              // 引擎序列号
    pub trade_id_counter: u64,      // 成交 ID 计数器
}
```

快照不仅存储订单数据，还存储引擎的内部计数器（`next_order_id`, `sequence`, `trade_id_counter`），确保恢复后 ID 不会重复。

### 原子写入

快照使用 "临时文件 + 重命名" 模式保证原子性：

```rust
// snapshot.rs:68-94
pub fn write(dir: &Path, snapshot: &Snapshot) -> Result<PathBuf> {
    let payload = bincode::serialize(snapshot)?;
    let crc = crc32fast::hash(&payload);

    let temp_path = dir.join(format!(".snapshot_{}.tmp", snapshot.sequence));
    let final_path = dir.join(format!("snapshot_{}.bin", snapshot.sequence));

    // 1. 写入临时文件
    let mut file = File::create(&temp_path)?;
    file.write_all(&payload)?;
    file.write_all(&crc.to_le_bytes())?;
    file.sync_all()?;  // 确保数据落盘

    // 2. 原子重命名
    fs::rename(&temp_path, &final_path)?;

    Ok(final_path)
}
```

**为什么这是原子的？**

在 POSIX 文件系统上，`rename()` 是原子操作。在任何时刻：
- 要么只有 `.tmp` 文件（写入中/写入失败）
- 要么只有 `.bin` 文件（写入完成）
- 不会出现半写入的 `.bin` 文件

### 快照文件格式

```
┌──────────────────────────┬──────────┐
│   bincode(Snapshot)      │  crc32   │
│   N bytes                │  u32 LE  │
└──────────────────────────┴──────────┘

文件名: snapshot_{sequence}.bin
```

### 定期快照触发

在引擎主循环中，每处理一定数量的事件后自动创建快照：

```rust
// main.rs:838-874
if ctx.snapshot_interval_events > 0
    && events_since_snapshot >= ctx.snapshot_interval_events
{
    let engine_snap = ctx.engine.snapshot_full_state();
    let snapshot = Snapshot { sequence: current_seq, timestamp, symbols: vec![sym_snapshot] };
    SnapshotWriter::write(snap_dir, &snapshot)?;
    events_since_snapshot = 0;
}
```

### 旧快照清理

`cleanup_old_snapshots()` 只保留最新的 N 个快照，删除旧的：

```rust
// snapshot.rs:161-194
pub fn cleanup_old_snapshots(dir: &Path, keep: usize) -> Result<()> {
    // 按序列号排序，删除最旧的
    snapshots.sort_by_key(|(seq, _)| *seq);
    let to_remove = snapshots.len() - keep;
    for (_, path) in snapshots.into_iter().take(to_remove) {
        fs::remove_file(&path)?;
    }
}
```

---

## 8.8 恢复流程 — 从快照到重放

`RecoveryManager` 负责崩溃后的状态恢复，流程如下：

```
                 ┌──────────────────────────────────────────┐
                 │          RecoveryManager::recover()       │
                 └───────────────────┬──────────────────────┘
                                     │
                    ┌────────────────▼──────────────────┐
                    │  1. 查找最新快照                     │
                    │     find_latest_snapshot()          │
                    │     → snapshot_100.bin (seq=100)    │
                    └────────────────┬──────────────────┘
                                     │
                    ┌────────────────▼──────────────────┐
                    │  2. 加载快照                        │
                    │     SnapshotReader::read()          │
                    │     → orders, counters              │
                    └────────────────┬──────────────────┘
                                     │
                    ┌────────────────▼──────────────────┐
                    │  3. 读取所有 WAL 文件               │
                    │     list_wal_files()                │
                    │     → [wal_1_50.log, wal_51_100.log,│
                    │        wal_101_150.log, active.log] │
                    └────────────────┬──────────────────┘
                                     │
                    ┌────────────────▼──────────────────┐
                    │  4. 过滤 WAL 条目                   │
                    │     跳过 seq <= 100（已在快照中）    │
                    │     保留 seq 101-150 + active       │
                    └────────────────┬──────────────────┘
                                     │
                    ┌────────────────▼──────────────────┐
                    │  5. 返回 RecoveryState              │
                    │     .snapshots: 快照中的订单/计数器  │
                    │     .wal_commands: 需要重放的命令    │
                    │     .last_sequence: 最后序列号       │
                    └──────────────────────────────────┘
```

实际恢复代码（在 `main.rs` 中）：

```rust
// main.rs:160-225
let recovery_mgr = RecoveryManager::new(&pc.snapshot_dir, &pc.wal_dir);
match recovery_mgr.recover() {
    Ok(state) => {
        // 1. 从快照恢复订单簿
        for (symbol, sym_state) in &state.snapshots {
            if let Some(engine) = engines.get_mut(&symbol.raw()) {
                for order in &sym_state.orders {
                    engine.book_mut().restore_order(order.clone())?;
                }
                // 恢复引擎计数器
                engine.set_next_order_id(sym_state.next_order_id);
                engine.set_sequence(sym_state.sequence);
                engine.set_trade_id_counter(sym_state.trade_id_counter);
            }
        }

        // 2. 重放 v2 WAL 命令（确定性）
        for wal_cmd in &state.wal_commands {
            if let Some(engine) = engines.get_mut(&wal_cmd.symbol_id) {
                engine.process_command(wal_cmd.command.clone(), ...);
            }
        }

        // 3. 重放 v1 遗留事件（向后兼容）
        for (_seq, event) in &state.legacy_wal_events {
            replay_legacy_wal_event(&mut engines, event);
        }

        // 4. 恢复全局 WAL 序列号
        wal_sequence.store(state.last_sequence, Ordering::Release);
    }
}
```

### WAL 过滤逻辑

```rust
// crates/tachyon-persist/src/recovery.rs:126-149
for entry_result in reader {
    let entry = entry_result?;
    let seq = entry.sequence();

    // 跳过已被快照覆盖的条目
    if seq <= snapshot_seq {
        continue;
    }

    match entry {
        WalEntry::Command(wal_cmd) => {
            state.wal_commands.push(wal_cmd);
        }
        WalEntry::LegacyEvent { sequence, data } => {
            let event = bincode::deserialize(&data)?;
            state.legacy_wal_events.push((sequence, event));
        }
    }
}
```

---

## 8.9 确定性重放 — 为什么必须是命令

**确定性重放**意味着：给定相同的命令序列和时间戳，引擎必须产生完全相同的输出。

### 为什么命令而非事件？

假设我们有以下命令序列：

```
CMD 1: PlaceOrder(Buy 100@50000, account=1, ts=T1)
CMD 2: PlaceOrder(Sell 50@50000, account=2, ts=T2)
CMD 3: CancelOrder(order_id=1)
```

**命令重放**：将这三条命令原样输入引擎，引擎自动产生正确的事件（Accept, Trade, Cancel），订单簿状态自然正确。

**事件重放**（v1 的方式）：需要从事件反推订单簿状态：
- OrderAccepted → 需要构造完整 Order 结构体
- Trade → 需要推算 maker 的剩余数量
- OrderCancelled → 需要遍历所有引擎查找该订单

命令重放是"正向执行"，事件重放是"逆向推导"——前者天然正确，后者容易出错。

### 时间戳确定性

注意 `WalCommand` 中保存了原始的 `timestamp`：

```rust
pub struct WalCommand {
    pub timestamp: u64,  // 原始请求的纳秒时间戳
    ...
}
```

重放时使用记录的时间戳（而非当前时间），确保以下行为一致：
- GTD（Good Till Date）订单的过期判断
- 订单的时间优先排序
- 事件中的时间戳字段

### 验证确定性

Tachyon 的测试直接验证了确定性重放：

```rust
// recovery.rs tests: test_recovery_deterministic_replay
// Run 1: 处理命令，记录所有事件
let mut engine1 = SymbolEngine::new(...);
for cmd in &commands {
    all_events1.extend(engine1.process_command(cmd.command.clone(), ...));
}

// Run 2: 完全相同的命令序列
let mut engine2 = SymbolEngine::new(...);
for cmd in &commands {
    all_events2.extend(engine2.process_command(cmd.command.clone(), ...));
}

// 两次运行必须产生完全相同的事件
assert_eq!(all_events1, all_events2);
// 两个引擎必须有完全相同的状态
assert_eq!(engine1.book().order_count(), engine2.book().order_count());
assert_eq!(engine1.sequence(), engine2.sequence());
```

---

## 8.10 历史交易存储 — TradeStore

除了 WAL（用于恢复），Tachyon 还维护一个独立的历史交易存储，用于 K 线聚合和历史查询。

### 56 字节定长记录

```rust
// crates/tachyon-persist/src/trade_store.rs:22-42
#[repr(C)]
pub struct TradeRecord {
    pub timestamp: u64,        // 8 bytes  — 纳秒时间戳
    pub trade_id: u64,         // 8 bytes  — 成交 ID
    pub price: i64,            // 8 bytes  — 定点价格
    pub quantity: u64,         // 8 bytes  — 定点数量
    pub maker_order_id: u64,   // 8 bytes  — maker 订单 ID
    pub taker_order_id: u64,   // 8 bytes  — taker 订单 ID
    pub symbol_id: u32,        // 4 bytes  — 品种 ID
    pub maker_side: u8,        // 1 byte   — 0=Buy, 1=Sell
    pub _pad: [u8; 3],         // 3 bytes  — 对齐填充
}                              // 总计: 56 bytes

// 编译时断言确保大小正确
const _: () = assert!(std::mem::size_of::<TradeRecord>() == 56);
```

**为什么用定长记录？**

```
文件偏移 = record_index * 56
```

定长记录意味着 O(1) 随机访问——不需要索引文件，直接通过偏移量定位。这对二分搜索至关重要。

### 二分搜索时间范围

由于引擎按顺序处理命令，成交记录天然按时间排序。这意味着可以直接在文件上做二分搜索：

```rust
// trade_store.rs:178-199
pub fn lower_bound(&self, target_ts: u64) -> io::Result<u64> {
    let mut lo: u64 = 0;
    let mut hi: u64 = self.record_count;

    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        // 只读取 timestamp 字段（前 8 字节）
        file.seek(SeekFrom::Start(mid * RECORD_SIZE as u64))?;
        let mut ts_buf = [0u8; 8];
        file.read_exact(&mut ts_buf)?;
        let ts = u64::from_le_bytes(ts_buf);

        if ts < target_ts {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    Ok(lo)
}
```

时间范围查询 [start, end) 只需两次二分搜索 + 一次顺序读取：

```rust
pub fn query_time_range(&self, start: u64, end: u64, limit: usize) -> Vec<TradeRecord> {
    let start_idx = self.lower_bound(start)?;
    let end_idx = self.upper_bound(end - 1)?;
    self.read_range(start_idx, end_idx.min(start_idx + limit))
}
```

对于 100 万条记录，二分搜索只需 ~20 次磁盘读取（每次只读 8 字节）。

### K 线聚合

TradeStore 支持直接在磁盘记录上计算 OHLCV K 线：

```rust
// trade_store.rs:288-343
pub fn compute_klines(&self, start: u64, end: u64, interval_ns: u64, limit: usize)
    -> Vec<Kline>
{
    let records = self.read_range(start_idx, end_idx)?;
    for record in &records {
        let bucket_start = (record.timestamp / interval_ns) * interval_ns;
        if let Some(last) = klines.last_mut() {
            if last.open_time == bucket_start {
                // 更新当前蜡烛图
                last.high = last.high.max(record.price);
                last.low = last.low.min(record.price);
                last.close = record.price;
                last.volume += record.quantity;
                last.trade_count += 1;
                continue;
            }
        }
        // 新蜡烛图
        klines.push(Kline { open: record.price, high: record.price, ... });
    }
}
```

支持的时间间隔：

| 格式 | 含义 | 纳秒数 |
|------|------|--------|
| `1s` | 1 秒 | 1,000,000,000 |
| `1m` | 1 分钟 | 60,000,000,000 |
| `5m` | 5 分钟 | 300,000,000,000 |
| `1h` | 1 小时 | 3,600,000,000,000 |
| `1d` | 1 天 | 86,400,000,000,000 |

### 异步写入管道

引擎线程（同步）通过 unbounded channel 将成交记录发送给异步写入任务，不阻塞撮合：

```
Engine thread ──unbounded_channel──▶ trade_history_writer task ──▶ disk
   (sync)           (non-blocking)        (async, batched)
```

```rust
// main.rs:890-924
async fn trade_history_writer(mut rx: UnboundedReceiver<TradeRecord>, store: Arc<TradeStore>) {
    loop {
        // 等待第一条记录
        let record = match rx.recv().await {
            Some(r) => r,
            None => break,  // 通道关闭
        };
        write_trade_record(&store, &mut writers, &record);

        // 非阻塞地读取所有已缓冲的记录
        while let Ok(record) = rx.try_recv() {
            write_trade_record(&store, &mut writers, &record);
        }

        // 批量 flush
        for writer in writers.values_mut() {
            writer.flush();
        }
    }
}
```

---

## 本章小结

| 组件 | 技术 | 关键指标 |
|------|------|---------|
| WAL | 追加写入 + CRC32 | 每条 ~1us (async) 到 ~100us (sync) |
| WAL 格式 | v2 命令 (bincode) | ~100-200 bytes/条 |
| 快照 | 原子写入 (tmp + rename) | 整个订单簿序列化 |
| 恢复 | 快照 + WAL 过滤 + 命令重放 | 确定性保证 |
| TradeStore | 56 字节定长 + 二分搜索 | O(log N) 时间查询 |
| K 线 | 流式 OHLCV 聚合 | 1s/1m/5m/15m/1h/4h/1d/1w |

### 恢复保证

| 场景 | 数据丢失 |
|------|---------|
| 进程崩溃 (Sync fsync) | 0 条命令 |
| 进程崩溃 (Batched N) | 最多 N-1 条 |
| 进程崩溃 (Async) | OS 页缓存中未刷盘的命令 |
| 磁盘损坏 | CRC32 检测，拒绝损坏条目 |
| 快照文件损坏 | CRC32 检测，回退到纯 WAL 重放 |

---

> **下一章**: [第九章：面试指南 — 高频交易系统面试全攻略](./09-interview.md)
