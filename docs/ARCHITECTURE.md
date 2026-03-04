# Tachyon - 技术架构设计文档

> Technical Architecture Specification v2.0
>
> 本文档为 Tachyon 撮合引擎的详细技术架构规范，面向有经验的 Rust 工程师，
> 提供足够的细节用于直接实现。

---

## 目录

1. [总体架构](#1-总体架构)
2. [线程模型](#2-线程模型)
3. [数据结构设计](#3-数据结构设计)
4. [定点数算术](#4-定点数算术)
5. [Lock-Free 通信](#5-lock-free-通信)
6. [内存架构](#6-内存架构)
7. [撮合算法](#7-撮合算法)
8. [事件系统](#8-事件系统)
9. [持久化层](#9-持久化层)
10. [协议栈](#10-协议栈)
11. [共识适配层](#11-共识适配层)
12. [风控管理](#12-风控管理)
13. [编译与性能优化](#13-编译与性能优化)
14. [性能目标](#14-性能目标)
15. [Crate 组织](#15-crate-组织)

---

## 1. 总体架构

Tachyon 采用 **LMAX Disruptor 启发的流水线架构**。所有组件通过单向 SPSC (Single-Producer
Single-Consumer) Ring Buffer 连接，形成一条无锁的事件处理流水线。

### 1.1 数据流总览

```
                        ┌──────────────────────────────────────────────────────────────────┐
                        │                        Tachyon Engine                             │
                        │                                                                  │
  FIX 4.4 ─────┐       │  ┌───────────┐  SPSC   ┌────────────┐  SPSC   ┌──────────────┐  │
  WebSocket ────┤       │  │           │  Ring    │            │  Ring   │  Matcher      │  │
  gRPC ─────────┼──────►│  │  Gateway  │────────►│  Sequencer │───┬────►│  (BTC/USDT)  │──┼──►  Market Data
                │       │  │  (tokio)  │         │  /Journal  │   │     │  Core 1 pin  │  │     Disseminator
  REST ─────────┘       │  │           │         │  Core 0    │   │     └──────────────┘  │
                        │  └───────────┘         │            │   │     ┌──────────────┐  │
                        │   async I/O            └────────────┘   ├────►│  Matcher      │──┼──►  Event
                        │   多线程 tokio                           │     │  (ETH/USDT)  │  │     Stream
                        │                                         │     │  Core 2 pin  │  │
                        │                                         │     └──────────────┘  │
                        │                                         │     ┌──────────────┐  │
                        │                                         └────►│  Matcher      │──┼──►  WAL
                        │                                               │  (SOL/USDT)  │  │     Writer
                        │                                               │  Core 3 pin  │  │
                        │                                               └──────────────┘  │
                        └──────────────────────────────────────────────────────────────────┘
```

### 1.2 核心设计原则

| 原则 | 说明 |
|------|------|
| **Mechanical Sympathy** | 代码设计贴合硬件特性：cache line 对齐、顺序内存访问、避免分支预测失败 |
| **Zero-Allocation Hot Path** | 撮合关键路径上零堆分配，所有对象预分配或栈上分配 |
| **Single Writer Principle** | 每个可变状态只有一个写者线程，消除所有锁竞争 |
| **Event Sourcing** | 所有状态变更通过事件表达，支持确定性回放 |
| **Shared Nothing** | 交易对之间无共享状态，天然可水平扩展 |

### 1.3 组件职责

| 组件 | 职责 | 热路径 |
|------|------|--------|
| **Gateway** | 协议解析 (FIX/WS/gRPC)，格式验证，会话管理 | 否 (async I/O) |
| **Sequencer** | 分配全局单调递增序列号，路由到对应 symbol matcher | 是 |
| **Journal** | 将 sequenced command 追加写入 WAL | 是 (可与 Sequencer 共享核心) |
| **Matcher** | 风控前检、订单簿操作、撮合逻辑、事件产出 | 是 (最热) |
| **Market Data Disseminator** | 聚合撮合事件，生成行情快照，推送给订阅者 | 否 |

---

## 2. 线程模型

### 2.1 Thread-per-Symbol 架构

```
┌─────────────────────────────────────────────────────────────────────────┐
│                          CPU Core Assignment                            │
├─────────────────────────────────────────────────────────────────────────┤
│                                                                         │
│  Core 0 (isolated):  Sequencer + Journal Writer                        │
│     ┌─────────────────────────────────────┐                            │
│     │  loop {                             │                            │
│     │    cmd = gateway_ring.try_recv();   │  ◄── busy-spin poll       │
│     │    seq = next_sequence();           │                            │
│     │    journal.append(seq, cmd);        │  ◄── mmap append          │
│     │    matcher_ring[cmd.symbol].send(); │  ◄── SPSC push            │
│     │  }                                 │                            │
│     └─────────────────────────────────────┘                            │
│                                                                         │
│  Core 1 (isolated):  BTC/USDT Matcher                                  │
│     ┌─────────────────────────────────────┐                            │
│     │  loop {                             │                            │
│     │    cmd = ring.try_recv();           │  ◄── busy-spin poll       │
│     │    events = engine.process(cmd);    │  ◄── match + risk         │
│     │    output_ring.send_batch(events);  │  ◄── SPSC push            │
│     │  }                                 │                            │
│     └─────────────────────────────────────┘                            │
│                                                                         │
│  Core 2 (isolated):  ETH/USDT Matcher                                  │
│  Core 3 (isolated):  SOL/USDT Matcher                                  │
│  ...                                                                    │
│                                                                         │
│  Core N   (shared):  Market Data Disseminator                          │
│  Core N+1 (shared):  tokio runtime (Gateway async I/O)                 │
│  Core N+2 (shared):  tokio runtime (Gateway async I/O)                 │
│                                                                         │
└─────────────────────────────────────────────────────────────────────────┘
```

### 2.2 线程类型详解

**Sequencer 线程 (Core 0)**：
- 单线程，busy-spin 轮询 gateway ring buffer
- 分配 monotonic sequence number (`u64`，永不回绕)
- 按 `symbol_id` 路由到对应 matcher 的 SPSC ring buffer
- 可选地与 journal writer 合并在同一线程（减少一次 ring buffer 跳转）
- 不做任何业务逻辑，纯粹的 sequencing + routing

**Matcher 线程 (Core 1..N, pinned)**：
- 每个 symbol 独占一个 CPU 核，通过 `core_affinity` crate 绑核
- busy-spin 轮询输入 ring buffer（不使用 `thread::park` 或条件变量）
- 内联执行风控前检 + 撮合逻辑 + 事件产出
- 所有状态 (OrderBook, Slab pool 等) 完全私有，无共享
- 输出事件通过 SPSC ring buffer 推送给 disseminator

**Gateway 线程 (tokio async)**：
- 运行在独立的 tokio runtime 上，与热路径物理隔离
- 处理 FIX 4.4、WebSocket、gRPC 协议解析
- 验证消息格式后，将 `Command` 推入 gateway -> sequencer 的 SPSC ring buffer
- **不在热路径上**：async I/O 的延迟抖动不影响撮合延迟

**Market Data Disseminator 线程**：
- 从各 matcher 的 output ring buffer 消费事件
- 聚合生成 L2/L3 行情快照
- 通过 ITCH-like binary feed 推送给订阅者
- 可运行在共享核心上（非延迟敏感）

### 2.3 CPU 绑核与隔离

```rust
use core_affinity::CoreId;

/// 将当前线程绑定到指定 CPU 核
fn pin_to_core(core_id: usize) {
    let core = CoreId { id: core_id };
    core_affinity::set_for_current(core);
}

/// 部署建议：在 Linux 上使用 isolcpus 隔离撮合核心
/// /etc/default/grub: GRUB_CMDLINE_LINUX="isolcpus=0-3 nohz_full=0-3 rcu_nocbs=0-3"
///
/// macOS (Apple Silicon) 上无法 isolcpus，但可以绑核 + 提升线程优先级
/// 使用 thread_priority crate 或 pthread_setschedparam
```

**部署配置建议**：

| 设置 | Linux | macOS |
|------|-------|-------|
| CPU 隔离 | `isolcpus=0-3` | 不支持，用绑核代替 |
| 无 tick 模式 | `nohz_full=0-3` | 不支持 |
| 关闭超线程 | BIOS 中禁用 HT | Apple Silicon 无 HT |
| CPU 调频 | `performance` governor | 自动管理 |
| 大页内存 | `hugepages=1024` (2MB pages) | 不支持 |

---

## 3. 数据结构设计

基于 WK Selph 的经典撮合引擎数据模型，针对 Rust 的所有权系统优化。

### 3.1 订单簿总览

```
                        OrderBook (单个 symbol)
┌──────────────────────────────────────────────────────────────────┐
│                                                                  │
│  bids: BTreeMap<Price, PriceLevelIdx>    (降序，最高价优先)       │
│  asks: BTreeMap<Price, PriceLevelIdx>    (升序，最低价优先)       │
│                                                                  │
│  levels: Slab<PriceLevel>                (价格级别存储池)         │
│  orders: Slab<Order>                     (订单存储池)            │
│  order_map: HashMap<OrderId, OrderHandle> (O(1) 订单查找)        │
│                                                                  │
│  best_bid: Option<Price>                 (BBO cache)             │
│  best_ask: Option<Price>                 (BBO cache)             │
│                                                                  │
│  symbol_id: u32                                                  │
│  tick_size: Price                        (最小价格变动)           │
│  lot_size: Quantity                      (最小数量变动)           │
│                                                                  │
└──────────────────────────────────────────────────────────────────┘

        Asks (卖盘, 升序)                    Bids (买盘, 降序)
        ┌─────────────────┐                 ┌─────────────────┐
        │ 50002.00: ──────┼──► [O5]──[O6]  │ 50000.50: ──────┼──► [O7]──[O8]
        │ 50001.50: ──────┼──► [O4]        │ 50000.00: ──────┼──► [O9]
best_ask│ 50001.00: ──────┼──► [O1]──[O2]  │ 49999.50: ──────┼──► [O10]──[O11]
        └─────────────────┘    ──[O3]       └─────────────────┘   best_bid
              ▲                                    ▲
              │                                    │
         BTreeMap<Price,                      BTreeMap<Price,
         PriceLevelIdx>                       PriceLevelIdx>
```

### 3.2 核心 Struct 定义

```rust
use slab::Slab;
use std::collections::{BTreeMap, HashMap};

/// 价格级别在 Slab 中的索引
type PriceLevelIdx = usize;

/// 订单在 Slab 中的索引
type OrderIdx = usize;

/// 订单 ID（外部分配，全局唯一）
type OrderId = u64;

/// OrderHandle 用于从 OrderId 快速定位订单的完整位置
#[derive(Clone, Copy)]
struct OrderHandle {
    /// 订单在 orders Slab 中的索引
    order_idx: OrderIdx,
    /// 所在价格级别在 levels Slab 中的索引
    level_idx: PriceLevelIdx,
    /// 冗余存储价格，用于在 BTreeMap 中定位（避免额外 Slab 查找）
    price: Price,
    /// 冗余存储方向，用于确定查找 bids 还是 asks
    side: Side,
}

/// 订单簿主结构
struct OrderBook {
    symbol_id: u32,

    /// 买盘价格级别索引（降序遍历）
    bids: BTreeMap<Price, PriceLevelIdx>,
    /// 卖盘价格级别索引（升序遍历）
    asks: BTreeMap<Price, PriceLevelIdx>,

    /// 价格级别存储池（Slab 提供 O(1) insert/remove + 索引稳定）
    levels: Slab<PriceLevel>,
    /// 订单存储池
    orders: Slab<Order>,
    /// OrderId -> OrderHandle 映射，用于 O(1) cancel/modify
    order_map: HashMap<OrderId, OrderHandle>,

    /// BBO (Best Bid and Offer) 缓存
    best_bid: Option<Price>,
    best_ask: Option<Price>,

    /// 市场参数
    tick_size: Price,
    lot_size: Quantity,

    /// 序列号（用于事件溯源）
    last_sequence: u64,
}
```

### 3.3 PriceLevel 与 Order (基于 Slab 的侵入式链表)

```rust
/// 价格级别：同一价格的所有订单组成双向链表
struct PriceLevel {
    price: Price,
    /// 该价格级别的总挂单量（缓存，避免遍历）
    total_quantity: Quantity,
    /// 挂单数量
    order_count: u32,
    /// 双向链表头（时间最早的订单，优先成交）
    head: Option<OrderIdx>,
    /// 双向链表尾（时间最晚的订单）
    tail: Option<OrderIdx>,
}

/// 订单：存储在 Slab<Order> 中，通过 prev/next 索引形成链表
struct Order {
    id: OrderId,
    price: Price,
    remaining_qty: Quantity,
    original_qty: Quantity,
    side: Side,
    order_type: OrderType,
    time_in_force: TimeInForce,
    account_id: u64,
    timestamp_ns: u64,
    /// 在同一 PriceLevel 中的前驱（更早的订单）
    prev: Option<OrderIdx>,
    /// 在同一 PriceLevel 中的后继（更晚的订单）
    next: Option<OrderIdx>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Side {
    Buy,
    Sell,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum OrderType {
    Limit,
    Market,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TimeInForce {
    GTC,  // Good-Til-Cancel
    IOC,  // Immediate-or-Cancel
    FOK,  // Fill-or-Kill
    PostOnly,
}
```

### 3.4 关键操作的数据流

**插入订单到价格级别 (尾插)**：

```rust
impl OrderBook {
    /// 将订单插入指定价格级别的尾部（FIFO: 新订单排最后）
    fn insert_order_to_level(&mut self, level_idx: PriceLevelIdx, order: Order) -> OrderIdx {
        let order_idx = self.orders.insert(order);

        let level = &mut self.levels[level_idx];
        level.total_quantity += self.orders[order_idx].remaining_qty;
        level.order_count += 1;

        // 链接到链表尾部
        if let Some(old_tail) = level.tail {
            self.orders[old_tail].next = Some(order_idx);
            self.orders[order_idx].prev = Some(old_tail);
        } else {
            // 链表为空，head 也指向新订单
            level.head = Some(order_idx);
        }
        level.tail = Some(order_idx);

        order_idx
    }

    /// 从价格级别中移除订单（O(1)，利用 prev/next 索引）
    fn remove_order_from_level(&mut self, level_idx: PriceLevelIdx, order_idx: OrderIdx) {
        let order = &self.orders[order_idx];
        let prev = order.prev;
        let next = order.next;
        let qty = order.remaining_qty;

        // 更新链表指针
        if let Some(p) = prev {
            self.orders[p].next = next;
        } else {
            self.levels[level_idx].head = next;
        }
        if let Some(n) = next {
            self.orders[n].prev = prev;
        } else {
            self.levels[level_idx].tail = prev;
        }

        // 更新级别统计
        self.levels[level_idx].total_quantity -= qty;
        self.levels[level_idx].order_count -= 1;

        // 从 Slab 中释放（O(1)，slot 回到 free list）
        self.orders.remove(order_idx);
    }
}
```

**BBO 缓存更新**：

```rust
impl OrderBook {
    /// 在每次 insert/cancel/match 后更新 BBO 缓存
    fn update_bbo(&mut self) {
        // BTreeMap::iter() 按升序遍历，first = 最低价
        // BTreeMap::iter().rev() 按降序遍历，first = 最高价
        self.best_ask = self.asks.keys().next().copied();
        self.best_bid = self.bids.keys().next_back().copied();
    }

    /// 获取买一 / 卖一 (O(1) from cache)
    #[inline(always)]
    fn best_bid(&self) -> Option<Price> {
        self.best_bid
    }

    #[inline(always)]
    fn best_ask(&self) -> Option<Price> {
        self.best_ask
    }
}
```

> **注意**: `BTreeMap::iter().next()` (最小键) 和 `BTreeMap::iter().next_back()` (最大键)
> 的时间复杂度均为 O(log n)，因为 B-Tree 需要从根节点遍历到叶节点。但由于我们在每次
> 操作后都缓存了 BBO，实际撮合循环中使用缓存值是 O(1) 的。BBO 缓存在 insert/remove
> price level 时增量更新：insert 时只需与当前 BBO 比较，remove 时仅在删除的恰好是
> BBO 级别时才需要查询 BTreeMap 获取新的最优价。

### 3.5 为什么选择 `slab` + 索引链表

| 方案 | 优点 | 缺点 |
|------|------|------|
| `VecDeque<Order>` | 简单 | 中间删除 O(n)，cancel 性能差 |
| `LinkedList<Order>` (std) | O(1) 删除 | 非侵入式，节点散布在堆上，cache miss |
| `unsafe` 裸指针链表 | O(1) 删除 | unsafe，难以维护 |
| **`Slab<Order>` + usize 索引** | **O(1) 删除，连续内存，safe Rust** | 索引可能 stale（需要小心管理） |

`Slab` 本质上是一个带 free list 的 `Vec`。插入返回一个 `usize` 索引，该索引在元素
被移除前保持有效。订单的 `prev`/`next` 字段存储的就是 Slab 索引，形成侵入式链表。
这种方式完全在 safe Rust 中实现，且内存布局紧凑（所有 Order 在同一个 Vec 内）。

---

## 4. 定点数算术

### 4.1 Price 和 Quantity 类型

浮点数在金融计算中是不可接受的（IEEE 754 精度丢失）。Tachyon 使用 **scaled integer**
定点数表示所有价格和数量。

```rust
/// 价格：有符号 64-bit 定点数
/// 负价格用于表示期权的负 premium 或做市商报价偏移
///
/// scale = 8 时：1.00000000 表示为 100_000_000i64
/// BTC/USDT 价格 50001.50 表示为 5_000_150_000_000i64
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
#[repr(transparent)]
pub struct Price(pub i64);

/// 数量：无符号 64-bit 定点数
/// 数量永远非负
///
/// scale = 8 时：1.50000000 BTC 表示为 150_000_000u64
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
#[repr(transparent)]
pub struct Quantity(pub u64);

/// 每个 symbol 的 scale 配置
struct SymbolConfig {
    /// 价格的小数位数 (例如 8 表示 10^8 scale)
    price_scale: u8,
    /// 数量的小数位数
    qty_scale: u8,
    /// 价格 scale 的 10 的幂次方 (预计算, 例如 10^8 = 100_000_000)
    price_multiplier: i64,
    /// 数量 scale 的 10 的幂次方
    qty_multiplier: u64,
    /// 最小价格变动 (tick size), 以 scaled integer 表示
    tick_size: Price,
    /// 最小数量变动 (lot size), 以 scaled integer 表示
    lot_size: Quantity,
}
```

### 4.2 算术运算

```rust
impl Price {
    /// 检查价格是否对齐到 tick size
    #[inline(always)]
    pub fn is_aligned(self, tick_size: Price) -> bool {
        self.0 % tick_size.0 == 0
    }

    /// 对齐到 tick size (向下取整)
    #[inline(always)]
    pub fn align_down(self, tick_size: Price) -> Price {
        Price(self.0 - self.0 % tick_size.0)
    }
}

impl Quantity {
    pub const ZERO: Quantity = Quantity(0);

    /// checked 减法（非热路径使用）
    #[inline(always)]
    pub fn checked_sub(self, rhs: Quantity) -> Option<Quantity> {
        self.0.checked_sub(rhs.0).map(Quantity)
    }

    /// 热路径减法（调用方必须保证 self >= rhs）
    /// SAFETY: 调用方确保不会下溢
    #[inline(always)]
    pub fn saturating_sub(self, rhs: Quantity) -> Quantity {
        Quantity(self.0.saturating_sub(rhs.0))
    }
}

/// 价格 * 数量 = 金额 (notional value)
/// 使用 u128 中间值防止溢出
///
/// 例如: price = 50000.00000000 (5_000_000_000_000i64, scale=8)
///       qty   = 1.50000000     (150_000_000u64, scale=8)
///       notional = price * qty / scale = 75000.00000000
///
/// 中间值: 5_000_000_000_000 * 150_000_000 = 750_000_000_000_000_000_000 (需要 u128)
#[inline(always)]
pub fn notional_value(price: Price, qty: Quantity, scale: u64) -> u64 {
    let intermediate = (price.0.unsigned_abs() as u128) * (qty.0 as u128);
    (intermediate / scale as u128) as u64
}
```

### 4.3 溢出安全保证

| 路径 | 策略 | 理由 |
|------|------|------|
| 风控检查 (pre-trade) | `checked_*` 操作 | 非热路径，安全第一 |
| 撮合 qty 计算 | `saturating_sub` / `min` | 热路径，但逻辑保证不溢出 |
| notional 计算 | `u128` 中间值 | price * qty 可能超过 u64 |
| sequence number | `wrapping_add` | u64 不会在实际中回绕 (584 年 @ 1B/sec) |

---

## 5. Lock-Free 通信

### 5.1 SPSC Ring Buffer 设计

所有线程间通信使用自研的 SPSC (Single-Producer Single-Consumer) 环形缓冲区。
这是 Tachyon 的核心基础设施。

```
        Producer (write)                              Consumer (read)
            │                                              │
            ▼                                              ▼
    ┌───────────────────────────────────────────────────────────────┐
    │  tail ──►  [ ][ ][ ][D][E][F][G][ ][ ]  ◄── head            │
    │            0   1   2  3   4   5   6  7   8                   │
    │                      ▲               ▲                       │
    │                      │               │                       │
    │                    head=3          tail=7                     │
    │                                                              │
    │  可读区间: [head, tail)  = slots 3,4,5,6                     │
    │  可写区间: [tail, head + capacity) = slots 7,8,0,1,2        │
    │                                                              │
    │  capacity = 8 (必须是 2 的幂)                                │
    │  mask = capacity - 1 = 7                                     │
    │  slot_index = sequence & mask  (位运算取模)                   │
    └───────────────────────────────────────────────────────────────┘
```

### 5.2 实现代码

```rust
use std::cell::UnsafeCell;
use std::mem::MaybeUninit;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Cache line 对齐的原子计数器
/// Apple Silicon (M1/M2/M3) 的 cache line 是 128 bytes
/// x86-64 通常是 64 bytes
/// 使用 128 bytes 对齐确保跨平台兼容
#[repr(align(128))]
struct CachePadded<T> {
    value: T,
}

impl<T> CachePadded<T> {
    fn new(value: T) -> Self {
        Self { value }
    }
}

/// Wait-free SPSC Ring Buffer
///
/// 性能特性:
/// - 入队: wait-free, 单次 atomic store (Release)
/// - 出队: wait-free, 单次 atomic load (Acquire)
/// - 零分配: 所有 slot 预分配
/// - 零拷贝: 直接写入 slot
pub struct SpscRingBuffer<T> {
    /// 预分配的 slot 数组
    buffer: Box<[UnsafeCell<MaybeUninit<T>>]>,
    /// capacity，必须是 2 的幂
    capacity: usize,
    /// capacity - 1，用于位运算取模
    mask: usize,
    /// 消费者的读位置（只有消费者写，生产者读）
    head: CachePadded<AtomicUsize>,
    /// 生产者的写位置（只有生产者写，消费者读）
    tail: CachePadded<AtomicUsize>,
}

// SAFETY: T: Send 时，SpscRingBuffer 可以跨线程共享
// 因为 SPSC 保证同一时刻只有一个线程读，一个线程写
unsafe impl<T: Send> Send for SpscRingBuffer<T> {}
unsafe impl<T: Send> Sync for SpscRingBuffer<T> {}

impl<T> SpscRingBuffer<T> {
    /// 创建指定容量的 ring buffer
    /// capacity 会被向上取整到 2 的幂
    pub fn new(capacity: usize) -> Self {
        let capacity = capacity.next_power_of_two();
        let buffer: Vec<UnsafeCell<MaybeUninit<T>>> =
            (0..capacity).map(|_| UnsafeCell::new(MaybeUninit::uninit())).collect();

        Self {
            buffer: buffer.into_boxed_slice(),
            capacity,
            mask: capacity - 1,
            head: CachePadded::new(AtomicUsize::new(0)),
            tail: CachePadded::new(AtomicUsize::new(0)),
        }
    }

    /// 生产者: 尝试入队一个元素
    /// 返回 true 表示成功，false 表示队列已满
    ///
    /// 只能由 Producer 线程调用
    #[inline]
    pub fn try_push(&self, value: T) -> Result<(), T> {
        let tail = self.tail.value.load(Ordering::Relaxed);
        let head = self.head.value.load(Ordering::Acquire);

        // 队列满判定: tail - head == capacity
        if tail - head >= self.capacity {
            return Err(value);
        }

        let slot = tail & self.mask;
        // SAFETY: 单生产者保证此 slot 不会被消费者访问
        unsafe {
            (*self.buffer[slot].get()).write(value);
        }

        // Release: 确保 value 的写入对消费者可见
        self.tail.value.store(tail + 1, Ordering::Release);
        Ok(())
    }

    /// 消费者: 尝试出队一个元素
    ///
    /// 只能由 Consumer 线程调用
    #[inline]
    pub fn try_pop(&self) -> Option<T> {
        let head = self.head.value.load(Ordering::Relaxed);
        let tail = self.tail.value.load(Ordering::Acquire);

        // 队列空判定
        if head >= tail {
            return None;
        }

        let slot = head & self.mask;
        // SAFETY: 单消费者保证此 slot 不会被生产者覆写
        let value = unsafe { (*self.buffer[slot].get()).assume_init_read() };

        // Release: 通知生产者此 slot 已消费
        self.head.value.store(head + 1, Ordering::Release);
        Some(value)
    }

    /// 消费者: 批量出队 (LMAX catch-up pattern)
    /// 一次性读取所有可用元素，减少 atomic 操作次数
    /// 使用 ArrayVec 避免堆分配，BATCH_CAP 应匹配典型批次大小
    #[inline]
    pub fn drain_batch<const BATCH_CAP: usize>(
        &self,
        out: &mut ArrayVec<T, BATCH_CAP>,
    ) -> usize {
        let head = self.head.value.load(Ordering::Relaxed);
        let tail = self.tail.value.load(Ordering::Acquire);

        let available = (tail - head).min(BATCH_CAP);
        if available == 0 {
            return 0;
        }

        for i in 0..available {
            let slot = (head + i) & self.mask;
            unsafe {
                out.push_unchecked((*self.buffer[slot].get()).assume_init_read());
            }
        }

        self.head.value.store(head + available, Ordering::Release);
        available
    }
}
```

### 5.3 内存顺序选择理由

| 操作 | Ordering | 理由 |
|------|----------|------|
| 生产者读 head | `Acquire` | 需要看到消费者最新释放的 slot |
| 生产者写 tail | `Release` | 确保 slot 内容的写入先于 tail 更新可见 |
| 消费者读 tail | `Acquire` | 需要看到生产者最新写入的 slot 内容 |
| 消费者写 head | `Release` | 通知生产者 slot 已释放 |
| 读己方指针 | `Relaxed` | 只有自己写，读自己的值不需要同步 |

**为什么不用 `SeqCst`**：`SeqCst` 在 ARM (Apple Silicon) 上会插入 `dmb ish` 全屏障，
代价显著高于 `Release`/`Acquire` 的半屏障。SPSC 场景只有两个线程，不需要全序保证。

### 5.4 False Sharing 防护

```
Without CachePadded:                 With CachePadded:
┌─────────────────────────┐          ┌─────────────────────────────────┐
│ cache line (128 bytes)  │          │ cache line 0 (128 bytes)        │
│ ┌──────┐ ┌──────┐      │          │ ┌──────┐ ┌──padding──────────┐  │
│ │ head │ │ tail │       │          │ │ head │ │  120 bytes       │  │
│ └──────┘ └──────┘       │          │ └──────┘ └──────────────────┘  │
│                         │          ├─────────────────────────────────┤
│ Producer 写 tail 会让   │          │ cache line 1 (128 bytes)        │
│ Consumer 的 head 缓存   │          │ ┌──────┐ ┌──padding──────────┐  │
│ 失效 (false sharing)    │          │ │ tail │ │  120 bytes       │  │
└─────────────────────────┘          │ └──────┘ └──────────────────┘  │
                                     └─────────────────────────────────┘
                                     head 和 tail 在不同 cache line，
                                     互不干扰
```

---

## 6. 内存架构

### 6.1 分配器策略

| 层级 | 分配器 | 用途 |
|------|--------|------|
| Global allocator | `mimalloc` | 替代系统 malloc，更好的多线程性能和碎片控制 |
| Order 对象 | `slab` crate | 带 free list 的 Vec，O(1) alloc/free，索引稳定 |
| PriceLevel 对象 | `slab` crate | 同上 |
| 请求级临时对象 | `bumpalo` | Arena allocator，批量释放，零碎片 |
| 撮合结果 (fills) | `SmallVec<[Fill; 8]>` | 栈上分配 (典型情况 <=8 fills)，溢出才 heap |
| 固定容量集合 | `ArrayVec` | 编译期固定大小，纯栈上 |
| Ring buffer slots | 预分配 `Box<[T]>` | 创建时一次性分配 |

### 6.2 全局分配器配置

```rust
// main.rs 或 lib.rs 顶部
use mimalloc::MiMalloc;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;
```

### 6.3 零分配热路径证明

撮合热路径是从 "ring buffer 收到 Command" 到 "events 推入 output ring buffer" 的
完整链路。以下证明此路径上零堆分配：

```
热路径操作链:
                                                              堆分配?
1. ring.try_pop()           -> 读预分配 slot                    否
2. risk_check(&cmd)         -> 纯计算，无分配                    否
3. orders.insert(order)     -> Slab free list pop              否 (预分配)
4. order_map.insert(id, h)  -> HashMap insert                  *可能* (rehash)
5. levels[idx].link(order)  -> 修改 usize 索引                  否
6. match_incoming()         -> 循环比较+修改 qty                 否
7.   fills.push(fill)       -> SmallVec 栈上 push               否 (<=8 fills)
8.   orders.remove(idx)     -> Slab free list push              否
9. output_ring.try_push(ev) -> 写预分配 slot                    否
```

**第 4 步的处理**：`HashMap` 在 load factor 超过阈值时会 rehash 并分配新的 backing
array。解决方案：

```rust
/// OrderBook 初始化时预分配 HashMap 容量
fn new(symbol_id: u32, config: &SymbolConfig) -> Self {
    Self {
        // 预留足够容量，避免运行时 rehash
        // 典型值：单 symbol 最大 100 万挂单
        order_map: HashMap::with_capacity(1_000_000),
        orders: Slab::with_capacity(1_000_000),
        levels: Slab::with_capacity(10_000),  // 价格级别数通常远少于订单数
        // ...
    }
}
```

同时，`SmallVec<[Fill; 8]>` 保证在 8 个 fill 以内不做堆分配。单笔订单产生超过 8 个
fill 的情况极为罕见，即使发生，也只有一次堆分配（amortized）。

### 6.4 bumpalo Arena 使用场景

```rust
use bumpalo::Bump;

/// 每个请求创建一个 arena，请求结束后批量释放
/// 用于网关层的协议解析临时对象
fn handle_fix_message(raw: &[u8], arena: &Bump) -> Result<Command> {
    // 在 arena 中分配临时的 FIX field map
    let fields = arena.alloc_slice_fill_default::<FixField>(64);
    // 解析 FIX 消息...
    // arena 在函数返回后由调用方 reset()
    Ok(command)
}
```

---

## 7. 撮合算法

### 7.1 价格-时间优先 (Price-Time Priority / FIFO)

撮合规则：
1. **价格优先**：买单价高者优先成交，卖单价低者优先成交
2. **时间优先**：同价格下，先到的订单优先成交 (FIFO)

### 7.2 核心撮合函数

```rust
use smallvec::SmallVec;

/// 撮合结果（栈上分配，典型 <=8 fills）
type Fills = SmallVec<[Fill; 8]>;

/// 单次 fill 记录
struct Fill {
    maker_order_id: OrderId,
    taker_order_id: OrderId,
    price: Price,
    quantity: Quantity,
    maker_account: u64,
    taker_account: u64,
}

/// 撮合引入的订单
///
/// 返回 fills 列表和订单的最终状态
fn match_incoming_order(
    book: &mut OrderBook,
    incoming: &mut IncomingOrder,
) -> (Fills, OrderStatus) {
    let mut fills: Fills = SmallVec::new();

    // FOK: 预检查流动性是否足够
    if incoming.time_in_force == TimeInForce::FOK {
        if !check_fok_liquidity(book, incoming) {
            return (fills, OrderStatus::Rejected(RejectReason::InsufficientLiquidity));
        }
    }

    // PostOnly: 如果会立即成交则拒绝
    if incoming.time_in_force == TimeInForce::PostOnly {
        if would_match(book, incoming) {
            return (fills, OrderStatus::Rejected(RejectReason::PostOnlyWouldMatch));
        }
    }

    // 主撮合循环
    while incoming.remaining_qty > Quantity::ZERO {
        // 获取对手方最优价格级别
        let opposite_best = match incoming.side {
            Side::Buy  => book.best_ask(),
            Side::Sell => book.best_bid(),
        };

        let Some(best_price) = opposite_best else {
            break;  // 对手方订单簿为空
        };

        // 价格检查：是否可以成交
        if !is_price_crossable(incoming.side, incoming.price, best_price) {
            break;  // 价格不匹配
        }

        // 获取该价格级别
        let level_idx = match incoming.side {
            Side::Buy  => book.asks[&best_price],
            Side::Sell => book.bids[&best_price],
        };

        // 逐单撮合该价格级别中的订单
        match_at_level(book, incoming, level_idx, best_price, &mut fills);

        // 如果价格级别已空，移除它
        if book.levels[level_idx].order_count == 0 {
            match incoming.side {
                Side::Buy  => { book.asks.remove(&best_price); }
                Side::Sell => { book.bids.remove(&best_price); }
            }
            book.levels.remove(level_idx);
            book.update_bbo();
        }
    }

    // 确定最终状态
    let status = determine_final_status(incoming, &fills);
    (fills, status)
}

/// 在单个价格级别内撮合
fn match_at_level(
    book: &mut OrderBook,
    incoming: &mut IncomingOrder,
    level_idx: PriceLevelIdx,
    match_price: Price,
    fills: &mut Fills,
) {
    let mut current = book.levels[level_idx].head;

    while incoming.remaining_qty > Quantity::ZERO {
        let Some(order_idx) = current else { break };

        let resting = &book.orders[order_idx];

        // 自成交防范 (STP)
        if resting.account_id == incoming.account_id {
            match handle_self_trade(book, incoming, order_idx) {
                STPAction::CancelMaker => {
                    current = book.orders[order_idx].next;
                    book.remove_order_from_level(level_idx, order_idx);
                    book.order_map.remove(&resting.id);
                    continue;
                }
                STPAction::CancelTaker => {
                    incoming.remaining_qty = Quantity::ZERO;
                    break;
                }
                STPAction::CancelBoth => {
                    current = book.orders[order_idx].next;
                    book.remove_order_from_level(level_idx, order_idx);
                    book.order_map.remove(&resting.id);
                    incoming.remaining_qty = Quantity::ZERO;
                    break;
                }
            }
        }

        // 计算成交量
        let fill_qty = std::cmp::min(incoming.remaining_qty, resting.remaining_qty);

        // 记录 fill
        fills.push(Fill {
            maker_order_id: resting.id,
            taker_order_id: incoming.id,
            price: match_price,
            quantity: fill_qty,
            maker_account: resting.account_id,
            taker_account: incoming.account_id,
        });

        // 更新数量
        incoming.remaining_qty = Quantity(incoming.remaining_qty.0 - fill_qty.0);
        let new_resting_qty = Quantity(resting.remaining_qty.0 - fill_qty.0);

        // 保存 next 指针（因为可能要删除当前节点）
        let next = book.orders[order_idx].next;

        if new_resting_qty == Quantity::ZERO {
            // maker 完全成交，从链表中移除
            book.remove_order_from_level(level_idx, order_idx);
            book.order_map.remove(&book.orders[order_idx].id);
        } else {
            // maker 部分成交，更新剩余量
            book.orders[order_idx].remaining_qty = new_resting_qty;
            book.levels[level_idx].total_quantity =
                Quantity(book.levels[level_idx].total_quantity.0 - fill_qty.0);
        }

        current = next;
    }
}
```

### 7.3 价格匹配判定

```rust
/// 检查 incoming 订单的价格是否可以与对手方价格成交
#[inline(always)]
fn is_price_crossable(incoming_side: Side, incoming_price: Price, opposite_price: Price) -> bool {
    match incoming_side {
        // 买单：出价 >= 卖价 才能成交
        Side::Buy => incoming_price.0 >= opposite_price.0,
        // 卖单：出价 <= 买价 才能成交
        Side::Sell => incoming_price.0 <= opposite_price.0,
    }
}

/// Market order 视为价格无限好
/// Buy market: price = i64::MAX (愿意以任何价格买入)
/// Sell market: price = i64::MIN (愿意以任何价格卖出)
```

### 7.4 订单类型处理

**IOC (Immediate-or-Cancel)**：

```rust
fn determine_final_status(incoming: &IncomingOrder, fills: &Fills) -> OrderStatus {
    match incoming.time_in_force {
        TimeInForce::IOC => {
            // IOC: 成交了多少算多少，剩余取消
            if fills.is_empty() {
                OrderStatus::Cancelled  // 完全没成交
            } else if incoming.remaining_qty == Quantity::ZERO {
                OrderStatus::FullyFilled
            } else {
                OrderStatus::PartiallyFilledAndCancelled
            }
            // 不插入订单簿
        }
        TimeInForce::FOK => {
            // FOK: 前面已经做了流动性预检查，到这里一定是全部成交
            OrderStatus::FullyFilled
        }
        TimeInForce::GTC => {
            if incoming.remaining_qty == Quantity::ZERO {
                OrderStatus::FullyFilled
            } else if !fills.is_empty() {
                OrderStatus::PartiallyFilled  // 剩余部分进入订单簿
            } else {
                OrderStatus::Accepted  // 完全没成交，整单进入订单簿
            }
        }
        TimeInForce::PostOnly => {
            // PostOnly 前面已检查不会立即成交，直接进入订单簿
            OrderStatus::Accepted
        }
    }
}
```

**FOK (Fill-or-Kill) 流动性预检查**：

```rust
/// FOK 需要在撮合前确认是否有足够的流动性
/// 如果不够，直接拒绝，不做部分成交
///
/// 注意: BTreeMap::iter() 返回 Iter，iter().rev() 返回 Rev<Iter>，
/// 二者是不同类型，无法用同一个 match 分支返回。因此拆成两个独立分支。
fn check_fok_liquidity(book: &OrderBook, incoming: &IncomingOrder) -> bool {
    let mut remaining = incoming.remaining_qty;

    // 内联闭包：对给定的 (price, level_idx) 迭代器检查流动性
    let mut check = |price: Price, level_idx: usize| -> std::ops::ControlFlow<bool> {
        if !is_price_crossable(incoming.side, incoming.price, price) {
            return std::ops::ControlFlow::Break(false);
        }
        let level = &book.levels[level_idx];
        if level.total_quantity.0 >= remaining.0 {
            return std::ops::ControlFlow::Break(true); // 当前级别就够了
        }
        remaining = Quantity(remaining.0 - level.total_quantity.0);
        std::ops::ControlFlow::Continue(())
    };

    match incoming.side {
        Side::Buy => {
            // 买单吃卖盘，升序遍历 asks (最低卖价优先)
            for (&price, &level_idx) in book.asks.iter() {
                match check(price, level_idx) {
                    std::ops::ControlFlow::Break(result) => return result,
                    std::ops::ControlFlow::Continue(()) => {}
                }
            }
        }
        Side::Sell => {
            // 卖单吃买盘，降序遍历 bids (最高买价优先)
            for (&price, &level_idx) in book.bids.iter().rev() {
                match check(price, level_idx) {
                    std::ops::ControlFlow::Break(result) => return result,
                    std::ops::ControlFlow::Continue(()) => {}
                }
            }
        }
    }

    false // 遍历完所有可成交级别，流动性仍不够
}
```

### 7.5 Stop Orders 触发机制

```rust
/// Stop order 存储在独立的触发簿中
/// 当市场价格达到触发价时，转换为普通订单进入撮合
struct TriggerBook {
    /// 价格 >= trigger_price 时触发的 stop buy orders
    /// BTreeMap 升序: 最低触发价优先检查
    stop_buys: BTreeMap<Price, Vec<StopOrder>>,
    /// 价格 <= trigger_price 时触发的 stop sell orders
    /// BTreeMap 降序: 最高触发价优先检查
    stop_sells: BTreeMap<Price, Vec<StopOrder>>,
}

/// 每次成交后检查是否有 stop order 需要触发
fn check_triggers(trigger_book: &mut TriggerBook, last_trade_price: Price) -> Vec<Command> {
    let mut triggered = Vec::new();

    // 检查 stop buy: 价格上涨到触发价
    while let Some((&trigger_price, _)) = trigger_book.stop_buys.first_key_value() {
        if last_trade_price.0 >= trigger_price.0 {
            let orders = trigger_book.stop_buys.remove(&trigger_price).unwrap();
            for stop in orders {
                triggered.push(stop.into_command());
            }
        } else {
            break;
        }
    }

    // 检查 stop sell: 价格下跌到触发价
    while let Some((&trigger_price, _)) = trigger_book.stop_sells.last_key_value() {
        if last_trade_price.0 <= trigger_price.0 {
            let orders = trigger_book.stop_sells.remove(&trigger_price).unwrap();
            for stop in orders {
                triggered.push(stop.into_command());
            }
        } else {
            break;
        }
    }

    triggered
}
```

### 7.6 自成交防范 (Self-Trade Prevention, STP)

```rust
#[derive(Clone, Copy)]
enum STPMode {
    /// 取消挂单方 (maker)，taker 继续撮合
    CancelMaker,
    /// 取消吃单方 (taker)，maker 保留
    CancelTaker,
    /// 双方都取消
    CancelBoth,
}

enum STPAction {
    CancelMaker,
    CancelTaker,
    CancelBoth,
}

fn handle_self_trade(
    _book: &OrderBook,
    incoming: &IncomingOrder,
    _maker_idx: OrderIdx,
) -> STPAction {
    match incoming.stp_mode {
        STPMode::CancelMaker => STPAction::CancelMaker,
        STPMode::CancelTaker => STPAction::CancelTaker,
        STPMode::CancelBoth  => STPAction::CancelBoth,
    }
}
```

---

## 8. 事件系统

### 8.1 事件定义

所有状态变更通过事件表达。事件是 WAL 的写入单元，也是状态恢复的基础。

```rust
/// 引擎产出的事件
/// 每个事件包含 sequence number 用于全序排列
#[derive(Clone, Debug)]
pub enum EngineEvent {
    /// 订单被接受并进入订单簿
    OrderAccepted {
        sequence: u64,
        order_id: OrderId,
        symbol_id: u32,
        side: Side,
        price: Price,
        quantity: Quantity,
        timestamp_ns: u64,
    },

    /// 订单被拒绝（风控不通过、PostOnly 冲突等）
    OrderRejected {
        sequence: u64,
        order_id: OrderId,
        reason: RejectReason,
        timestamp_ns: u64,
    },

    /// 订单被取消
    OrderCancelled {
        sequence: u64,
        order_id: OrderId,
        remaining_qty: Quantity,
        timestamp_ns: u64,
    },

    /// 订单过期
    OrderExpired {
        sequence: u64,
        order_id: OrderId,
        timestamp_ns: u64,
    },

    /// 成交事件（每个 fill 产生一个 Trade 事件）
    Trade {
        sequence: u64,
        trade_id: u64,
        symbol_id: u32,
        price: Price,
        quantity: Quantity,
        maker_order_id: OrderId,
        taker_order_id: OrderId,
        maker_account: u64,
        taker_account: u64,
        maker_side: Side,
        timestamp_ns: u64,
    },

    /// 订单簿深度变更（用于 L2 市场数据推送）
    DepthUpdate {
        sequence: u64,
        symbol_id: u32,
        side: Side,
        price: Price,
        /// 新的总量（0 表示该价格级别已清空）
        new_quantity: Quantity,
        timestamp_ns: u64,
    },

    /// 快照创建完成
    SnapshotCreated {
        sequence: u64,
        symbol_id: u32,
        checkpoint_sequence: u64,
        timestamp_ns: u64,
    },

    /// 交易对添加
    SymbolAdded {
        sequence: u64,
        symbol_id: u32,
        tick_size: Price,
        lot_size: Quantity,
        price_scale: u8,
        qty_scale: u8,
    },

    /// 交易暂停
    TradingHalted {
        sequence: u64,
        symbol_id: u32,
        reason: HaltReason,
        timestamp_ns: u64,
    },

    /// 交易恢复
    TradingResumed {
        sequence: u64,
        symbol_id: u32,
        timestamp_ns: u64,
    },
}

#[derive(Clone, Debug)]
pub enum RejectReason {
    InsufficientLiquidity,
    PostOnlyWouldMatch,
    PriceBandViolation,
    MaxOrderSizeExceeded,
    MaxPositionExceeded,
    RateLimitExceeded,
    TradingHalted,
    InvalidTickSize,
    InvalidLotSize,
    UnknownSymbol,
}

#[derive(Clone, Debug)]
pub enum HaltReason {
    CircuitBreaker,
    Manual,
    Maintenance,
}
```

### 8.2 Event Sourcing 保证

**确定性回放 (Deterministic Replay)**：给定相同的初始状态和相同的 Command 序列（带相同
的 sequence number），引擎必须产出完全相同的 Event 序列。

这要求：
1. 所有 Command 在处理前已有确定的 sequence number
2. Matcher 内部不依赖系统时钟（时间戳由 Sequencer 统一分配）
3. 没有随机性或非确定性分支
4. HashMap 的遍历顺序不影响结果（我们只通过 BTreeMap 遍历价格级别）

```
Replay 流程:
                                      验证
  WAL Events ──► Replay Engine ──► 产出 Events ──► 逐一比对原始 Events
                      │
                      ▼
                 重建 OrderBook 状态
```

---

## 9. 持久化层

### 9.1 WAL (Write-Ahead Log) 设计

WAL 使用 memory-mapped file，追加写入，顺序 I/O 最大化磁盘吞吐。

```
WAL 文件布局:
┌──────────┬──────────┬──────────┬──────────┬─────┐
│ Header   │ Entry 0  │ Entry 1  │ Entry 2  │ ... │
│ (固定)    │ (变长)    │ (变长)    │ (变长)    │     │
└──────────┴──────────┴──────────┴──────────┴─────┘

Entry 格式:
┌──────────┬──────────┬──────────┬──────────────┬──────────┐
│ length   │ sequence │ checksum │ payload      │ padding  │
│ (u32)    │ (u64)    │ (u32)    │ (变长 bytes)  │ (对齐)    │
└──────────┴──────────┴──────────┴──────────────┴──────────┘
  4 bytes    8 bytes    4 bytes    N bytes        0-7 bytes (8-byte aligned)
```

```rust
use std::fs::{File, OpenOptions};
use std::io;

/// WAL fsync 策略
#[derive(Clone, Copy)]
pub enum SyncPolicy {
    /// 每个 entry 都 fsync — 最安全，延迟最高
    EveryEntry,
    /// 每 N 个 entry 或每 T 微秒 fsync — 平衡安全性和性能
    Batched { max_entries: u32, max_micros: u64 },
    /// 不主动 fsync，依赖 OS page cache 刷盘 — 最快，可能丢数据
    Async,
}

/// WAL Header
#[repr(C)]
struct WalHeader {
    magic: [u8; 8],       // b"TACHYWAL"
    version: u32,
    symbol_id: u32,
    created_at_ns: u64,
    first_sequence: u64,
}

/// WAL Entry Header (固定大小部分)
#[repr(C)]
struct WalEntryHeader {
    /// payload 长度 (不含 header 和 padding)
    length: u32,
    /// 全局序列号
    sequence: u64,
    /// CRC32 of payload
    checksum: u32,
}

/// Memory-mapped WAL writer
pub struct WalWriter {
    file: File,
    /// mmap 映射的内存区域
    mmap: memmap2::MmapMut,
    /// 当前写入位置
    write_pos: usize,
    /// 文件总容量
    capacity: usize,
    /// fsync 策略
    sync_policy: SyncPolicy,
    /// 上次 fsync 以来的 entry 数
    entries_since_sync: u32,
    /// 上次 fsync 的时间 (纳秒)
    last_sync_ns: u64,
}

impl WalWriter {
    /// 追加一个 entry 到 WAL
    pub fn append(&mut self, sequence: u64, payload: &[u8]) -> io::Result<()> {
        let header = WalEntryHeader {
            length: payload.len() as u32,
            sequence,
            checksum: crc32fast::hash(payload),
        };

        let header_size = std::mem::size_of::<WalEntryHeader>();
        let padded_payload_len = (payload.len() + 7) & !7;  // 8-byte align
        let total_entry_size = header_size + padded_payload_len;

        // 检查容量，必要时扩展文件
        if self.write_pos + total_entry_size > self.capacity {
            self.grow()?;
        }

        // 写入 header
        let header_bytes: &[u8; std::mem::size_of::<WalEntryHeader>()] =
            unsafe { &*(&header as *const WalEntryHeader as *const _) };
        self.mmap[self.write_pos..self.write_pos + header_size]
            .copy_from_slice(header_bytes);

        // 写入 payload
        self.mmap[self.write_pos + header_size..self.write_pos + header_size + payload.len()]
            .copy_from_slice(payload);

        self.write_pos += total_entry_size;
        self.entries_since_sync += 1;

        // 按策略执行 fsync
        self.maybe_sync()?;

        Ok(())
    }

    fn maybe_sync(&mut self) -> io::Result<()> {
        match self.sync_policy {
            SyncPolicy::EveryEntry => {
                self.mmap.flush()?;
            }
            SyncPolicy::Batched { max_entries, max_micros } => {
                let now_ns = timestamp_ns();
                let elapsed_us = (now_ns - self.last_sync_ns) / 1000;
                if self.entries_since_sync >= max_entries || elapsed_us >= max_micros {
                    self.mmap.flush()?;
                    self.entries_since_sync = 0;
                    self.last_sync_ns = now_ns;
                }
            }
            SyncPolicy::Async => {
                // 不做 fsync，依赖 OS
            }
        }
        Ok(())
    }
}
```

### 9.2 Snapshot (快照)

定期创建 OrderBook 的全量快照，用于加速恢复。

```rust
/// 使用 rkyv 进行零拷贝序列化
/// rkyv 的反序列化是 O(1) — 直接将 mmap 的内存解释为结构体
///
/// Snapshot 文件: {symbol_id}_{sequence}.snap
///
/// 快照内容:
/// - OrderBook 的完整状态 (所有价格级别和订单)
/// - 状态哈希 (用于验证一致性)
/// - 创建时的 sequence number (用于 WAL replay 起点)

/// 快照创建策略
struct SnapshotPolicy {
    /// 每处理 N 个 command 后创建快照
    interval_commands: u64,
    /// 或每 T 秒创建快照
    interval_seconds: u64,
}
```

### 9.3 恢复流程

```
┌─────────────────────────────────────────────────────────────────────────┐
│                          Recovery Procedure                             │
│                                                                         │
│  1. 查找最新 snapshot 文件                                               │
│     └─► {symbol_id}_latest.snap                                        │
│                                                                         │
│  2. 加载 snapshot (rkyv zero-copy deserialization)                      │
│     └─► OrderBook state at sequence S                                  │
│                                                                         │
│  3. 打开 WAL 文件，定位到 sequence S+1                                   │
│     └─► 二分查找或顺序扫描                                                │
│                                                                         │
│  4. 逐条 replay WAL entries: S+1, S+2, ..., S+N                       │
│     └─► 重放每个 Command，验证产出的 Events 与 WAL 中记录的一致           │
│                                                                         │
│  5. 验证状态哈希                                                         │
│     └─► hash(current_state) == expected_hash                           │
│                                                                         │
│  6. 恢复完成，开始接受新的 Command                                       │
│                                                                         │
│  耗时: snapshot 加载 ~1ms + WAL replay ~10us/entry                     │
│  典型恢复: 1ms + 10000 entries * 10us = ~101ms                         │
└─────────────────────────────────────────────────────────────────────────┘
```

### 9.4 Journal Segment 归档

```
活跃 WAL 文件滚动:

segment_000001.wal (已归档, LZ4 compressed)
segment_000002.wal (已归档, LZ4 compressed)
segment_000003.wal ← 当前活跃 segment
                      │
                      ├─ 达到 max_segment_size (例如 256MB) 时
                      │  1. 关闭当前 segment
                      │  2. 创建新 segment
                      │  3. 异步线程 LZ4 压缩旧 segment
                      │
                      └─ 如果已有比旧 segment 更新的 snapshot
                         可以安全删除已归档的旧 segment
```

### 9.5 确定性重放 (Deterministic Replay) — 已实现

> 本节描述当前已实现的持久化和恢复机制，与上述 9.1-9.4 的原始设计规划有所不同。

#### 核心变更：WAL 存储 Command（输入）而非 Event（输出）

原始设计中 WAL 存储 `EngineEvent`（引擎输出事件），恢复时需要从事件反向重建订单簿状态。
这种方式复杂、脆弱、且不保证确定性。

当前实现改为存储 `WalCommand`（引擎输入命令），包含：

```rust
pub struct WalCommand {
    pub sequence: u64,      // 全局 WAL 序列号
    pub symbol_id: u32,     // 目标交易对
    pub command: Command,   // PlaceOrder / CancelOrder / ModifyOrder
    pub account_id: u64,    // 账户 ID
    pub timestamp: u64,     // 外部分配的时间戳（非 SystemTime::now()）
}
```

#### 写前日志语义

```
                   Bridge (tokio)
                       │
                  EngineCommand { command, timestamp, ... }
                       │
                       ▼
                 ┌─────────────────┐
            ①   │ WAL: append_cmd  │  ← 写入 WalCommand（真正的 Write-Ahead）
                 └────────┬────────┘
                          │
                          ▼
            ②    engine.process_command(cmd, account_id, timestamp)
                          │
                          ▼
            ③    events → response_tx → gateway → client
```

关键：命令在处理 **之前** 写入 WAL，确保崩溃恢复时不会丢失已确认的命令。

#### WAL 版本兼容

- **v2 (当前)**: `[version: u8=2][length: u32][sequence: u64][payload(WalCommand)][crc32]`
- **v1 (遗留)**: `[length: u32][sequence: u64][payload(EngineEvent)][crc32]`

读取时自动检测版本：首字节为 `2` 时按 v2 解析，否则按 v1 处理。
v1 遗留事件走旧恢复路径（从事件重建），v2 命令走确定性重放路径。

#### 确定性保证

`SymbolEngine::process_command` 是一个纯状态机：

- 输入：`(Command, account_id, timestamp)`
- 输出：`SmallVec<[EngineEvent; 8]>`
- **不调用** `SystemTime::now()` 或任何外部状态
- 相同的输入序列必然产生相同的输出序列

时间戳在 Gateway 的 `bridge.rs` 中生成（`SystemTime::now()`），嵌入到 `EngineCommand.timestamp` 中。
WAL 记录的是原始时间戳，重放时使用 WAL 中保存的时间戳，因此重放是完全确定性的。

#### 恢复流程

```
1. 加载最新 Snapshot (如有)
   └─► 恢复订单簿、引擎计数器 (sequence, next_order_id, trade_id_counter)

2. 读取 WAL 文件 (支持 v1 + v2 混合)
   └─► 跳过 sequence <= snapshot_sequence 的条目

3. v2 WalCommand → engine.process_command(cmd, account_id, timestamp)
   v1 LegacyEvent → 旧路径：从事件重建订单簿状态

4. 恢复完成，开始接受新命令
```

---

## 10. 协议栈

### 10.1 协议分层

```
┌─────────────────────────────────────────────────────────────┐
│                     External Protocols                       │
│  ┌──────────┐  ┌───────────┐  ┌──────────┐  ┌───────────┐  │
│  │ FIX 4.4  │  │ WebSocket │  │  gRPC    │  │   REST    │  │
│  │ (fefix)  │  │ (tokio-   │  │ (tonic)  │  │  (axum)   │  │
│  │          │  │  tungstenite)│ │          │  │           │  │
│  └────┬─────┘  └─────┬─────┘  └────┬─────┘  └─────┬─────┘  │
│       │              │              │              │         │
│  ─────┴──────────────┴──────────────┴──────────────┴─────── │
│                                                              │
│                    Gateway Layer (tokio async)                │
│            协议解析 → 验证 → 转换为 Command                    │
│                                                              │
│  ────────────────────────────────────────────────────────── │
│                                                              │
│                    SPSC Ring Buffer                           │
│            Command (Internal Binary Format)                  │
│                                                              │
│  ────────────────────────────────────────────────────────── │
│                                                              │
│                    Matching Engine Core                       │
│            只处理 binary Command，从不解析文本                  │
│                                                              │
├─────────────────────────────────────────────────────────────┤
│                     Internal Protocol                        │
│  ┌──────────────────────────────────────────────────────┐   │
│  │  SBE-Inspired Binary Encoding                        │   │
│  │  - 固定位置字段 (fixed-position fields)               │   │
│  │  - 零拷贝解码 (直接 cast &[u8] to &Struct)           │   │
│  │  - 无 schema evolution (内部协议，版本由部署控制)      │   │
│  └──────────────────────────────────────────────────────┘   │
│                                                              │
│                     Market Data Output                       │
│  ┌──────────────────────────────────────────────────────┐   │
│  │  ITCH-Like Binary Feed                               │   │
│  │  - 每个消息类型一个固定大小的 struct                    │   │
│  │  - 消息类型前缀 (1 byte message type)                 │   │
│  │  - 用于高频场景的低延迟行情推送                         │   │
│  └──────────────────────────────────────────────────────┘   │
└─────────────────────────────────────────────────────────────┘
```

### 10.2 内部二进制消息格式 (SBE-Inspired)

```rust
/// 内部 Command 格式：固定大小，零拷贝
/// 所有字段在固定偏移位置，可以直接 cast
#[repr(C, packed)]
pub struct CommandMsg {
    /// 消息类型
    msg_type: u8,           // offset 0
    _pad1: [u8; 3],         // offset 1  (alignment padding)
    /// 全局序列号 (Sequencer 分配)
    sequence: u64,          // offset 4
    /// 交易对 ID
    symbol_id: u32,         // offset 12
    /// 订单 ID
    order_id: u64,          // offset 16
    /// 账户 ID
    account_id: u64,        // offset 24
    /// 价格 (定点数)
    price: i64,             // offset 32
    /// 数量 (定点数)
    quantity: u64,           // offset 40
    /// 方向: 0=Buy, 1=Sell
    side: u8,               // offset 48
    /// 订单类型: 0=Limit, 1=Market
    order_type: u8,         // offset 49
    /// 有效期: 0=GTC, 1=IOC, 2=FOK, 3=PostOnly
    time_in_force: u8,      // offset 50
    /// STP 模式: 0=CancelMaker, 1=CancelTaker, 2=CancelBoth
    stp_mode: u8,           // offset 51
    /// 时间戳 (纳秒)
    timestamp_ns: u64,      // offset 52
    /// 预留
    _reserved: [u8; 4],     // offset 60
}                            // total: 64 bytes (一个 x86 cache line)

/// 消息类型枚举
const MSG_NEW_ORDER: u8 = 1;
const MSG_CANCEL: u8 = 2;
const MSG_MODIFY: u8 = 3;
const MSG_CANCEL_ALL: u8 = 4;

/// 零拷贝解码
#[inline(always)]
fn decode_command(buf: &[u8; 64]) -> &CommandMsg {
    // SAFETY: CommandMsg 是 #[repr(C, packed)]，64 bytes 固定大小
    unsafe { &*(buf.as_ptr() as *const CommandMsg) }
}
```

### 10.3 FIX 4.4 Gateway

```rust
/// FIX 4.4 协议网关 (使用 fefix crate)
/// 将 FIX 消息解析为内部 Command
///
/// 支持的 FIX 消息类型:
/// - NewOrderSingle (D)      → Command::NewOrder
/// - OrderCancelRequest (F)  → Command::Cancel
/// - OrderCancelReplaceRequest (G) → Command::Modify
///
/// 输出的 FIX 消息类型:
/// - ExecutionReport (8)     ← EngineEvent::Trade / OrderAccepted / etc.
/// - OrderCancelReject (9)   ← EngineEvent::OrderRejected
```

---

## 11. 共识适配层

### 11.1 ConsensusAdapter Trait

```rust
/// 共识适配器 trait
/// 所有 Command 在进入 Sequencer 之前通过共识层
///
/// 这使得 Tachyon 可以:
/// 1. 独立运行 (Standalone, 无共识)
/// 2. 集成 Raft (CFT, crash fault tolerance)
/// 3. 集成 HotStuff (BFT, byzantine fault tolerance)
/// 4. 集成自定义共识协议
pub trait ConsensusAdapter: Send + Sync {
    /// 提议一个 command 进入共识
    /// 返回 sequenced command (带全局序列号)
    fn propose(&self, command: Command) -> Result<SequencedCommand, ConsensusError>;

    /// 当 command 被共识层 commit 后的回调
    /// 在 Standalone 模式下，propose 成功即视为 committed
    fn on_committed(&self, seq: u64, command: &Command);
}

/// 带序列号的 command
pub struct SequencedCommand {
    pub sequence: u64,
    pub command: Command,
    pub timestamp_ns: u64,
}

/// 共识错误
pub enum ConsensusError {
    /// 不是 leader
    NotLeader { leader_hint: Option<String> },
    /// 超时
    Timeout,
    /// 内部错误
    Internal(String),
}
```

### 11.2 Standalone 模式 (Pass-Through)

```rust
/// 独立模式: 无共识，直接分配序列号
/// 用于单节点部署或嵌入式场景
pub struct StandaloneAdapter {
    next_sequence: AtomicU64,
}

impl ConsensusAdapter for StandaloneAdapter {
    fn propose(&self, command: Command) -> Result<SequencedCommand, ConsensusError> {
        let seq = self.next_sequence.fetch_add(1, Ordering::Relaxed);
        Ok(SequencedCommand {
            sequence: seq,
            command,
            timestamp_ns: timestamp_ns(),
        })
    }

    fn on_committed(&self, _seq: u64, _command: &Command) {
        // No-op in standalone mode
    }
}
```

### 11.3 Raft / HotStuff 适配 (Future)

```
┌──────────┐     propose      ┌──────────────┐     replicate    ┌──────────┐
│ Gateway  │ ───────────────► │ Raft Leader  │ ───────────────► │ Follower │
│          │                  │ (Consensus   │                  │          │
│          │ ◄─────────────── │  Adapter)    │ ◄─────────────── │          │
│          │   committed seq  └──────────────┘   ack            └──────────┘
│          │                         │
│          │                         ▼
│          │                  ┌──────────────┐
│          │                  │  Sequencer   │  ── 只有 committed commands
│          │                  │  + Matcher   │     才进入撮合
│          │                  └──────────────┘
└──────────┘
```

---

## 12. 风控管理

### 12.1 Pre-Trade 风控 (内联，撮合前)

风控检查在撮合主循环中 **同步内联执行**，不经过额外的线程或队列。
这确保风控延迟被计入总撮合延迟，且不会增加额外的通信开销。

```rust
/// Pre-trade 风控检查 (内联在 matcher 中)
///
/// 每项检查都是 O(1) 的简单比较，不涉及 I/O 或复杂计算
/// 总检查耗时目标: <10ns
struct RiskChecker {
    /// 价格带: 价格偏离参考价的最大百分比 (BPS, basis points)
    price_band_bps: u32,
    /// 参考价 (通常是最近一笔成交价或开盘价)
    reference_price: Price,
    /// 单笔最大订单量
    max_order_size: Quantity,
    /// 单账户最大挂单数
    max_orders_per_account: u32,
    /// Fat-finger 保护: 最大偏离标准差数
    max_price_std_devs: u32,
    /// 价格标准差 (滚动计算)
    price_std_dev: i64,
}

impl RiskChecker {
    /// 执行所有 pre-trade 检查
    #[inline]
    fn check(&self, cmd: &Command, account_order_count: u32) -> Result<(), RejectReason> {
        // 1. 价格带检查
        self.check_price_band(cmd.price)?;

        // 2. 最大订单量检查
        if cmd.quantity > self.max_order_size {
            return Err(RejectReason::MaxOrderSizeExceeded);
        }

        // 3. 每账户挂单数检查
        if account_order_count >= self.max_orders_per_account {
            return Err(RejectReason::RateLimitExceeded);
        }

        // 4. Fat-finger 检查
        self.check_fat_finger(cmd.price)?;

        Ok(())
    }

    /// 价格带检查: 价格不能偏离参考价超过 N bps
    #[inline]
    fn check_price_band(&self, price: Price) -> Result<(), RejectReason> {
        let ref_price = self.reference_price.0;
        if ref_price == 0 {
            return Ok(());  // 没有参考价时跳过
        }
        let deviation = ((price.0 - ref_price).unsigned_abs() as u128 * 10_000) / ref_price.unsigned_abs() as u128;
        if deviation > self.price_band_bps as u128 {
            return Err(RejectReason::PriceBandViolation);
        }
        Ok(())
    }

    /// Fat-finger 保护: 价格偏离超过 N 个标准差
    #[inline]
    fn check_fat_finger(&self, price: Price) -> Result<(), RejectReason> {
        if self.price_std_dev == 0 {
            return Ok(());
        }
        let deviation = (price.0 - self.reference_price.0).unsigned_abs();
        if deviation > (self.max_price_std_devs as i64 * self.price_std_dev).unsigned_abs() {
            return Err(RejectReason::PriceBandViolation);
        }
        Ok(())
    }
}
```

### 12.2 Circuit Breaker (断路器)

```rust
/// 断路器: 当价格在短时间内剧烈波动时暂停交易
struct CircuitBreaker {
    /// 价格变动阈值 (BPS)
    threshold_bps: u32,
    /// 检测时间窗口 (纳秒)
    window_ns: u64,
    /// 暂停交易的持续时间 (纳秒)
    halt_duration_ns: u64,
    /// 窗口起始价格
    window_start_price: Price,
    /// 窗口起始时间
    window_start_ns: u64,
    /// 当前是否暂停
    is_halted: bool,
    /// 暂停开始时间
    halt_start_ns: u64,
}

impl CircuitBreaker {
    /// 每次成交后调用，检查是否需要触发断路器
    fn on_trade(&mut self, price: Price, timestamp_ns: u64) -> Option<HaltReason> {
        if self.is_halted {
            // 检查是否可以恢复
            if timestamp_ns - self.halt_start_ns >= self.halt_duration_ns {
                self.is_halted = false;
                self.window_start_price = price;
                self.window_start_ns = timestamp_ns;
            }
            return None;
        }

        // 检查时间窗口是否过期，需要重置
        if timestamp_ns - self.window_start_ns >= self.window_ns {
            self.window_start_price = price;
            self.window_start_ns = timestamp_ns;
            return None;
        }

        // 计算价格变动
        let ref_price = self.window_start_price.0;
        if ref_price == 0 {
            return None;
        }
        let change_bps = ((price.0 - ref_price).unsigned_abs() as u128 * 10_000)
            / ref_price.unsigned_abs() as u128;

        if change_bps > self.threshold_bps as u128 {
            self.is_halted = true;
            self.halt_start_ns = timestamp_ns;
            return Some(HaltReason::CircuitBreaker);
        }

        None
    }
}
```

---

## 13. 编译与性能优化

### 13.1 Release Profile

```toml
# Cargo.toml (workspace root)

[profile.release]
# Link-Time Optimization: 跨 crate 内联优化
# "fat" 最慢编译但最优结果
lto = "fat"

# 单 codegen unit: 最大化内联和优化机会
# 代价是增量编译变慢
codegen-units = 1

# panic 时直接 abort，而不是 unwind
# 减少代码体积，消除 unwind 表
panic = "abort"

# 最高优化等级
opt-level = 3

# 开启 strip，减小二进制体积
strip = true

# debug info 用于 profiling (不影响运行性能)
debug = 1

[profile.bench]
inherits = "release"
# benchmark 需要完整 debug info
debug = 2
```

### 13.2 RUSTFLAGS 配置

```bash
# 编译时使用当前 CPU 的全部指令集
# 包括 AVX2, BMI2, POPCNT 等 (x86-64)
# 或 NEON, SVE 等 (ARM/Apple Silicon)
export RUSTFLAGS="-C target-cpu=native"

# 可选: 启用特定的 target feature
# export RUSTFLAGS="-C target-cpu=native -C target-feature=+avx2,+bmi2"
```

### 13.3 PGO (Profile-Guided Optimization) Pipeline

```bash
#!/bin/bash
# PGO 构建流程

# Step 1: 构建 instrumented 版本
RUSTFLAGS="-Cprofile-generate=/tmp/pgo-data" \
    cargo build --release --bin tachyon-server

# Step 2: 运行 instrumented 版本处理真实 workload
./target/release/tachyon-server &
PID=$!
# 运行基准测试工作负载 (模拟真实交易流)
cargo run --release --bin tachyon-bench -- --profile-workload
kill $PID

# Step 3: 合并 profiling 数据
llvm-profdata merge -o /tmp/pgo-data/merged.profdata /tmp/pgo-data

# Step 4: 使用 profile data 重新编译
RUSTFLAGS="-Cprofile-use=/tmp/pgo-data/merged.profdata -C target-cpu=native" \
    cargo build --release --bin tachyon-server

# 预期收益: 5-15% 性能提升 (主要来自更好的分支预测和内联决策)
```

### 13.4 运行时优化清单

| 优化项 | 方法 | 预期收益 |
|--------|------|----------|
| CPU 绑核 | `core_affinity` crate | 消除 context switch 开销 |
| 大页内存 | `libhugetlbfs` / `madvise(MADV_HUGEPAGE)` | 减少 TLB miss |
| NUMA 感知 | `libnuma` / `numactl` | 消除跨 NUMA 内存访问 |
| 禁用 turbo boost | CPU governor = performance | 消除频率切换延迟 |
| 网络绕过 | io_uring / DPDK (未来) | 减少 kernel 网络栈开销 |

---

## 14. 性能目标

### 14.1 延迟目标

| 操作 | 目标 | 参考基准 | 说明 |
|------|------|----------|------|
| New Order (existing level) | < 100ns | lightning-match-engine: 46ns | 插入到已有价格级别 |
| New Order (new level) | < 500ns | — | 需要创建新价格级别 + BTreeMap 插入 |
| Cancel | < 100ns | exchange-core: ~700ns (Java) | HashMap lookup + Slab remove |
| Modify/Move | < 100ns | exchange-core: ~500ns (Java) | Cancel + re-insert |
| BBO query | < 10ns | — | 缓存直读 |

### 14.2 吞吐目标

| 指标 | 目标 | 参考基准 |
|------|------|----------|
| 单 symbol 吞吐 | > 5M ops/sec | exchange-core: 5M (Java, 旧硬件) |
| 多 symbol 总吞吐 | > 20M ops/sec | 4 cores * 5M |
| Journal 写入开销 | < 1us / entry | Aeron IPC: 0.25us |

### 14.3 端到端延迟

| 路径 | 目标 | 参考基准 |
|------|------|----------|
| FIX msg in -> execution report out | < 50us | NASDAQ: 14us |
| 内部 API (嵌入式) -> event out | < 5us | — |

### 14.4 Benchmark 方法论

```rust
/// 使用 criterion 进行统计学基准测试
///
/// 关键 benchmark 场景:
/// 1. add_order_existing_level: 插入订单到已有价格级别
/// 2. add_order_new_level: 插入订单到新价格级别
/// 3. cancel_order: 取消随机订单
/// 4. match_single_fill: 一对一撮合
/// 5. match_multi_fill: 一对多撮合 (扫多个价格级别)
/// 6. mixed_workload: 混合工作负载 (70% add, 20% cancel, 10% market order)
///
/// 每个 benchmark 都需要:
/// - 预热 (warm-up) 阶段
/// - 统计显著的迭代次数
/// - 报告 mean, median, p95, p99
/// - 火焰图 (flamegraph) 辅助分析
```

---

## 15. Crate 组织

```
tachyon/
├── Cargo.toml                        # workspace root
├── crates/
│   ├── tachyon-core/                  # 核心类型定义
│   │   ├── src/
│   │   │   ├── lib.rs
│   │   │   ├── types.rs               # Price, Quantity, Side, OrderType
│   │   │   ├── order.rs               # Order, OrderId, IncomingOrder
│   │   │   ├── event.rs               # EngineEvent, RejectReason
│   │   │   ├── command.rs             # Command (NewOrder, Cancel, Modify)
│   │   │   └── error.rs               # TachyonError, ConsensusError
│   │   └── Cargo.toml
│   │
│   ├── tachyon-book/                  # 订单簿实现
│   │   ├── src/
│   │   │   ├── lib.rs
│   │   │   ├── orderbook.rs           # OrderBook 核心结构
│   │   │   ├── price_level.rs         # PriceLevel + Slab 链表操作
│   │   │   └── trigger.rs             # TriggerBook (stop orders)
│   │   ├── benches/
│   │   │   └── orderbook_bench.rs     # criterion benchmarks
│   │   └── Cargo.toml
│   │
│   ├── tachyon-engine/                # 撮合引擎
│   │   ├── src/
│   │   │   ├── lib.rs
│   │   │   ├── matcher.rs             # match_incoming_order 撮合逻辑
│   │   │   ├── sequencer.rs           # Sequencer 定序器
│   │   │   ├── risk.rs                # RiskChecker, CircuitBreaker
│   │   │   └── engine.rs              # EngineLoop 主循环 (busy-spin)
│   │   ├── benches/
│   │   │   └── matching_bench.rs
│   │   └── Cargo.toml
│   │
│   ├── tachyon-io/                    # 通信基础设施
│   │   ├── src/
│   │   │   ├── lib.rs
│   │   │   ├── spsc.rs                # SpscRingBuffer (本文档中设计)
│   │   │   └── mpsc.rs                # MPSC 队列 (多 gateway -> sequencer)
│   │   ├── benches/
│   │   │   └── spsc_bench.rs
│   │   └── Cargo.toml
│   │
│   ├── tachyon-gateway/               # 协议网关
│   │   ├── src/
│   │   │   ├── lib.rs
│   │   │   ├── fix.rs                 # FIX 4.4 gateway (fefix)
│   │   │   ├── websocket.rs           # WebSocket 推送
│   │   │   ├── grpc.rs                # gRPC 服务 (tonic)
│   │   │   └── codec.rs              # SBE binary codec
│   │   ├── proto/
│   │   │   └── tachyon.proto
│   │   └── Cargo.toml
│   │
│   ├── tachyon-persist/               # 持久化层
│   │   ├── src/
│   │   │   ├── lib.rs
│   │   │   ├── wal.rs                 # WalWriter, WalReader
│   │   │   ├── snapshot.rs            # rkyv snapshot create/load
│   │   │   └── recovery.rs            # Recovery procedure
│   │   └── Cargo.toml
│   │
│   ├── tachyon-bench/                 # 基准测试套件
│   │   ├── src/
│   │   │   ├── lib.rs
│   │   │   ├── workload.rs            # 工作负载生成器
│   │   │   ├── latency.rs             # 延迟统计 (HDR histogram)
│   │   │   └── scenarios.rs           # 真实场景模拟
│   │   └── Cargo.toml
│   │
│   └── tachyon-server/                # 可执行文件
│       ├── src/
│       │   └── main.rs                # 启动配置、线程创建、核绑定
│       └── Cargo.toml
│
├── docs/
│   ├── PRD.md                         # 产品需求文档
│   ├── ARCHITECTURE.md                # 本文档
│   └── ROADMAP.md                     # 开发路线图
│
├── config/
│   └── default.toml                   # 默认配置
│
└── README.md
```

### 15.1 Crate 依赖关系

```
tachyon-server
    ├── tachyon-engine
    │   ├── tachyon-book
    │   │   └── tachyon-core
    │   ├── tachyon-io
    │   └── tachyon-core
    ├── tachyon-gateway
    │   ├── tachyon-io
    │   └── tachyon-core
    ├── tachyon-persist
    │   └── tachyon-core
    └── tachyon-bench (dev-dependency)
        ├── tachyon-engine
        └── tachyon-core
```

### 15.2 核心第三方依赖

| Crate | 版本 | 用途 | 所在 tachyon crate |
|-------|------|------|-------------------|
| `slab` | 0.4 | 订单/价格级别的对象池 | tachyon-book |
| `smallvec` | 1.x | 栈上小数组 (fill results) | tachyon-engine |
| `arrayvec` | 0.7 | 固定容量栈上集合 | tachyon-core |
| `mimalloc` | 0.1 | 全局分配器 | tachyon-server |
| `bumpalo` | 3.x | Arena allocator | tachyon-gateway |
| `crossbeam-utils` | 0.8 | CachePadded (如果不自研) | tachyon-io |
| `memmap2` | 0.9 | Memory-mapped WAL | tachyon-persist |
| `rkyv` | 0.8 | 零拷贝序列化 (snapshots) | tachyon-persist |
| `crc32fast` | 1.x | WAL entry checksum | tachyon-persist |
| `lz4_flex` | 0.11 | Journal segment 压缩 | tachyon-persist |
| `fefix` | 0.8 | FIX 4.4 协议 | tachyon-gateway |
| `tokio` | 1.x | 异步运行时 (gateway) | tachyon-gateway |
| `tonic` | 0.12 | gRPC 框架 | tachyon-gateway |
| `core_affinity` | 0.8 | CPU 绑核 | tachyon-server |
| `tracing` | 0.1 | 结构化日志 | all |
| `criterion` | 0.5 | 基准测试框架 | tachyon-bench |
| `hdrhistogram` | 7.x | HDR 延迟直方图 | tachyon-bench |

---

## 附录 A: 不使用的技术与理由

| 技术 | 理由 |
|------|------|
| `async` 在撮合热路径 | Future poll 机制引入不确定延迟，busy-spin 更可预测 |
| `Arc<Mutex<_>>` | 锁竞争是延迟杀手，Single Writer Principle 完全消除锁 |
| `std::collections::HashMap` | 使用 `hashbrown`（已被 std 采用）或保持默认，关键是预分配容量 |
| 浮点数 (`f64`) | IEEE 754 精度丢失在金融场景不可接受，使用 i64 定点数 |
| 动态分派 (`dyn Trait`) | 虚表查找 + 间接跳转阻止内联，热路径使用泛型静态分派 |
| `String` 在热路径 | 堆分配 + UTF-8 验证开销，使用固定大小 `[u8; N]` 或 symbol_id: u32 |
| `SeqCst` memory ordering | ARM 上的全屏障代价高，SPSC 只需 Release/Acquire |
| `tokio::sync::mpsc` | 非 wait-free，有 allocation，自研 SPSC 更可控 |

## 附录 B: 关键设计决策记录 (ADR)

### ADR-001: BTreeMap vs Vec vs 稀疏数组 用于价格级别索引

**决策**: 初期使用 `BTreeMap<Price, PriceLevelIdx>`

**理由**:
- BTreeMap 对任意价格范围都能工作（无需预知价格区间）
- O(log n) 插入/删除/查找，n 通常 < 1000 个价格级别
- 有序遍历天然支持 BBO 查询
- 后续可优化为排序 Vec（对 cache 更友好）或稀疏数组（如果价格范围已知）

**备选方案**:
- 排序 `Vec<(Price, PriceLevelIdx)>`: 更好的 cache 性能但插入是 O(n)
- 稀疏数组 `[Option<PriceLevelIdx>; MAX_TICKS]`: O(1) 但需要预知价格范围

### ADR-002: Slab + 索引链表 vs unsafe 裸指针链表

**决策**: 使用 `slab` crate + `usize` 索引

**理由**:
- 完全 safe Rust，无 `unsafe` 代码
- Slab 内部是 Vec，内存连续，cache 友好
- O(1) 插入/删除
- 索引比指针更容易序列化（snapshot 友好）

### ADR-003: 128-byte Cache Line 对齐

**决策**: 使用 `#[repr(align(128))]` 而非 64-byte

**理由**:
- Apple Silicon (M1/M2/M3) 的 L1 cache line 是 128 bytes
- x86-64 是 64 bytes
- 使用 128 保证两种架构都不会 false sharing
- 代价是每个 CachePadded 浪费约 120 bytes，可接受（只有少量计数器需要）
