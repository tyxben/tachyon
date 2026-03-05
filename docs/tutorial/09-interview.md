# 第九章：面试指南 — 高频交易系统面试全攻略

> 本章将 Tachyon 的技术栈转化为面试武器。每个问题都标注了难度、参考答案、关键词和对应的代码位置。

---

## 目录

- [9.1 系统设计面试题](#91-系统设计面试题)
- [9.2 数据结构与算法](#92-数据结构与算法)
- [9.3 并发与内存模型](#93-并发与内存模型)
- [9.4 操作系统与硬件](#94-操作系统与硬件)
- [9.5 Rust 特定](#95-rust-特定)
- [9.6 项目讲解技巧](#96-项目讲解技巧)

---

## 9.1 系统设计面试题

### Q1: "设计一个高性能撮合引擎"

**难度**: 高 | **关键词**: 价格-时间优先, per-symbol 线程, SPSC, WAL

**参考答案**:

```
整体架构:

           REST/WS/TCP 网关
                 │
          ┌──────┴──────┐
          │ EngineBridge │  (per-symbol SPSC queue)
          └──────┬──────┘
     ┌───────────┼───────────┐
     ▼           ▼           ▼
  Engine-0    Engine-1    Engine-2     (专用 OS 线程, CPU 绑核)
  (BTCUSDT)   (ETHUSDT)   (SOLUSDT)
     │           │           │
     ▼           ▼           ▼
   WAL        WAL         WAL          (先写后执行)
```

**核心设计决策**:

1. **线程模型**: 每个交易品种一个专用 OS 线程。不用 tokio 是因为异步运行时的 work-stealing 会引入不确定的调度延迟。专用线程 + CPU 亲和性绑核可以保证 L1/L2 cache 热度，实现 <100ns 中位数撮合延迟。

2. **数据结构**: 订单簿使用 BTreeMap<Price, PriceLevelIdx> 索引价格级别。每个价格级别内部用侵入式双向链表维护时间优先。订单存储在 Slab<Order> 中实现 O(1) 分配/释放。O(log P) 插入（P = 价格级别数），O(1) 取消。

3. **通信**: 网关（异步）和引擎（同步）通过无锁 SPSC 环形缓冲区桥接。SPSC 只需要 Release/Acquire 内存序，不需要 CAS，延迟极低。

4. **持久化**: Write-Ahead Log 记录原始命令（而非事件），支持确定性重放恢复。定期快照 + WAL 轮转控制恢复时间。

5. **网关层**: 三种协议——REST (JSON, 易用)、WebSocket (实时推送)、TCP 二进制 (最低延迟)。

**回答要点**: 先画架构图，再逐层展开。面试官通常会追问某一层的细节（如订单簿的数据结构或 WAL 的 fsync 策略），准备好深入。

**对应代码**: `bin/tachyon-server/src/main.rs` (系统编排), `crates/tachyon-engine/src/engine.rs` (撮合)

---

### Q2: "设计一个订单簿"

**难度**: 中 | **关键词**: BTreeMap, Slab, 侵入式链表, BBO 缓存

**参考答案**:

订单簿需要支持三个核心操作:
- **插入**: O(log P) — 在正确的价格级别追加订单
- **取消**: O(1) — 通过订单 ID 直接定位并删除
- **获取最优价**: O(1) — 实时维护 BBO (Best Bid/Offer)

**数据结构选择**:

```
HashMap<OrderId, SlabIdx>     ← O(1) 订单查找
         │
         ▼
Slab<Order>                   ← O(1) 分配/释放，内存连续
  order.prev / order.next     ← 侵入式链表（u32 索引，非指针）
         │
         ▼
BTreeMap<Price, PriceLevelIdx> ← O(log P) 有序价格级别
         │
         ▼
Slab<PriceLevel>              ← head/tail 指向链表首尾
```

**为什么不用 `std::collections::LinkedList<T>`?**

`LinkedList` 每个节点独立堆分配，cache 不友好。侵入式链表将 prev/next 嵌入 Order 结构体中（u32 索引指向 Slab），所有订单存储在连续内存的 Slab 中，cache 命中率高。

**为什么 BBO 用 `Option<Price>` 缓存?**

`BTreeMap::iter().next()` 虽然是 O(log N)，但在热路径上仍然太慢。维护一个 `best_bid: Option<Price>` 在价格级别变更时增量更新，查询 BBO 降为 O(1)。

**对应代码**: `crates/tachyon-book/src/book.rs`

---

### Q3: "设计一个低延迟消息队列"

**难度**: 中 | **关键词**: SPSC, 环形缓冲区, 2的幂, Release/Acquire

**参考答案**:

对于单生产者-单消费者（SPSC）场景，可以使用无锁环形缓冲区：

```rust
struct SpscQueue<T> {
    buffer: Box<[MaybeUninit<UnsafeCell<T>>]>,
    capacity: usize,           // 必须是 2 的幂
    head: CachePadded<AtomicU64>,  // 消费者写，生产者读
    tail: CachePadded<AtomicU64>,  // 生产者写，消费者读
}
```

**关键设计**:

1. **2 的幂容量**: `index = seq & (capacity - 1)` 用位运算替代取模，节省 ~3ns/op
2. **Cache line padding**: head 和 tail 分别 padding 到 64 字节，防止 false sharing
3. **内存序**: 生产者 `tail.store(Release)`，消费者 `head.load(Acquire)` — 不需要 SeqCst
4. **批量操作**: `drain_batch()` 一次取出多条消息，摊薄原子操作开销

**MPSC 扩展**: 多生产者需要 CAS 循环竞争 tail，或者使用 per-producer SPSC + 消费者轮询的方案。

**对应代码**: `crates/tachyon-io/src/spsc.rs`, `crates/tachyon-io/src/mpsc.rs`

---

### Q4: "如何实现故障恢复"

**难度**: 高 | **关键词**: WAL, 快照, 确定性重放, CRC32, 原子写入

**参考答案**:

三层恢复机制：

```
恢复流程:
1. 加载最新快照 (snapshot_100.bin)
   → 恢复订单簿 + 引擎计数器
2. 读取快照之后的 WAL 条目 (seq > 100)
   → 过滤已被快照覆盖的条目
3. 确定性重放 WAL 命令
   → engine.process_command(cmd, account_id, timestamp)
```

**关键设计决策**:

| 决策 | 选择 | 原因 |
|------|------|------|
| WAL 内容 | 命令（输入）而非事件（输出） | 正向重放天然正确，不需要逆向推导 |
| 快照写入 | tmp 文件 + 原子 rename | 防止半写入的损坏文件 |
| 完整性 | CRC32 每条 WAL + 每个快照 | 检测磁盘损坏和部分写入 |
| fsync 策略 | 可配置 (Sync/Batched/Async) | 用户选择持久性 vs 性能 |
| 时间戳 | 记录原始时间戳 | 确保重放时行为一致（如 GTD 过期） |

**为什么快照 + WAL 比纯 WAL 好？**

纯 WAL 恢复需要从头重放所有命令，如果有 1 亿条命令，恢复时间不可接受。快照提供检查点，只需重放快照之后的增量命令。

**对应代码**: `crates/tachyon-persist/src/recovery.rs`, `crates/tachyon-persist/src/wal.rs`, `crates/tachyon-persist/src/snapshot.rs`

---

### Q5: "设计实时市场数据推送系统"

**难度**: 中 | **关键词**: broadcast channel, 订阅过滤, 慢消费者, 连接限制

**参考答案**:

```
Engine thread ─broadcast::send()─▶ WsState
                                       │
                        ┌──────────────┼──────────────┐
                        ▼              ▼              ▼
                    Client A       Client B       Client C
                 (trades@BTC)   (depth@ETH)    (ticker@BTC)
```

使用 tokio `broadcast` channel 实现一对多推送。每个客户端维护自己的订阅集合，只接收匹配的消息。

慢消费者策略：broadcast channel 报告 `Lagged` 时发送警告，连续 3 次 lag 断开连接。比静默丢弃更友好。

连接数限制使用原子 increment-then-check 避免 TOCTOU 竞态：先 `fetch_add(1)`，如果超限则 `fetch_sub(1)` 回滚。

**对应代码**: `crates/tachyon-gateway/src/ws.rs`

---

## 9.2 数据结构与算法

### Q1: 价格-时间优先撮合的时间复杂度是多少？

**难度**: 初 | **关键词**: O(log P), O(1), BTreeMap, 侵入式链表

**参考答案**:

| 操作 | 复杂度 | 说明 |
|------|--------|------|
| 下单 | O(log P) | P = 价格级别数，BTreeMap 插入 |
| 取消 | O(1) | HashMap 查找 + Slab 索引 + 链表删除 |
| 撮合（每次成交） | O(1) | 从最优价格级别的链表头取出 |
| 获取 BBO | O(1) | 缓存的 `Option<Price>` |
| 获取深度 N 档 | O(N) | BTreeMap 迭代器 |

注意：下单操作可能触发多次撮合（吃穿多个价格级别），此时总复杂度为 O(log P + K)，K = 成交笔数。

**对应代码**: `crates/tachyon-book/src/book.rs`

---

### Q2: 为什么用 Slab 分配器而不是标准堆分配？

**难度**: 中 | **关键词**: Slab, O(1), 内存碎片, 连续内存

**参考答案**:

```
标准堆分配:
  malloc(sizeof(Order)) → 系统调用/内存碎片 → ~50-200ns

Slab 分配:
  slab.insert(order) → 从空闲列表取索引 → ~5-10ns
```

Slab 分配器的优势：
1. **O(1) 分配/释放**: 维护空闲索引链表，insert 取头部，remove 归还头部
2. **内存连续**: 所有 Order 存储在一个 `Vec<Entry<Order>>` 中，cache 友好
3. **无碎片**: 定长槽位，不存在外部碎片
4. **索引寻址**: 返回 `usize` 索引而非指针，配合侵入式链表的 u32 索引使用

代价是需要预设最大容量（或动态扩容时 memcpy），以及不能存储大小不同的对象。

**对应代码**: `crates/tachyon-book/src/book.rs` (使用 `slab` crate)

---

### Q3: 环形缓冲区为什么要求容量是 2 的幂？

**难度**: 初 | **关键词**: 位运算, 取模优化

**参考答案**:

```rust
// 如果 capacity 是 2 的幂:
let index = sequence & (capacity - 1);  // 1 条指令，~1ns

// 如果 capacity 是任意值:
let index = sequence % capacity;        // 除法指令，~20-40ns
```

当 capacity = 2^N 时，`x & (capacity - 1)` 等价于 `x % capacity`，但用的是 AND 指令而非 DIV 指令。在 x86 上，整数除法需要 20-40 个时钟周期，而 AND 只需要 1 个周期。

对于每秒处理 500 万+ 消息的系统，这个优化节省的时间相当可观。

**对应代码**: `crates/tachyon-io/src/spsc.rs:28` (`assert!(capacity.is_power_of_two())`)

---

### Q4: 如何在定长磁盘记录上实现二分搜索？

**难度**: 中 | **关键词**: O(log N), seek, 定长记录, lower_bound

**参考答案**:

如果每条记录固定 56 字节，且按时间戳排序：

```rust
fn lower_bound(file: &File, target_ts: u64, record_count: u64) -> u64 {
    let mut lo = 0u64;
    let mut hi = record_count;
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        // 直接 seek 到第 mid 条记录的时间戳字段
        file.seek(SeekFrom::Start(mid * 56))?;
        let mut buf = [0u8; 8];
        file.read_exact(&mut buf)?;
        let ts = u64::from_le_bytes(buf);
        if ts < target_ts { lo = mid + 1; } else { hi = mid; }
    }
    lo
}
```

关键点：
- 定长记录使得 `offset = index * RECORD_SIZE`，O(1) 随机访问
- 只读取需要比较的 8 字节（时间戳），不读整条记录
- 100 万条记录只需 ~20 次 seek（log2(1000000) ≈ 20）
- 不需要额外的索引文件

**对应代码**: `crates/tachyon-persist/src/trade_store.rs:178-199`

---

### Q5: 侵入式链表 vs 非侵入式链表

**难度**: 中 | **关键词**: 侵入式, cache 命中, 零额外分配

**参考答案**:

**非侵入式** (`std::collections::LinkedList<T>`)：
```
Node { value: T, prev: *Node, next: *Node }
每个节点独立堆分配 → cache 不友好, 每次 malloc/free
```

**侵入式** (Tachyon 的方式)：
```rust
struct Order {
    // ... 业务字段 ...
    prev: u32,  // 在 Slab<Order> 中的索引
    next: u32,  // NO_LINK = u32::MAX 表示无连接
}
```

所有 Order 存储在 Slab 的连续内存中，prev/next 是 Slab 索引而非指针。

| 特性 | 非侵入式 | 侵入式 |
|------|---------|--------|
| 内存分配 | 每个节点独立 malloc | Slab 批量分配 |
| Cache 友好 | 差（节点散落在堆上） | 好（Slab 内存连续） |
| 额外开销 | 每节点 2 个指针 (16B) | 每节点 2 个 u32 (8B) |
| 删除（已知位置） | O(1) | O(1) |
| 可以同时属于多个链表 | 不行 | 可以（加更多 prev/next 字段） |

**对应代码**: `crates/tachyon-core/src/order.rs` (Order 结构体中的 prev/next 字段)

---

### Q6: CRC32 vs SHA256 用于数据完整性校验

**难度**: 初 | **关键词**: CRC32, 性能, 安全性

**参考答案**:

| | CRC32 | SHA256 |
|---|-------|--------|
| 速度 | ~3 GB/s (SSE 4.2 硬件加速) | ~500 MB/s |
| 检测能力 | 单/双 bit 翻转, 突发错误 | 任意修改 |
| 抗碰撞 | 弱（可构造碰撞） | 强（密码学安全） |
| 输出大小 | 4 字节 | 32 字节 |
| 适用场景 | 传输/存储完整性 | 安全校验/签名 |

WAL 用 CRC32 的原因：我们只需要检测磁盘损坏和部分写入（非恶意攻击），CRC32 足够且速度快几个数量级。在每秒百万次写入的场景下，SHA256 的开销不可忽略。

**对应代码**: `crates/tachyon-persist/src/wal.rs:136-140` (crc32fast)

---

## 9.3 并发与内存模型

### Q1: 解释 Rust/C++ 的内存序级别

**难度**: 高 | **关键词**: Relaxed, Acquire, Release, SeqCst

**参考答案**:

```
Relaxed  ──▶  只保证原子性，不保证顺序
Acquire  ──▶  后续读写不能重排到此操作之前（"获取"屏障）
Release  ──▶  之前读写不能重排到此操作之后（"释放"屏障）
AcqRel   ──▶  同时具有 Acquire 和 Release 语义
SeqCst   ──▶  全局唯一顺序，最强保证，也最慢
```

**Tachyon 中的实际例子**:

```rust
// SPSC 队列: 生产者用 Release, 消费者用 Acquire
// 保证: 消费者读到 tail 更新时，一定能看到 buffer 中的数据
producer: self.tail.store(new_tail, Ordering::Release);
consumer: let tail = self.tail.load(Ordering::Acquire);

// WAL 序列号: 只需要唯一递增，用 Relaxed
let seq = wal_sequence.fetch_add(1, Ordering::Relaxed) + 1;

// shutdown 标志: Release/Acquire 保证引擎看到 shutdown 时也看到之前的状态变更
main:   shutdown.store(true, Ordering::Release);
engine: if shutdown.load(Ordering::Acquire) { break; }
```

**面试追问**: "为什么 SPSC 不需要 SeqCst？"
答: SPSC 只有两个线程，Release/Acquire 对已经建立了完整的 happens-before 关系。SeqCst 的全局顺序保证对 SPSC 没有额外价值，但会引入不必要的内存屏障开销。

**对应代码**: `crates/tachyon-io/src/spsc.rs`

---

### Q2: 什么是 false sharing？如何避免？

**难度**: 中 | **关键词**: cache line, 64 字节, padding

**参考答案**:

**问题**: 两个原子变量如果在同一个 cache line（通常 64 字节）上，即使被不同 CPU 核心修改的是不同变量，缓存一致性协议（MESI）也会导致整条 cache line 在核心间来回传输（"乒乓"）。

```
核心 0 写 head ─┐    ┌─ 核心 1 写 tail
                ▼    ▼
  ┌─────────────────────────────┐
  │ [head] [tail] [其他数据...] │  ← 同一条 cache line (64B)
  └─────────────────────────────┘
  每次写操作导致对方核心的 cache 失效 → 延迟从 ~1ns 升到 ~50-100ns
```

**解决**: padding 到独立的 cache line:

```rust
#[repr(align(128))]  // 128 字节对齐（覆盖 Intel/AMD 的预取行为）
struct CachePadded<T> {
    value: T,
}

struct SpscQueue<T> {
    head: CachePadded<AtomicU64>,  // 独占一条 cache line
    tail: CachePadded<AtomicU64>,  // 独占另一条 cache line
}
```

**对应代码**: `crates/tachyon-io/src/spsc.rs` (CachePadded)

---

### Q3: Lock-free vs Wait-free 的区别

**难度**: 中 | **关键词**: 进展保证, CAS 循环, SPSC

**参考答案**:

| | Lock-free | Wait-free |
|---|-----------|-----------|
| 定义 | 至少一个线程在有限步内完成 | 每个线程都在有限步内完成 |
| 实现 | CAS 循环 | 无循环/有界循环 |
| 饥饿 | 可能（高竞争下某线程一直失败） | 不可能 |
| 复杂度 | 中等 | 高 |
| 示例 | MPSC 队列 | SPSC 队列 |

**Tachyon 的 SPSC 是 wait-free 的**：
- `try_push`: 一次 `tail.load` + 比较 + 写入 + `tail.store` — 固定步数
- `try_pop`: 一次 `head.load` + 比较 + 读取 + `head.store` — 固定步数
- 没有 CAS 重试循环

**Tachyon 的 MPSC 是 lock-free 的**：
- 多个生产者竞争 `tail.compare_exchange`，失败的线程重试
- 至少一个生产者在每轮 CAS 中成功，满足 lock-free 定义

**对应代码**: `crates/tachyon-io/src/spsc.rs` (wait-free), `crates/tachyon-io/src/mpsc.rs` (lock-free)

---

### Q4: ABA 问题是什么？Tachyon 如何避免？

**难度**: 高 | **关键词**: CAS, 序列号, u64 计数器

**参考答案**:

**ABA 问题**: CAS 操作比较的是值，不是身份。如果值从 A 变为 B 再变回 A，CAS 会认为"没变"。

```
Thread 1: 读取 head = A，准备 CAS(A → new)
Thread 2: pop A, push B, pop B, push A ← head 又变回 A
Thread 1: CAS(A → new) 成功 ← 但 A 已经不是原来那个 A 了！
```

**Tachyon 的 SPSC 天然不受 ABA 影响**：head 和 tail 是单调递增的 u64 序列号，不是指针/索引值。它们从不"回退"：

```rust
// head 只增不减
let old_head = self.head.load(Acquire);
// ... 读取 buffer[old_head & mask] ...
self.head.store(old_head + 1, Release);
```

即使处理了 2^64 条消息（理论上需要 585 年@10亿消息/秒），序列号也不会回绕。

**对于使用指针的无锁数据结构**（不是 Tachyon 的情况），常见解决方案：
- 加版本号（tagged pointer）
- Hazard pointers
- Epoch-based reclamation

**对应代码**: `crates/tachyon-io/src/spsc.rs`

---

### Q5: RwLock 的 poison 是什么？为什么 Tachyon 选择恢复？

**难度**: 中 | **关键词**: poison, panic, into_inner, 可用性

**参考答案**:

当持有 `std::sync::RwLock` 的线程 panic 时，锁进入 "poisoned" 状态。后续的 `lock()` / `write()` 调用返回 `Err(PoisonError)`。

```rust
// 传播 panic（默认行为）
let guard = lock.write().unwrap();  // 如果 poisoned，这里也 panic

// 恢复（Tachyon 的选择）
let guard = lock.write().unwrap_or_else(|p| p.into_inner());
```

**Tachyon 选择恢复的原因**:
- 共享状态（recent_trades, book_snapshots）是"尽力而为"的缓存，不是关键状态
- 即使数据暂时不一致，也比整个系统崩溃好
- 引擎线程的关键状态（订单簿）不通过 RwLock 共享
- `into_inner()` 获取被保护的数据，继续服务

**对应代码**: `bin/tachyon-server/src/main.rs:689-691`

---

### Q6: 为什么 WAL 序列号用 `Ordering::Relaxed`？

**难度**: 高 | **关键词**: Relaxed, 单调递增, happens-before

**参考答案**:

```rust
let seq = ctx.wal_sequence.fetch_add(1, Ordering::Relaxed) + 1;
```

`fetch_add` 的原子性保证了序列号唯一递增，即使多个引擎线程同时调用。`Relaxed` 不提供跨线程的顺序保证，但这里不需要：

1. 每个引擎线程独立写自己的 WAL 条目
2. 序列号只需要全局唯一，不需要反映跨线程的因果关系
3. WAL 的顺序由每个引擎线程内的单线程执行保证
4. 恢复时按序列号排序，不依赖写入顺序

如果序列号需要反映全局因果顺序（比如分布式系统），则需要更强的 ordering 或 Lamport 时钟。但在单机多线程场景下，`Relaxed` 的性能优势（~1ns vs ~10ns）是值得的。

**对应代码**: `bin/tachyon-server/src/main.rs:645`

---

## 9.4 操作系统与硬件

### Q1: CPU 缓存一致性协议 (MESI) 如何工作？

**难度**: 高 | **关键词**: MESI, Modified, Exclusive, Shared, Invalid

**参考答案**:

每条 cache line 在每个核心中处于四种状态之一：

| 状态 | 含义 | 本地读 | 本地写 | 其他核心读 | 其他核心写 |
|------|------|--------|--------|-----------|-----------|
| **M** (Modified) | 本核心独占修改 | 命中 | 命中 | → S (写回) | → I (写回) |
| **E** (Exclusive) | 本核心独占未修改 | 命中 | → M | → S | → I |
| **S** (Shared) | 多核心共享 | 命中 | → M (invalidate) | 命中 | → I |
| **I** (Invalid) | 无效 | 缓存未命中 | 缓存未命中 | - | - |

**与 Tachyon 的关系**:
- 引擎线程绑核后，订单簿数据长期处于 M/E 状态（独占），无需跨核心同步
- SPSC 的 head/tail 在不同核心上，padding 后各自独占一条 cache line，减少 S→I 的 invalidation
- 如果不 padding，head 和 tail 在同一 cache line，每次写操作都触发 invalidation（~50ns）

---

### Q2: NUMA 架构对撮合引擎有什么影响？

**难度**: 高 | **关键词**: NUMA, 本地内存, 远程内存, 绑核

**参考答案**:

```
NUMA Node 0                 NUMA Node 1
┌──────────────┐           ┌──────────────┐
│ CPU 0-7      │           │ CPU 8-15     │
│ Local Memory │◀──QPI──▶│ Local Memory │
└──────────────┘           └──────────────┘
  本地访问: ~70ns            远程访问: ~120-300ns
```

如果引擎线程分配内存在 Node 0，但被调度到 Node 1 的 CPU 上执行，所有内存访问都变成远程访问，延迟翻倍。

**Tachyon 的应对**:
1. CPU 亲和性绑核（`core_affinity::set_for_current`），线程不会被调度到其他核心
2. 线程启动后分配的内存自动在本地 NUMA 节点上（first-touch policy）
3. 未来优化：可以通过 `libnuma` 显式指定内存分配节点

---

### Q3: 上下文切换的代价是多少？

**难度**: 中 | **关键词**: 上下文切换, TLB flush, cache pollution

**参考答案**:

一次上下文切换的直接成本：~1-10us

但间接成本更大：
- **TLB 刷新**: 切换进程后 TLB 缓存失效，后续内存访问需要重新 page walk (~100-1000ns per miss)
- **Cache 污染**: 新线程的数据填充 L1/L2，原线程的 "热" 数据被淘汰
- **分支预测器重置**: CPU 的分支预测历史对新线程无效

**Tachyon 的策略**:
- 专用 OS 线程 + CPU 绑核 → 不被抢占，无上下文切换
- spin-loop 而非 sleep → 空闲时不让出 CPU
- 如果确实空闲过长，使用 `yield_now()` 而非 `sleep`

---

### Q4: Huge Pages 和 TLB

**难度**: 中 | **关键词**: 4KB, 2MB, 1GB, TLB miss

**参考答案**:

| 页大小 | 覆盖 1GB 内存需要的页数 | TLB 条目需求 |
|--------|----------------------|-------------|
| 4KB | 262,144 页 | 262,144 条 (远超 TLB 容量) |
| 2MB | 512 页 | 512 条 (可能放入 TLB) |
| 1GB | 1 页 | 1 条 |

典型 CPU 的 TLB 容量：~1536 条（L1 dTLB）+ ~1536 条（L2 TLB）。

订单簿如果包含 100 万订单 × 128 字节/订单 ≈ 128MB，用 4KB 页需要 32,768 个 TLB 条目——远超 TLB 容量，导致频繁 TLB miss（每次 ~10-100ns 的 page walk）。

**Tachyon 未来优化方向**:
- 使用 `madvise(MADV_HUGEPAGE)` 或 `mmap` 的 `MAP_HUGETLB` 标志
- 对 Slab 的底层 Vec 使用 huge page 分配器
- 在 Linux 上配置 transparent huge pages (THP)

---

### Q5: 为什么 Nagle 算法要禁用？

**难度**: 初 | **关键词**: TCP_NODELAY, Nagle, 延迟

**参考答案**:

Nagle 算法：当有未确认的小包时，缓冲后续小包，等到 ACK 返回或缓冲区满才发送。目的是减少网络中的小包数量。

```
不禁用 Nagle:
  send(50B) → 缓冲，等待 ACK...  → ~40ms 后才发送
                                     (典型 WAN RTT)
禁用 Nagle (TCP_NODELAY):
  send(50B) → 立即发送 → ~0.1ms 内到达对端
```

交易系统中每条消息都很小（几十到几百字节），但必须立即发送。40ms 的延迟在高频交易中是不可接受的。

```rust
// crates/tachyon-gateway/src/tcp.rs:84
let _ = stream.set_nodelay(true);
```

**对应代码**: `crates/tachyon-gateway/src/tcp.rs:84`

---

## 9.5 Rust 特定

### Q1: `unsafe` 的安全契约是什么？

**难度**: 高 | **关键词**: unsafe, safety contract, UB

**参考答案**:

`unsafe` 不意味着"不安全"，而是告诉编译器"我在此承担安全责任"。使用 `unsafe` 时必须确保满足"安全契约"：

```rust
// 示例：SPSC 队列中的 unsafe 读取
unsafe {
    // 安全契约:
    // 1. self.head < self.tail（有数据可读）—— 由 Acquire load 保证
    // 2. buffer[index] 已被生产者写入 —— 由 Release/Acquire 对保证
    // 3. 没有其他线程同时读取同一位置 —— 由单消费者保证
    ptr::read(self.buffer[index].get())
}
```

**Tachyon 中的 unsafe 使用**:
- `SpscQueue`: `UnsafeCell` + `MaybeUninit` + `ptr::read/write` — 避免不必要的 Clone
- `#[repr(C)]` 结构体: 确保内存布局确定性（如 TradeRecord 的 56 字节）
- `CachePadded`: `#[repr(align(128))]` 控制对齐

**面试关键**: 每次使用 `unsafe` 都应该写注释说明为什么是安全的。

**对应代码**: `crates/tachyon-io/src/spsc.rs`

---

### Q2: `Send` 和 `Sync` trait 的含义

**难度**: 中 | **关键词**: Send, Sync, 线程安全

**参考答案**:

```
Send:  T 可以安全地移动到另一个线程
       例: Vec<u8>, String, Arc<T>
       反例: Rc<T>（引用计数非原子）

Sync:  &T 可以安全地在多个线程间共享
       例: Mutex<T>, AtomicU64, i32
       反例: Cell<T>, RefCell<T>（内部可变性非线程安全）

关系:  T: Sync ⟺ &T: Send
```

**Tachyon 的体现**:
- `SpscQueue<T>` 实现了 `Send + Sync`（通过 unsafe impl），因为它的设计保证了正确的跨线程访问
- `EngineCommand` 包含 `oneshot::Sender<T>`，它是 `Send` 的——tokio 的 oneshot 通道天然支持跨线程
- `Arc<SpscQueue<EngineCommand>>` 被多个线程共享——需要 `SpscQueue: Sync`

---

### Q3: `UnsafeCell` 的作用

**难度**: 高 | **关键词**: interior mutability, aliasing, UnsafeCell

**参考答案**:

`UnsafeCell<T>` 是 Rust 中获取内部可变性的唯一原语。所有其他内部可变性类型（`Cell`, `RefCell`, `Mutex`, `RwLock`, `AtomicXxx`）都建立在 `UnsafeCell` 之上。

```rust
// SPSC 队列的 buffer
buffer: Box<[MaybeUninit<UnsafeCell<T>>]>
```

**为什么需要 `UnsafeCell`？**

Rust 的别名规则（aliasing rules）规定：如果存在 `&T`（不可变引用），则不能存在 `&mut T`。`UnsafeCell` 是编译器的"逃生口"——它告诉编译器"这个值可能通过共享引用被修改"，阻止编译器做出错误的优化假设。

在 SPSC 队列中：生产者通过 `&self` 写入 buffer（因为是单生产者，只需共享引用），必须用 `UnsafeCell` 来合法地获取可变访问。

---

### Q4: `#[repr(C)]` vs 默认布局

**难度**: 中 | **关键词**: repr(C), padding, 内存布局

**参考答案**:

```rust
// 默认布局: 编译器可以重排字段、插入 padding
struct DefaultLayout {
    a: u8,   // 1 byte
    b: u64,  // 8 bytes — 编译器可能把这个放在第一位
    c: u32,  // 4 bytes
}
// sizeof 可能是 16（重排后）或 24（不重排，有 padding）

// C 布局: 字段顺序固定，padding 可预测
#[repr(C)]
struct CLayout {
    a: u8,       // offset 0, 1 byte
    _pad: [u8;7],// offset 1, 7 bytes padding
    b: u64,      // offset 8, 8 bytes
    c: u32,      // offset 16, 4 bytes
    _pad2: [u8;4],// offset 20, 4 bytes padding
}               // sizeof = 24
```

**Tachyon 中 `#[repr(C)]` 的使用**:

```rust
#[repr(C)]
pub struct TradeRecord {  // 精确 56 字节
    pub timestamp: u64,
    pub trade_id: u64,
    pub price: i64,
    ...
}
const _: () = assert!(size_of::<TradeRecord>() == 56);
```

使用 `#[repr(C)]` 的原因：
1. **磁盘格式确定**: 不同编译器版本/优化级别不会改变布局
2. **可以安全 memcpy**: `to_bytes()` / `from_bytes()` 对应精确的字段偏移
3. **FFI 兼容**: 如果需要与 C/C++ 交互

**对应代码**: `crates/tachyon-persist/src/trade_store.rs:22-42`

---

### Q5: Rust 的 `serde` Serialize/Deserialize 在热路径上的性能问题

**难度**: 中 | **关键词**: serde, bincode, 零拷贝, 热路径

**参考答案**:

`serde` + `bincode` 的组合在大多数场景下性能足够，但在纳秒级热路径上可能成为瓶颈：
- 序列化/反序列化涉及内存分配（`Vec<u8>`）
- 枚举类型（如 `Command`）需要写入 tag 字节并分支
- 嵌套结构需要递归遍历

**Tachyon 的策略**:
- **热路径（撮合引擎内部）**: 不使用序列化，直接操作结构体
- **温路径（WAL 写入）**: 使用 `bincode`（紧凑二进制，比 JSON 快 100x）
- **冷路径（REST API）**: 使用 `serde_json`（可读性好，性能够用）
- **磁盘存储（TradeStore）**: 手写 `to_bytes()`/`from_bytes()`，零开销

---

## 9.6 项目讲解技巧

### 30 秒电梯演讲

> "我用 Rust 从零构建了一个高性能交易撮合引擎。核心撮合延迟低于 100 纳秒，单品种吞吐超过 500 万笔/秒。架构参考了 Hyperliquid——每个交易品种一个专用线程绑核，通过无锁 SPSC 队列与网关通信。支持 WAL + 快照的确定性崩溃恢复。目前有 223 个测试全部通过，包括订单簿、撮合引擎、无锁队列、网关层和持久化五个核心模块。"

### 面试中应该主动提及的数字

| 指标 | 数值 | 意义 |
|------|------|------|
| 撮合延迟（中位数） | <100ns | 纯算法延迟 |
| 单品种吞吐 | >5M ops/sec | 单线程能力 |
| 测试覆盖 | 223 tests | 工程质量 |
| 代码量 | ~8K LOC Rust | 可独立完成的规模 |
| 内存效率 | <1GB / 1M orders | 资源友好 |

### 设计权衡的讨论方向

面试官最喜欢的追问是 "为什么选 X 而不选 Y？"。准备好以下权衡：

1. **BTreeMap vs HashMap 用于价格索引**
   - BTreeMap: O(log P) 插入，但天然有序，支持 range scan 取深度
   - HashMap: O(1) 插入，但无序，需要额外排序
   - 选 BTreeMap 因为取深度是常用操作，且价格级别通常 <1000

2. **专用线程 vs tokio task 用于引擎**
   - 专用线程: 可预测延迟，但浪费空闲 CPU
   - tokio task: 资源效率高，但调度延迟不可预测
   - 选专用线程因为延迟是撮合引擎的第一优先级

3. **命令重放 vs 事件重放用于 WAL**
   - 命令重放: 正向执行，天然正确
   - 事件重放: 需要逆向推导订单簿状态
   - 选命令重放因为实现简单且保证确定性

4. **std::sync::RwLock vs tokio::sync::RwLock 用于共享状态**
   - std::sync: 兼容引擎线程（非 tokio），短持有时间下性能更好
   - tokio::sync: 不阻塞 tokio worker，但不能在同步代码中使用
   - 选 std::sync 因为引擎线程不在 tokio 运行时上

5. **CRC32 vs SHA256 用于 WAL 校验**
   - CRC32: 硬件加速, ~3GB/s
   - SHA256: 密码学安全, ~500MB/s
   - 选 CRC32 因为只需检测硬件错误，不防恶意攻击

### "如果让你改进，你会怎么做？" 的回答

1. **零分配审计**: 使用 counting allocator 找出热路径上的所有堆分配，替换为栈分配或 arena 分配
2. **SIMD 批量价格比较**: 使用 AVX2 一次比较 8 个价格级别，加速订单匹配
3. **PGO (Profile-Guided Optimization)**: 用真实负载的 profile 数据指导编译器优化分支预测和内联决策
4. **io_uring**: 替换标准的 `write()` + `fsync()`，减少 WAL 写入的系统调用开销
5. **Hardware timestamping**: 使用 NIC 硬件时间戳替代 `SystemTime::now()`，消除时钟漂移
6. **FIX 协议支持**: 添加标准金融信息交换协议，与真实交易基础设施对接

---

## 本章小结

面试准备的核心策略：

1. **画图先行**: 系统设计题一定先画架构图，再逐层展开
2. **数字说话**: 记住关键性能指标，用数字展示工程能力
3. **权衡讨论**: 不说"最好"，说"在这个场景下 X 比 Y 更适合，因为..."
4. **代码定位**: 每个回答都能指向具体的源文件和行号
5. **深度优先**: 面试官追问某个方向时，展示你理解到底层（硬件/OS）的能力

> **返回目录**: [Tachyon 教程首页](./README.md)
