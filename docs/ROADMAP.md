# Tachyon - Development Roadmap

> 详细开发路线图与里程碑规划 | Detailed Development Roadmap & Milestones

---

## 总览

整个项目分为 **5 个阶段 (Phase)**，采用递增式开发。每个阶段结束时都有一个可运行、可测试的里程碑。所有阶段都坚持 **test-first、benchmark-first** 的工程纪律——测试和基准测试不是后期补充，而是设计和实现的前置条件。

```
Phase 1          Phase 2            Phase 3          Phase 4            Phase 5
Foundation       Engine + IO        Networking       Persistence        Extreme Optimization
[核心类型+订单簿]  [撮合引擎+无锁IO]   [网络层]          [持久化+可观测性]    [极致性能调优]
 2-3 weeks        3-4 weeks          2-3 weeks        2-3 weeks          持续迭代
    │                │                  │                │                  │
    ▼                ▼                  ▼                ▼                  ▼
  types +         matching +         WebSocket +      WAL +              lock-free +
  orderbook       sequencer +        REST +           snapshots +        zero-alloc +
  + tests         lock-free IO       FIX 4.4          metrics            SIMD + PGO
  + benches       + events           + binary srv     + Docker           + OS tuning
```

---

## Phase 1: Foundation (基础层) — 预计 2-3 周

> **目标：** 核心数据类型 + 高性能订单簿 + 100% 测试覆盖 + 基准测试基线
>
> **为什么这是第一步：** 所有后续组件都构建在这些核心类型之上。类型设计的好坏直接决定了整个系统的性能天花板和正确性保证。特别是定点数算术和对象池设计——这些是撮合引擎的 DNA，必须一开始就做对。

### 1.1 项目初始化与工程基础设施

**Cargo Workspace 搭建**

创建包含 8 个 crate 的 workspace 结构。每个 crate 职责单一、边界清晰：

| Crate | 职责 | 依赖关系 |
|-------|------|----------|
| `tachyon-core` | 核心类型定义 (Price, Quantity, Order, Event...) | 无外部依赖（除 thiserror, serde） |
| `tachyon-book` | 订单簿数据结构与操作 | tachyon-core |
| `tachyon-engine` | 撮合逻辑、定序器、引擎主循环 | tachyon-core, tachyon-book, tachyon-io |
| `tachyon-io` | Lock-free SPSC/MPSC 队列 | tachyon-core |
| `tachyon-gateway` | API 网关 (WebSocket, REST, FIX) | tachyon-core, tachyon-engine |
| `tachyon-persist` | WAL + Snapshot 持久化 | tachyon-core |
| `tachyon-bench` | 基准测试套件与场景模拟 | 全部 |
| `tachyon-server` | 可执行文件入口 | 全部 |

**为什么用 workspace：** Cargo workspace 允许独立编译、独立测试每个 crate，同时共享依赖版本。更重要的是它强制了依赖方向——`tachyon-core` 不可能意外依赖 `tachyon-engine`。

**CI/CD 配置 (GitHub Actions)**

```yaml
# 每次 push 和 PR 触发以下检查：
- cargo fmt --check          # 代码格式一致性
- cargo clippy -- -D warnings # 静态分析，warnings 视为 error
- cargo test                  # 全部单元测试 + 集成测试
- cargo miri test -p tachyon-io  # 对 unsafe 代码进行未定义行为检测
- cargo bench -- --test       # 确保 benchmark 代码可编译（不运行完整 benchmark）
```

**为什么需要 Miri：** `tachyon-io` 中的 lock-free 队列包含 `unsafe` 代码（`MaybeUninit`、原子操作）。Miri 是 Rust 官方的未定义行为检测器，能在编译时发现 data race、use-after-free、uninitialized memory read 等问题。

**代码规范文件**

- `rustfmt.toml`：统一代码风格（tab width、import grouping、max width 等）
- `clippy.toml`：配置 clippy lint 级别，特别是禁止 `unwrap()` 在非测试代码中使用
- `.editorconfig`：编辑器级别的格式约定

**Criterion 基准测试框架**

从第一天起就搭建 criterion benchmark 框架。每个 crate 的 `benches/` 目录包含对应的性能测试。Criterion 使用统计学方法（Bootstrap 置信区间）来判断性能是否发生回归，比简单计时可靠得多。

```toml
# Cargo.toml (workspace root)
[workspace.dependencies]
criterion = { version = "0.5", features = ["html_reports"] }
```

**Pre-commit Hooks**

使用 `cargo-husky` 或自定义 `.githooks/pre-commit` 脚本，在 commit 前自动运行 `cargo fmt --check` 和 `cargo clippy`，防止不合规代码进入仓库。

---

### 1.2 `tachyon-core` — 核心类型

这是整个系统的类型基础。每个类型的设计都直接影响性能和正确性。

#### 1.2.1 Price(i64) — 定点数价格类型

```rust
/// 定点数价格表示
/// 例如: scale=2 时，50001.50 USDT 表示为 Price(5000150)
/// 使用 i64 而非 u64，因为价格差值（spread）可能为负数
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Price(i64);
```

**为什么用定点数而非浮点数：**
- 浮点数 `0.1 + 0.2 != 0.3`，在金融系统中不可接受
- 定点数是精确的整数运算，不存在精度损失
- `i64` 在 scale=8 时最大表示约 92,233,720,368.54775807，足够覆盖所有交易场景

**需要实现的 trait 和方法：**
- `Ord`、`Hash`、`Copy` — 用于 BTreeMap 键和 HashMap 键
- `checked_add()`、`checked_sub()` — 防止溢出
- `checked_mul_qty()` — 价格 * 数量，使用 `i128` 中间值避免溢出：`(self.0 as i128 * qty.0 as i128) / scale_factor`
- `Display` — 格式化为人类可读的小数形式
- `From<&str>` / `TryFrom<f64>` — 解析输入（仅在 gateway 层使用，不在热路径上）

**关键实现细节：**
- `scale` 作为 `SymbolConfig` 的一部分在每个交易对上配置，而非编码在 Price 类型内部
- 乘法操作使用 `u128` 中间值：两个 `i64` 相乘可能溢出 `i64`，但不会溢出 `i128`

#### 1.2.2 Quantity(u64) — 定点数数量类型

```rust
/// 无符号定点数数量
/// 数量永远 >= 0，所以用 u64
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Quantity(u64);
```

**需要实现：**
- `checked_add()`、`checked_sub()` — 部分成交时扣减数量
- `is_zero()` — 判断订单是否完全成交
- `min(self, other)` — 撮合时取较小量
- `saturating_sub()` — 安全减法，结果不低于 0

**为什么 Quantity 用 u64 而 Price 用 i64：** 数量永远是非负的（不可能卖出负数量），而价格差值计算需要负数支持。

#### 1.2.3 OrderId(u64) — 全局唯一订单标识

```rust
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct OrderId(u64);
```

由 Sequencer 分配，单调递增。64 位空间足够支撑每秒 100 万笔订单运行约 58 万年。

#### 1.2.4 Symbol — 交易对标识

```rust
/// 使用 u32 作为内部表示，节省内存并加速比较
/// 交易对名称 (如 "BTC/USDT") 与 u32 之间的映射在 SymbolRegistry 中维护
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Symbol(u32);
```

**为什么用 u32 而非 String：** 在热路径上，交易对比较是高频操作。`u32` 比较是一条 CPU 指令，`String` 比较需要遍历字节。`u32` 最多支持约 43 亿个交易对，远超任何交易所的需求。

#### 1.2.5 枚举类型

```rust
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Side {
    Buy,
    Sell,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OrderType {
    Limit,
    Market,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TimeInForce {
    GTC,       // Good-Til-Cancel: 挂单直到被取消
    IOC,       // Immediate-or-Cancel: 立即成交否则取消剩余
    FOK,       // Fill-or-Kill: 全部成交否则全部取消
    PostOnly,  // 只做 Maker，如果会立即成交则拒绝
    GTD(u64),  // Good-Til-Date: 到指定时间戳后过期
}
```

**为什么 OrderType 和 TimeInForce 分开：** IOC/FOK 不是订单类型，而是执行策略。一个 Limit Order 可以是 GTC 也可以是 IOC。这种分离使撮合逻辑更清晰：先按 OrderType 匹配价格，再按 TimeInForce 决定未成交部分的处理方式。

#### 1.2.6 Order 结构体

```rust
pub struct Order {
    pub id: OrderId,
    pub symbol: Symbol,
    pub side: Side,
    pub price: Price,
    pub quantity: Quantity,
    pub remaining_qty: Quantity,
    pub order_type: OrderType,
    pub time_in_force: TimeInForce,
    pub timestamp: u64,          // nanosecond timestamp
    // 侵入式链表指针 (slab indices)
    pub prev: u32,               // u32::MAX 表示无前驱
    pub next: u32,               // u32::MAX 表示无后继
}
```

**为什么用 slab index (u32) 而非 `Option<usize>` 或裸指针：**
- `u32` 只有 4 字节，`Option<usize>` 在 64 位系统上是 16 字节（因为 niche optimization 对 usize 不适用于 slab index 语义）
- 裸指针需要 `unsafe` 且难以保证安全性
- Slab index 是类型安全的，且对象池（slab）保证 index 有效期间指向的内存不会被移动
- 使用 `u32::MAX` 作为哨兵值（sentinel），避免 `Option` 的开销

#### 1.2.7 Trade 结构体

```rust
pub struct Trade {
    pub trade_id: u64,
    pub symbol: Symbol,
    pub price: Price,
    pub quantity: Quantity,
    pub maker_order_id: OrderId,
    pub taker_order_id: OrderId,
    pub maker_side: Side,        // Maker 的方向（Buy 或 Sell）
    pub timestamp: u64,
}
```

**为什么记录 `maker_side` 而非 `taker_side`：** 金融系统惯例。Maker 是被动方（已经在 book 中的订单），记录 maker_side 可以直接推导出 taker_side（取反），同时方便计算 maker/taker 不同的手续费。

#### 1.2.8 EngineEvent 枚举

```rust
pub enum EngineEvent {
    OrderAccepted { order_id: OrderId, symbol: Symbol, side: Side, price: Price, qty: Quantity, timestamp: u64 },
    OrderRejected { order_id: OrderId, reason: RejectReason, timestamp: u64 },
    OrderCancelled { order_id: OrderId, remaining_qty: Quantity, timestamp: u64 },
    OrderExpired { order_id: OrderId, timestamp: u64 },
    Trade { trade: Trade },
    BookUpdate { symbol: Symbol, side: Side, price: Price, new_total_qty: Quantity, timestamp: u64 },
}
```

**为什么 EngineEvent 是 Event Sourcing 的基石：** 所有状态变更都表达为事件。这意味着：
1. 可以从事件日志完整重建系统状态（crash recovery）
2. 可以验证确定性（相同事件序列 -> 相同最终状态）
3. 可以回放历史进行审计和调试

#### 1.2.9 EngineError 枚举

```rust
#[derive(thiserror::Error, Debug)]
pub enum EngineError {
    #[error("order not found: {0}")]
    OrderNotFound(OrderId),
    #[error("symbol not found: {0:?}")]
    SymbolNotFound(Symbol),
    #[error("invalid price: {0:?}")]
    InvalidPrice(Price),
    #[error("invalid quantity: {0:?}")]
    InvalidQuantity(Quantity),
    #[error("order book full")]
    BookFull,
    #[error("price out of range")]
    PriceOutOfRange,
    #[error("rate limit exceeded")]
    RateLimitExceeded,
    #[error("self trade prevented")]
    SelfTradePrevented,
}
```

使用 `thiserror` 宏自动生成 `Display` 和 `std::error::Error` 实现。生产代码中禁止 `unwrap()`，所有可失败操作返回 `Result<T, EngineError>`。

#### 1.2.10 SymbolConfig — 交易对配置

```rust
pub struct SymbolConfig {
    pub symbol: Symbol,
    pub tick_size: Price,       // 最小价格变动单位
    pub lot_size: Quantity,     // 最小数量变动单位
    pub price_scale: u8,        // 价格小数位数
    pub qty_scale: u8,          // 数量小数位数
    pub max_price: Price,       // 最大允许价格
    pub min_price: Price,       // 最小允许价格
    pub max_order_qty: Quantity, // 单笔最大数量
    pub min_order_qty: Quantity, // 单笔最小数量
}
```

**为什么需要 SymbolConfig：** 不同交易对有不同的精度和限制。BTC/USDT 的 tick_size 可能是 0.01，而一个小市值 token 可能是 0.000001。这些参数在创建交易对时配置，之后不可修改（修改需要暂停交易）。

#### 1.2.11 测试要求

- **Property-based testing (proptest)**：对所有算术操作进行模糊测试
  - `Price::checked_add()` 满足交换律和结合律
  - `Quantity::checked_sub(a, b)` 当 a < b 时返回 None
  - 任意 Price * Quantity 不会 panic
  - `Order` 的 `remaining_qty` 永远 <= `quantity`
- **100% 代码覆盖率**：核心类型是基础，必须完全覆盖
- 所有 `#[derive]` 的 trait 都需要验证（如 `Ord` 的传递性）

---

### 1.3 `tachyon-book` — 订单簿

订单簿是整个撮合引擎中性能最敏感的数据结构。每次订单操作（插入、取消、撮合）都需要操作订单簿。

#### 1.3.1 OrderPool — 基于 Slab 的对象池

```rust
use slab::Slab;

/// 使用 slab crate 实现预分配对象池
/// Slab 内部是一个 Vec<Entry<T>>，Entry 是 Occupied(T) 或 Vacant(next_free_idx)
/// 这实现了 O(1) 分配和释放，且内存连续布局
pub struct OrderPool {
    orders: Slab<Order>,
}
```

**为什么用 `slab` crate 而非自己实现：**
- `slab` 是 Tokio 团队维护的成熟 crate，无 `unsafe` 代码
- 提供 O(1) insert（从 free list 分配）和 O(1) remove（归还到 free list）
- 内部使用连续 `Vec` 存储，缓存友好
- 返回的 key 是 `usize`，可以安全地用作 index

**预分配策略：** 启动时通过 `Slab::with_capacity(initial_capacity)` 预分配内存。`initial_capacity` 根据预期最大挂单量配置（例如 100 万）。这避免了运行时的内存分配。

#### 1.3.2 PriceLevel — 价格级别

```rust
pub struct PriceLevel {
    pub price: Price,
    pub total_quantity: Quantity,  // 缓存：该价格上所有订单的总量
    pub order_count: u32,         // 缓存：该价格上的订单数
    pub head_idx: u32,            // 链表头（最早的订单，FIFO 出队点）
    pub tail_idx: u32,            // 链表尾（最新的订单，FIFO 入队点）
}
```

**为什么缓存 `total_quantity` 和 `order_count`：**
- 行情推送需要频繁查询每个价格级别的总量，遍历链表代价太高
- 每次 add/cancel/fill 时增量更新这两个值，O(1) 维护成本
- 使 `get_depth(n)` 操作可以直接读取，无需遍历

**侵入式双向链表：** 同一价格级别内的订单通过 `Order.prev` / `Order.next` 字段形成双向链表。这些字段存储的是 slab index（u32），不是指针。

- **入队（新订单）：** 追加到 `tail`，O(1)
- **出队（最早订单成交）：** 从 `head` 取出，O(1)
- **任意删除（取消订单）：** 通过 `prev`/`next` 直接 unlink，O(1)

#### 1.3.3 OrderBook — 核心订单簿结构

```rust
pub struct OrderBook {
    pub symbol: Symbol,
    pub config: SymbolConfig,

    // 价格级别索引 — BTreeMap 保证按价格排序
    bids: BTreeMap<Price, u32>,     // Price -> PriceLevel slab index (降序遍历)
    asks: BTreeMap<Price, u32>,     // Price -> PriceLevel slab index (升序遍历)

    // 对象池
    orders: Slab<Order>,            // 所有订单的存储
    levels: Slab<PriceLevel>,       // 所有价格级别的存储

    // O(1) 查找
    order_map: HashMap<OrderId, usize>,  // OrderId -> orders slab key

    // BBO 缓存
    best_bid: Option<Price>,
    best_ask: Option<Price>,
}
```

**数据结构选择的理由：**

| 数据结构 | 选择 | 理由 |
|----------|------|------|
| 价格级别索引 | `BTreeMap<Price, u32>` | 需要有序遍历（get_depth）、O(log n) 插入/删除。价格级别数通常 < 10000，BTree 的 cache locality 优于 skip list |
| 订单存储 | `Slab<Order>` | O(1) 分配/释放、连续内存、无 unsafe |
| 订单查找 | `HashMap<OrderId, usize>` | O(1) 按 ID 查找，用于取消订单。使用 `hashbrown` 替代 std HashMap 获得更好性能 |
| BBO 缓存 | `Option<Price>` | O(1) 查询最优价，避免每次从 BTreeMap 取 first/last |

#### 1.3.4 操作实现

**`add_order(order) -> Result<()>`**

```
1. 验证订单参数 (price, quantity 在合法范围内)
2. 在 orders slab 中分配 Order slot
3. 在 order_map 中注册 OrderId -> slab key
4. 查找或创建对应 price 的 PriceLevel：
   a. 如果 levels 中已有该价格 -> 获取 PriceLevel
   b. 如果没有 -> 在 levels slab 中分配新 PriceLevel，注册到 bids/asks BTreeMap
5. 将 Order 追加到 PriceLevel 链表尾部 (tail)：
   a. 设置 order.prev = level.tail_idx
   b. 设置原 tail 的 next = 新 order 的 slab index
   c. 更新 level.tail_idx
6. 更新 level.total_quantity += order.quantity
7. 更新 level.order_count += 1
8. 更新 BBO 缓存：
   a. Buy side: if price > best_bid -> best_bid = Some(price)
   b. Sell side: if price < best_ask -> best_ask = Some(price)
```

**`cancel_order(order_id) -> Result<CancelledOrder>`**

```
1. 从 order_map 查找 slab key，O(1)
2. 从 orders slab 读取 Order
3. 从对应 PriceLevel 的链表中 unlink：
   a. 如果有 prev -> prev.next = order.next
   b. 如果有 next -> next.prev = order.prev
   c. 如果是 head -> level.head_idx = order.next
   d. 如果是 tail -> level.tail_idx = order.prev
4. 更新 level.total_quantity -= order.remaining_qty
5. 更新 level.order_count -= 1
6. 如果 level.order_count == 0 -> 移除空 PriceLevel：
   a. 从 bids/asks BTreeMap 中移除 entry
   b. 从 levels slab 中释放
7. 从 order_map 移除 OrderId
8. 从 orders slab 释放 Order
9. 更新 BBO 缓存：
   a. 如果取消的订单在 best_bid 价格 且该价格级别已空 ->
      best_bid = bids.keys().next_back() (BTreeMap 最后一个 key)
   b. best_ask 同理
10. 返回 CancelledOrder { order_id, remaining_qty }
```

**为什么 BBO 缓存更新在 cancel 中更复杂：** Add 操作只需要检查新价格是否优于当前 BBO（简单比较）。但 cancel 操作可能删除 BBO 价格上的最后一个订单，需要查找新的 BBO。虽然 `BTreeMap::keys().next_back()` 是 O(log n) 操作（需要从根遍历到最右叶子），但由于我们缓存了 BBO，此查询仅在 BBO 价格级别被清空时才触发，频率很低。

**`get_best_bid() / get_best_ask() -> Option<&PriceLevel>`**

直接从 `best_bid` / `best_ask` 缓存返回，O(1)。不需要遍历 BTreeMap。

**`get_depth(n) -> (Vec<(Price, Qty)>, Vec<(Price, Qty)>)`**

```
bids: bids.iter().rev().take(n) 取降序前 N 个
asks: asks.iter().take(n) 取升序前 N 个
```

使用 BTreeMap 的有序迭代器，无需排序。

**`modify_order(order_id, new_qty, new_price) -> Result<()>`**

实现为 cancel + re-insert。这意味着修改后的订单**失去原有的时间优先级**，排到该价格级别的队尾。这是所有主流交易所的标准行为——防止通过频繁修改获得不公平的优先级。

#### 1.3.5 边界条件测试

以下场景必须有专门的测试用例：

| 场景 | 测试要点 |
|------|----------|
| 空订单簿 | `best_bid` / `best_ask` 返回 None，`get_depth(n)` 返回空 |
| 单个订单 | 插入后成为 BBO，取消后 BBO 恢复 None |
| 同价格多订单 | FIFO 顺序正确，取消中间订单后链表完整 |
| 取消不存在的订单 | 返回 `EngineError::OrderNotFound` |
| 取消价格级别最后一个订单 | 价格级别被移除，BBO 正确更新 |
| BBO 更新验证 | 每次操作后 BBO 一致性校验 |
| 大量订单 | 100 万订单插入/取消，验证无内存泄漏 |
| 价格边界 | `Price(i64::MAX)`、`Price(i64::MIN)` 的算术安全性 |

#### 1.3.6 基准测试

```rust
// benches/orderbook_bench.rs
// 使用 Criterion 框架

// 1. 插入性能
benchmark: insert 1M limit orders at random prices
target: < 200ns per insert (median)

// 2. 取消性能
benchmark: cancel 1M orders by random order_id
target: < 100ns per cancel (median)

// 3. 混合工作负载
benchmark: 随机交替 insert/cancel/query，模拟真实交易场景
distribution: 40% insert, 40% cancel, 10% modify, 10% get_depth

// 4. BBO 查询
benchmark: 连续 best_bid/best_ask 查询
target: < 10ns (应该是单次缓存读取)
```

---

### 里程碑 1 -- 验收标准

```
[x] Cargo workspace 搭建完成，8 个 crate 可独立编译
[x] GitHub Actions CI 全部通过 (test, clippy, fmt, miri)
[x] tachyon-core 所有类型实现完毕，100% 测试覆盖
[x] proptest 属性测试覆盖所有算术操作
[x] tachyon-book OrderBook 完整实现，所有操作正确
[x] 边界条件测试全部通过
[x] Criterion 基准测试：insert < 200ns, cancel < 100ns (median)
[x] 代码通过 clippy + miri 检查
[x] 零 unwrap() 在非测试代码中
```

---

## Phase 2: Matching Engine + Lock-Free IO (撮合引擎 + 无锁通信) — 预计 3-4 周

> **目标：** 完整的撮合逻辑 + Lock-Free 线程间通信 + 多交易对并发 + 确定性验证
>
> **为什么这是第二步：** Phase 1 建立了"数据层"（类型 + 订单簿），Phase 2 建立"逻辑层"（撮合 + 通信）。Lock-free IO 必须先于 Matching Engine 实现，因为 Engine 的主循环依赖 SPSC 队列来接收命令和发送事件。

### 2.1 `tachyon-io` — Lock-Free 通信

#### 2.1.1 SpscQueue<T> — Wait-Free 单生产者单消费者队列

这是整个系统中延迟最敏感的组件。Sequencer -> Matcher 和 Matcher -> Event Handler 之间的通信都通过 SPSC 队列。

```rust
pub struct SpscQueue<T> {
    buffer: Box<[MaybeUninit<T>]>,        // 预分配环形缓冲区
    capacity: usize,                       // 必须是 2 的幂
    head: CachePadded<AtomicUsize>,        // 消费者读指针 (128 字节对齐)
    tail: CachePadded<AtomicUsize>,        // 生产者写指针 (128 字节对齐)
}
```

**关键设计决策：**

**1. 容量为 2 的幂 (Power-of-2 capacity)**

```rust
// 传统取模：index % capacity — 需要除法指令（~25 cycles on x86）
// 位运算取模：index & (capacity - 1) — 一条 AND 指令（1 cycle）
let actual_index = raw_index & (self.capacity - 1);
```

在每秒数百万次操作中，这个差异是显著的。构造函数中用 `assert!(capacity.is_power_of_two())` 强制约束。

**2. CachePadded 防 false sharing (128 字节对齐)**

```rust
/// Apple Silicon (M1/M2/M3) 的 cache line 是 128 字节
/// x86-64 的 cache line 是 64 字节
/// 使用 128 字节对齐兼容两种架构
#[repr(align(128))]
pub struct CachePadded<T>(T);
```

**为什么是 128 而非 64：** Apple Silicon 使用 128 字节 cache line。如果只对齐到 64 字节，在 M1/M2 上 `head` 和 `tail` 可能位于同一个 cache line，导致 false sharing——两个核心互相刷对方的缓存，显著增加延迟。

**3. Release/Acquire 语义**

```rust
pub fn try_push(&self, value: T) -> Result<(), T> {
    let tail = self.tail.0.load(Ordering::Relaxed);  // 只有生产者写 tail
    let head = self.head.0.load(Ordering::Acquire);   // 读取消费者进度
    if tail - head >= self.capacity {
        return Err(value);  // 队列满
    }
    unsafe {
        let slot = &mut *self.buffer[tail & (self.capacity - 1)].as_mut_ptr();
        ptr::write(slot, value);
    }
    self.tail.0.store(tail + 1, Ordering::Release);   // 发布新数据
    Ok(())
}

pub fn try_pop(&self) -> Option<T> {
    let head = self.head.0.load(Ordering::Relaxed);  // 只有消费者写 head
    let tail = self.tail.0.load(Ordering::Acquire);   // 读取生产者进度
    if head >= tail {
        return None;  // 队列空
    }
    let value = unsafe {
        ptr::read(self.buffer[head & (self.capacity - 1)].as_ptr())
    };
    self.head.0.store(head + 1, Ordering::Release);   // 确认消费完成
    Some(value)
}
```

**为什么不用 SeqCst：** `SeqCst` 在 ARM (Apple Silicon) 上需要额外的 memory barrier 指令（DMB ISH），开销约 10-20ns。SPSC 场景下 `Release`/`Acquire` 提供了充分的保证：生产者 Release 写入确保数据对消费者 Acquire 读取可见。

**4. 批量弹出 (LMAX Disruptor Pattern)**

```rust
/// 一次性弹出多个元素，减少原子操作次数
/// 当消费者落后时（如突发流量），批量追赶
/// 使用 ArrayVec 避免堆分配，BATCH_CAP 应匹配典型批次大小
pub fn batch_pop<const BATCH_CAP: usize>(
    &self,
    buf: &mut ArrayVec<T, BATCH_CAP>,
) -> usize {
    let head = self.head.0.load(Ordering::Relaxed);
    let tail = self.tail.0.load(Ordering::Acquire);
    let available = tail - head;
    let count = available.min(BATCH_CAP);
    for i in 0..count {
        let value = unsafe {
            ptr::read(self.buffer[(head + i) & (self.capacity - 1)].as_ptr())
        };
        unsafe { buf.push_unchecked(value); }
    }
    self.head.0.store(head + count, Ordering::Release);  // 一次原子更新
    count
}
```

#### 2.1.2 MpscQueue<T> — Lock-Free 多生产者单消费者队列

用于 Gateway（多个异步 handler） -> Sequencer（单线程）的通信。

**实现策略：** 使用多个 SPSC 队列组合。每个 Gateway handler 线程持有一个独立的 SPSC 队列到 Sequencer。Sequencer 轮询所有 SPSC 队列。这比真正的 lock-free MPSC（如 Michael-Scott queue）更简单且性能更好，因为避免了 CAS 竞争。

```rust
pub struct MpscQueue<T> {
    queues: Vec<SpscQueue<T>>,  // 每个生产者一个 SPSC
}
```

#### 2.1.3 基准测试

```
1. 单线程吞吐量：同一线程 push + pop 交替，衡量原子操作本身的开销
   target: > 100M ops/sec

2. 跨线程吞吐量：生产者线程连续 push，消费者线程连续 pop
   target: > 50M ops/sec

3. 延迟分布：使用 rdtsc/rdtscp 精确计时
   target: median < 50ns, P99 < 200ns

4. 批量操作：batch_pop 在不同 batch size 下的吞吐量
```

---

### 2.2 `tachyon-engine` — 撮合引擎

#### 2.2.1 Matcher — 撮合器

Matcher 是纯函数式组件——给定订单簿状态和输入订单，产生确定性的输出（trades + events）。无副作用、无 I/O、无分配。

**`match_limit_order(book, order) -> SmallVec<[EngineEvent; 8]>`**

> **注意：** 此处及后续撮合函数的返回类型在伪代码中使用 `SmallVec<[EngineEvent; 8]>` 表示。
> 典型撮合产生的事件数 ≤ 8 个（1 个 Accept + 若干 Trade + 可能的 Cancel），
> `SmallVec` 在栈上内联存储，避免堆分配。超过 8 个时自动 fallback 到堆。
> 生产实现中也可使用 `bumpalo` arena 分配器替代。

```
核心逻辑：Price-Time Priority 撮合循环

1. 确定对手方 (incoming Buy -> match against asks, Sell -> bids)
2. while remaining_qty > 0:
   a. 取对手方最优价格级别 (best_ask for Buy, best_bid for Sell)
   b. 如果没有对手方 -> break (剩余部分挂单)
   c. 价格检查：
      - Buy:  if best_ask.price > order.price -> break (对手价格太高)
      - Sell: if best_bid.price < order.price -> break (对手价格太低)
   d. 从 PriceLevel 头部开始撮合 (时间优先):
      - fill_qty = min(incoming.remaining_qty, resting.remaining_qty)
      - 生成 Trade event
      - 更新双方 remaining_qty
      - 如果 resting 完全成交 -> 从链表移除 -> 生成 OrderFilled event
   e. 如果 PriceLevel 为空 -> 从 BTreeMap 移除
3. 如果 remaining_qty > 0:
   - 根据 TimeInForce 处理：
     - GTC: 插入订单簿挂单
     - IOC: 取消剩余 -> 生成 OrderCancelled event
     - FOK: 不应该走到这里（FOK 有独立处理）
     - PostOnly: 不应该走到这里（PostOnly 有独立处理）
```

**成交价格规则：** 使用 resting order（maker）的价格。这是所有 CLOB 交易所的标准规则。Taker 得到了至少与自己报价一样好或更好的价格（price improvement）。

**`match_market_order(book, order) -> SmallVec<[EngineEvent; 8]>`**

与 Limit 撮合类似，但跳过价格检查步骤（任何价格都可以成交）。Market Order 没有价格参数，它会吃掉对手方的所有流动性直到完全成交或订单簿清空。

**`handle_ioc(book, order) -> SmallVec<[EngineEvent; 8]>`**

```
1. 执行正常的 match_limit_order
2. 如果有未成交剩余 -> 立即取消（不挂单）
3. 生成 OrderCancelled event for remaining
```

**`handle_fok(book, order) -> SmallVec<[EngineEvent; 8]>`**

```
1. 预检查：遍历对手方价格级别，计算可用流动性
   available_qty = sum of all resting qty at matchable prices
2. if available_qty < order.quantity:
   -> 直接拒绝，生成 OrderRejected { reason: InsufficientLiquidity }
   -> 不修改订单簿（原子性保证）
3. if available_qty >= order.quantity:
   -> 执行正常撮合（保证全部成交）
```

**为什么 FOK 需要预检查：** FOK 的语义是"全部成交或全部取消"。如果直接开始撮合，部分成交后发现流动性不足，就需要回滚已执行的成交——这极其复杂且破坏确定性。预检查是只读操作，不修改任何状态。

**`handle_post_only(book, order) -> SmallVec<[EngineEvent; 8]>`**

```
1. 检查对手方 BBO:
   - Buy PostOnly:  if best_ask <= order.price -> REJECT
   - Sell PostOnly: if best_bid >= order.price -> REJECT
2. 如果不会 cross spread -> 正常插入订单簿作为 maker
```

**为什么需要 PostOnly：** Maker（挂单方）通常享受更低的手续费甚至返佣。Post-Only 订单保证用户只做 maker，如果订单价格会导致立即成交（taker），则拒绝而非执行。

**自成交防范 (Self-Trade Prevention, STP)**

```
在撮合循环中，每次匹配到 resting order 时:
if resting.account_id == incoming.account_id:
    根据配置策略处理:
    - CancelNewest: 取消 incoming order
    - CancelOldest: 取消 resting order
    - CancelBoth: 取消双方
    - DecrementAndCancel: 减去较小量，取消另一方
```

#### 2.2.2 Sequencer — 定序器

Sequencer 是系统的 **单点序列化层 (Single Point of Serialization)**。所有来自 Gateway 的命令都经过 Sequencer 分配全局序列号，然后路由到对应 Symbol 的撮合线程。

```rust
pub struct Sequencer {
    next_sequence: u64,                          // 单调递增计数器
    inbound: MpscQueue<InboundCommand>,          // 从 Gateway 接收
    symbol_queues: HashMap<Symbol, SpscQueue<SequencedCommand>>,  // 到各撮合线程
}

impl Sequencer {
    pub fn run(&mut self) {
        loop {
            // 从所有 Gateway handler 收集命令
            if let Some(cmd) = self.inbound.try_pop() {
                let seq = self.next_sequence;
                self.next_sequence += 1;
                let sequenced = SequencedCommand { sequence: seq, command: cmd };
                // 路由到对应 symbol 的撮合线程
                if let Some(queue) = self.symbol_queues.get(&cmd.symbol) {
                    let _ = queue.try_push(sequenced);
                }
            }
        }
    }
}
```

**为什么 Sequencer 是单线程：** 全局序列号必须严格单调递增。多线程分配需要 atomic CAS（有 contention），而单线程直接 `+= 1` 是 1 cycle 操作。Sequencer 本身不做任何计算，只做序列号分配和路由，所以单线程不是瓶颈。

**为什么需要全局序列号：** 这是 Event Sourcing 和确定性回放的基础。如果没有全局序列号，crash recovery 时无法确定事件的相对顺序。

#### 2.2.3 Engine — 引擎主体

```rust
pub struct Engine {
    symbols: HashMap<Symbol, SymbolEngine>,
    sequencer_handle: JoinHandle<()>,
}

pub struct SymbolEngine {
    symbol: Symbol,
    book: OrderBook,
    matcher: Matcher,
    inbound: SpscQueue<SequencedCommand>,   // 从 Sequencer 接收
    outbound: SpscQueue<EngineEvent>,       // 事件输出
    core_id: usize,                         // 绑定的 CPU 核
}

impl SymbolEngine {
    /// 每个交易对的主循环，运行在独立线程上
    pub fn run(&mut self) {
        // 将当前线程绑定到指定 CPU 核
        core_affinity::set_for_current(CoreId { id: self.core_id });

        let shutdown = Arc::new(AtomicBool::new(false));

        while !shutdown.load(Ordering::Relaxed) {
            // 从 SPSC 队列弹出命令
            if let Some(cmd) = self.inbound.try_pop() {
                let events = match cmd.command {
                    Command::PlaceOrder(order) => self.matcher.match_order(&mut self.book, order),
                    Command::CancelOrder(id) => self.handle_cancel(id),
                    Command::ModifyOrder(id, new_qty, new_price) => self.handle_modify(id, new_qty, new_price),
                };
                // 将事件推入输出队列
                for event in events {
                    let _ = self.outbound.try_push(event);
                }
            }
            // 可选：如果队列为空，可以 spin_loop_hint() 或 yield
        }
    }
}
```

**`spawn_symbol_thread(symbol, core_id)`**

使用 `core_affinity` crate 将线程绑定到特定 CPU 核。这确保了：
- **缓存亲和性：** 线程的数据始终在同一个 CPU 核的 L1/L2 cache 中
- **无调度抖动：** 操作系统不会将线程迁移到其他核心
- **确定性延迟：** 减少了 cache cold miss 导致的延迟毛刺

**Graceful Shutdown：** 通过 `AtomicBool` flag 通知所有线程退出。每个线程在主循环中检查 flag，收到退出信号后完成当前操作并退出。

#### 2.2.4 Risk Manager — 基础风控

在 Matcher 执行撮合之前进行前置检查：

```rust
pub struct RiskManager {
    config: RiskConfig,
}

impl RiskManager {
    /// 价格偏离检查 — 防止 fat finger 错误
    pub fn check_price_band(&self, order: &Order, book: &OrderBook) -> Result<(), EngineError> {
        if let Some(best) = book.best_bid() {
            let threshold = best.price * self.config.price_band_pct; // e.g., 5%
            if order.side == Side::Sell && order.price < best.price - threshold {
                return Err(EngineError::PriceOutOfRange);
            }
        }
        Ok(())
    }

    /// 最大订单数量检查
    pub fn check_max_qty(&self, order: &Order) -> Result<(), EngineError> { ... }

    /// 速率限制 — Token Bucket 算法
    pub fn check_rate_limit(&mut self, account_id: u64) -> Result<(), EngineError> { ... }
}
```

**为什么风控放在 Matcher 前面而非 Gateway：** Gateway 是异步多线程的，做风控需要加锁访问共享状态。而 Matcher 是单线程的（per-symbol），可以无锁访问该 symbol 的所有状态。风控检查和撮合在同一线程中顺序执行，既简单又高效。

### 2.3 确定性验证

```
确定性验证协议:
1. 录制: 正常运行，将所有 SequencedCommand 写入日志文件
2. 回放: 从日志文件读取 SequencedCommand，重新创建空 Engine，逐条执行
3. 比较: 回放产生的 EngineEvent 序列与原始运行的 EngineEvent 序列逐事件比较
4. 断言: 两个序列必须完全相同 (byte-identical after serialization)
```

**为什么确定性至关重要：**
- **Crash Recovery 正确性：** 从 WAL 回放必须产生与原始运行完全相同的状态
- **审计合规：** 监管机构可能要求证明撮合结果的可重现性
- **调试利器：** 可以在本地回放生产环境的事件日志来重现 bug

---

### 里程碑 2 -- 验收标准

```
[x] SpscQueue 实现并通过 Miri 检查 (无 data race, 无 UB)
[x] MpscQueue 实现并通过并发正确性测试
[x] Matcher 支持所有 P0 订单类型: Limit, Market, IOC, FOK, PostOnly
[x] 自成交防范 (STP) 实现并测试
[x] Sequencer 正确分配序列号并路由
[x] Engine 支持多交易对并发运行，每交易对独立线程
[x] CPU Core Pinning 工作正常
[x] 确定性验证通过: 录制 -> 回放 -> 事件完全一致
[x] 性能: > 2M ops/sec 单交易对
[x] 延迟: < 500ns median 撮合延迟
[x] 集成测试: 多交易对、多订单类型、并发场景
```

---

## Phase 3: Networking (网络层) — 预计 2-3 周

> **目标：** WebSocket + REST + FIX 4.4 + 可部署的二进制文件
>
> **为什么这是第三步：** Phase 1-2 建立了完整的内核。Phase 3 给内核加上"外壳"——让外部世界可以与之交互。网络层运行在异步 runtime (Tokio) 上，完全隔离于撮合热路径。

### 3.1 WebSocket 服务器

**技术栈：** `tokio` + `tokio-tungstenite`

#### 频道设计

```
WebSocket Channels:
├── Public (无需认证):
│   ├── ticker@{symbol}        — 最新成交价、24h 量
│   ├── depth@{symbol}         — L2 订单簿深度 (top N levels)
│   ├── trades@{symbol}        — 实时成交流
│   └── book_snapshot@{symbol} — 完整订单簿快照 (首次连接)
│
└── Private (需认证):
    ├── orders@{user_id}       — 用户订单状态变更
    └── fills@{user_id}        — 用户成交通知
```

#### 消息格式

**二进制格式 (Primary)：** 自定义 SBE-like (Simple Binary Encoding) 格式
```
[message_type: u8][length: u16][payload: bytes]
```

- 零 copy 解析：直接 reinterpret_cast 到结构体（需要注意字节序和对齐）
- 最小网络开销：一个 Trade 消息约 48 字节，而 JSON 约 200 字节

**JSON 格式 (Fallback)：** 用于开发调试和不需要极致性能的客户端

```json
{"ch": "trades@BTCUSDT", "data": {"price": "50001.50", "qty": "0.5", "side": "buy", "ts": 1234567890}}
```

#### 背压处理 (Backpressure)

慢消费者是 WebSocket 推送的经典问题。策略：

```
每个连接维护一个有界 outbound buffer:
- 如果 buffer 满 -> 丢弃最旧的 market data message (行情数据可以被覆盖)
- 对于 private channel (orders, fills) -> 不丢弃，等待或断开连接
- 行情数据使用 "conflation" 策略: 同一 symbol 的 depth update 只保留最新一条
```

**为什么对行情数据可以丢弃而不丢：** 行情数据是"最新状态"的快照，旧的已经没有价值。而 order/fill 通知是"事件"，每一条都有业务意义，不可丢失。

#### 连接管理

- **Heartbeat：** 每 30 秒 ping/pong，超时 90 秒断开
- **重连策略：** 客户端负责重连，服务端在重连后发送 book snapshot 帮助客户端重建状态
- **连接数限制：** 每 IP 最大 N 个连接，防止资源耗尽

### 3.2 REST API (管理接口)

**框架选择：** `axum`（构建在 `tokio` + `hyper` + `tower` 之上，与 WebSocket 共享 runtime）

| Method | Endpoint | 说明 | 响应 |
|--------|----------|------|------|
| GET | `/api/v1/orderbook/{symbol}` | 订单簿快照 | 买卖各 N 档 (price, qty) |
| GET | `/api/v1/trades/{symbol}` | 最近 N 笔成交 | Trade 数组 |
| POST | `/api/v1/order` | 下单 | OrderId + 初始状态 |
| DELETE | `/api/v1/order/{id}` | 撤单 | 成功/失败 |
| GET | `/api/v1/symbols` | 交易对列表 | SymbolConfig 数组 |
| POST | `/api/v1/admin/symbol` | 管理交易对 | 添加/删除/暂停交易对 |

**POST /api/v1/order 请求体：**
```json
{
  "symbol": "BTCUSDT",
  "side": "buy",
  "type": "limit",
  "time_in_force": "GTC",
  "price": "50000.00",
  "quantity": "1.5"
}
```

**为什么用 axum 而非 actix-web：** axum 是 Tokio 官方团队的项目，与 Tokio 生态深度集成。它使用 `tower::Service` trait，这意味着可以复用 tower 生态中的所有 middleware（rate limiting、tracing、timeout 等）。

### 3.3 FIX 4.4 Gateway (P1)

**框架选择：** `fefix` crate — 目前唯一的 Rust FIX 协议实现

#### 支持的消息类型

| FIX MsgType | Tag 35 值 | 方向 | 用途 |
|-------------|-----------|------|------|
| NewOrderSingle | D | Client -> Engine | 新订单 |
| ExecutionReport | 8 | Engine -> Client | 订单确认/成交/拒绝 |
| OrderCancelRequest | F | Client -> Engine | 撤单请求 |
| OrderCancelReject | 9 | Engine -> Client | 撤单拒绝 |
| Logon | A | 双向 | 会话建立 |
| Logout | 5 | 双向 | 会话断开 |
| Heartbeat | 0 | 双向 | 心跳保活 |
| TestRequest | 1 | 双向 | 心跳探测 |

#### Session 管理

```
FIX Session 状态机:
  DISCONNECTED -> LOGON_SENT -> ACTIVE -> LOGOUT_SENT -> DISCONNECTED

关键实现:
- 序列号管理: MsgSeqNum (tag 34) 必须严格递增
- Gap fill: 如果检测到 sequence gap，发送 ResendRequest
- 断线重连: 从上次 MsgSeqNum 继续，请求 gap fill
```

**为什么支持 FIX 4.4：** FIX 是金融行业的标准通信协议，所有专业做市商和经纪商的交易系统都通过 FIX 连接。支持 FIX 4.4 意味着 Tachyon 可以直接接入专业交易基础设施。

### 3.4 Binary Server — `tachyon-server`

#### 配置文件 (TOML)

```toml
[server]
bind_address = "0.0.0.0"
websocket_port = 8080
rest_port = 8081
fix_port = 9876

[engine]
sequencer_core = 0          # Sequencer 绑定的 CPU 核
matching_cores = [1, 2, 3, 4]  # 撮合线程可用的 CPU 核

[symbols]
  [symbols.BTCUSDT]
  tick_size = "0.01"
  lot_size = "0.001"
  price_scale = 2
  qty_scale = 3
  max_price = "1000000.00"
  min_price = "0.01"
  max_order_qty = "1000.000"

[persistence]
wal_dir = "/data/tachyon/wal"
snapshot_dir = "/data/tachyon/snapshots"
fsync_strategy = "batched"    # sync | batched | async
fsync_batch_size = 1000       # batched 模式下每 N 个事件 fsync 一次

[logging]
level = "info"
format = "json"               # json | pretty
```

#### 信号处理与优雅关停

```
启动流程:
1. 解析配置文件
2. 初始化 SymbolRegistry
3. 启动 Sequencer 线程
4. 为每个 Symbol 启动撮合线程 (pinned to core)
5. 启动 WebSocket server
6. 启动 REST server
7. 启动 FIX gateway
8. 就绪 (log "Tachyon engine ready")

关停流程 (SIGTERM / SIGINT):
1. 停止接受新连接
2. 通知所有 Gateway handler 停止接收新请求
3. 等待所有 in-flight 请求处理完毕 (最多 N 秒)
4. 设置 shutdown flag -> 撮合线程退出主循环
5. 创建最终 snapshot (保存当前状态)
6. 关闭 WAL 文件
7. 退出
```

---

### 里程碑 3 -- 验收标准

```
[x] WebSocket 服务器运行，支持 ticker/depth/trades 频道
[x] 二进制 + JSON 消息格式可切换
[x] 背压处理：慢消费者不影响其他连接
[x] REST API 全部端点实现并测试
[x] FIX 4.4 基础会话管理 (Logon/Logout/Heartbeat)
[x] FIX NewOrderSingle 和 ExecutionReport 正确工作
[x] tachyon-server 可执行文件可从 TOML 配置启动
[x] 优雅关停：SIGTERM 后正确保存状态
[x] 端到端延迟 (含网络): < 100us
[x] 集成测试：通过 WebSocket/REST 下单 -> 撮合 -> 收到成交通知
```

---

## Phase 4: Persistence + Observability (持久化 + 可观测性) — 预计 2-3 周

> **目标：** Crash Recovery + Prometheus 监控 + Docker 部署
>
> **为什么这是第四步：** 前三个阶段实现了"能用"的系统，Phase 4 让它"能扛"——崩溃后能恢复、出问题能诊断、部署能自动化。

### 4.1 WAL (Write-Ahead Log)

所有 EngineEvent 在产生后**先写入 WAL，再返回给客户端**。这保证了 durability——即使进程崩溃，已确认的事件也不会丢失。

#### 日志格式

```
WAL Entry 二进制格式:
┌─────────┬──────────┬─────────────────────┬─────────┐
│ length  │ sequence │      payload        │  crc32  │
│  u32    │   u64    │  serialized bytes   │  u32    │
│ 4 bytes │  8 bytes │  variable length    │ 4 bytes │
└─────────┴──────────┴─────────────────────┴─────────┘
```

- `length`：整个 entry 的总字节数（用于快速跳过和前向扫描）
- `sequence`：全局序列号（来自 Sequencer）
- `payload`：序列化后的 EngineEvent（使用 `bincode` 或 `rkyv`）
- `crc32`：CRC32 校验（使用 `crc32fast` crate），检测数据损坏

#### 写入策略

```rust
pub enum FsyncStrategy {
    /// 每次写入后立即 fsync — 最强持久性保证，但性能最差
    /// 延迟: 约 200us (SSD), 约 5ms (HDD)
    Sync,

    /// 每 N 个事件或 T 毫秒 fsync 一次 — 平衡性能与持久性
    /// 崩溃时最多丢失一个 batch 的数据
    Batched { count: usize, interval: Duration },

    /// 不主动 fsync，依赖 OS page cache flush — 最高性能，但崩溃可能丢数据
    /// 适用于有 UPS 或不需要严格持久性的场景
    Async,
}
```

#### Memory-Mapped 写入

使用 `mmap` 将 WAL 文件映射到内存空间，写入操作变为内存拷贝（`memcpy`），然后按策略调用 `msync`/`fsync`。

```
优势:
- 避免 write() 系统调用开销 (每次 ~1us)
- 内核自动管理 page cache 和 dirty page writeback
- 可以使用 huge pages 进一步优化

实现细节:
- 预分配固定大小文件 (e.g., 256 MB)
- 维护 write_offset 指针
- 当接近文件末尾时触发日志轮转
```

#### 日志轮转

```
轮转策略:
1. 当前 WAL 文件大小 > max_wal_size (default: 256 MB) -> 创建新文件
2. 文件命名: wal_{first_sequence}_{last_sequence}.log
3. 旧文件压缩: 使用 lz4_flex 进行 LZ4 压缩 (压缩率约 2-4x，压缩速度 > 1 GB/s)
4. 保留策略: 保留最近 N 个文件或最近 T 天的文件
```

**为什么选 LZ4 而非 zstd/gzip：** 在持久化场景中，压缩速度比压缩率更重要。LZ4 压缩速度约 2-4 GB/s，是 zstd 的 3-5 倍。WAL 文件包含大量重复的结构（相同的 symbol、相近的 price），即使用 LZ4 也能获得不错的压缩率。

### 4.2 Snapshots — 快照

#### 技术选择: rkyv

```rust
use rkyv::{Archive, Deserialize, Serialize};

#[derive(Archive, Serialize, Deserialize)]
pub struct Snapshot {
    pub sequence: u64,              // 快照对应的最后一个序列号
    pub timestamp: u64,
    pub order_books: Vec<OrderBookSnapshot>,  // 所有交易对的订单簿
    pub checksum: u64,              // 状态哈希
}
```

**为什么用 rkyv 而非 serde + bincode：**
- `rkyv` 实现 zero-copy deserialization：反序列化时不需要分配内存和拷贝数据
- 直接在 mmap'd 文件上操作 archived 数据
- 恢复 1M 个订单的快照：rkyv 约 1ms，bincode 约 50ms
- 代价是序列化后的格式不跨平台（但 Tachyon 是 Linux 部署，不需要跨平台）

#### 快照策略

```
触发条件 (满足任一):
1. 每 N 个事件 (default: 100,000)
2. 每 T 分钟 (default: 5 分钟)
3. 手动触发 (admin API)

流程:
1. 序列化当前所有 OrderBook 状态
2. 计算状态哈希 (用于后续验证)
3. 写入快照文件: snapshot_{sequence}.bin
4. 记录快照元数据 (sequence, timestamp, hash)
5. 清理旧快照 (保留最近 N 个)

验证:
- 从快照恢复后重新计算状态哈希
- 哈希必须与快照中记录的一致
- 不一致则报错并尝试上一个快照
```

### 4.3 Recovery — 崩溃恢复

```
恢复流程:
1. 查找最新的有效快照文件
2. 反序列化快照 (rkyv zero-copy, ~1ms for 1M orders)
3. 验证快照哈希
4. 查找快照序列号之后的 WAL 文件
5. 顺序回放 WAL 事件:
   a. 读取 entry
   b. 验证 CRC32
   c. 检查 sequence 连续性 (无 gap)
   d. 将事件应用到 OrderBook
6. 回放完毕 -> 恢复到崩溃前的精确状态
7. 开始接受新订单

错误处理:
- CRC32 校验失败 -> 截断到最后一个有效 entry
- Sequence gap -> 报告警告，可能数据丢失
- 快照损坏 -> 尝试上一个快照
- 所有快照损坏 -> 从最早的 WAL 完整回放 (慢但可靠)
```

**恢复速度基准测试目标：**
- 快照恢复 (1M orders): < 5ms
- WAL 回放 (100K events): < 100ms
- 总恢复时间: < 1 秒

### 4.4 Observability — 可观测性

#### Prometheus 指标

```rust
// 使用 prometheus crate 注册指标

// 撮合延迟直方图 — 这是最重要的指标
let match_latency = Histogram::with_opts(
    HistogramOpts::new("tachyon_match_latency_ns", "Matching latency in nanoseconds")
        .buckets(vec![100.0, 500.0, 1000.0, 5000.0, 10000.0, 50000.0, 100000.0])
)?;
// buckets: 100ns, 500ns, 1us, 5us, 10us, 50us, 100us
// 这些桶对应了从最优到可接受的延迟范围

// 吞吐量计数器
let orders_total = IntCounter::new("tachyon_orders_total", "Total orders processed")?;
let trades_total = IntCounter::new("tachyon_trades_total", "Total trades executed")?;

// 订单簿深度仪表盘
let book_depth = IntGaugeVec::new(
    Opts::new("tachyon_book_depth", "Order book depth"),
    &["symbol", "side"]  // labels: symbol=BTCUSDT, side=bid/ask
)?;

// 队列使用率
let queue_utilization = GaugeVec::new(
    Opts::new("tachyon_queue_utilization", "Ring buffer utilization ratio"),
    &["queue_name"]
)?;

// 活跃订单数
let active_orders = IntGaugeVec::new(
    Opts::new("tachyon_active_orders", "Number of active orders"),
    &["symbol"]
)?;

// 内存使用
let memory_usage = IntGauge::new("tachyon_memory_bytes", "Process memory usage")?;
```

#### 结构化日志

```rust
use tracing::{info, warn, instrument};
use tracing_subscriber::{fmt, EnvFilter};

// 初始化
tracing_subscriber::fmt()
    .json()                    // JSON 格式（方便 ELK/Loki 采集）
    .with_env_filter(EnvFilter::from_default_env())
    .with_target(true)         // 包含模块路径
    .with_thread_ids(true)     // 包含线程 ID
    .init();

// 使用示例
#[instrument(skip(book), fields(symbol = %order.symbol))]
fn match_order(book: &mut OrderBook, order: Order) -> SmallVec<[EngineEvent; 8]> {
    info!(order_id = %order.id, side = ?order.side, price = %order.price, "matching order");
    // ...
}
```

**为什么用 tracing 而非 log：** `tracing` 支持结构化字段（key=value pairs）、span（嵌套的执行上下文）和 `#[instrument]` 宏（自动记录函数入参）。`log` 只支持非结构化的字符串消息。

#### HTTP 端点

| 端点 | 用途 |
|------|------|
| `GET /metrics` | Prometheus scrape endpoint |
| `GET /health` | 健康检查 (返回 200 + 状态 JSON) |
| `GET /health/ready` | 就绪检查 (recovery 完成后才返回 200) |

### 4.5 Deployment — 部署

#### Dockerfile

```dockerfile
# Multi-stage build
FROM rust:latest AS builder
WORKDIR /build
COPY . .
RUN cargo build --release

# Minimal runtime image
FROM scratch
COPY --from=builder /build/target/release/tachyon-server /tachyon-server
COPY --from=builder /build/config/default.toml /config/default.toml
ENTRYPOINT ["/tachyon-server"]
```

**为什么 `FROM scratch`：** 最小化攻击面和镜像体积。Rust 编译产生的是静态链接的二进制文件（需要 `target = x86_64-unknown-linux-musl`），不依赖任何动态库，可以在空镜像中运行。

#### docker-compose

```yaml
services:
  tachyon:
    build: .
    ports:
      - "8080:8080"   # WebSocket
      - "8081:8081"   # REST
      - "9876:9876"   # FIX
    volumes:
      - ./data:/data
      - ./config:/config

  prometheus:
    image: prom/prometheus
    volumes:
      - ./monitoring/prometheus.yml:/etc/prometheus/prometheus.yml
    ports:
      - "9090:9090"

  grafana:
    image: grafana/grafana
    volumes:
      - ./monitoring/grafana/dashboards:/var/lib/grafana/dashboards
      - ./monitoring/grafana/provisioning:/etc/grafana/provisioning
    ports:
      - "3000:3000"
```

#### Grafana Dashboard

提供预置的 Grafana dashboard JSON，包含以下面板：

- **延迟分布：** P50 / P95 / P99 / P99.9 时间序列
- **吞吐量：** orders/sec 和 trades/sec
- **订单簿深度：** 每个 symbol 的 bid/ask 深度
- **队列健康：** 各 ring buffer 的利用率
- **系统资源：** CPU / 内存 / 磁盘 I/O

---

### 里程碑 4 -- 验收标准

```
[x] WAL 写入工作正常，三种 fsync 策略可配置
[x] 日志轮转 + LZ4 压缩
[x] rkyv 快照创建和恢复
[x] 崩溃恢复: kill -9 后重启，状态完整恢复
[x] 恢复时间 < 1 秒 (1M orders 快照 + 100K WAL events)
[x] CRC32 校验检测损坏的 WAL entry
[x] Prometheus 指标: 延迟直方图、吞吐量、深度、队列利用率
[x] 结构化日志 (JSON 格式)
[x] /metrics + /health endpoint
[x] Docker 镜像构建成功
[x] docker-compose 一键启动 (Tachyon + Prometheus + Grafana)
[x] Grafana dashboard 可视化
```

---

## Phase 5: Extreme Optimization (极致优化) — 持续迭代

> **目标：** 压榨到硬件极限——亚微秒级延迟，零分配热路径
>
> **为什么这是最后一步：** 优化的第一条铁律是"先让它工作，再让它快"。Phase 1-4 建立了一个正确的、可测量的、可监控的系统。Phase 5 在这个基础上，用数据驱动的方式进行极致优化。**永远不要在没有 profiler 数据的情况下优化。**

### 5.1 Zero-Allocation 审计

**方法：** 使用 counting allocator 在 benchmark 中验证零分配

```rust
#[cfg(test)]
mod tests {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CountingAllocator {
        alloc_count: AtomicUsize,
    }

    unsafe impl GlobalAlloc for CountingAllocator {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            self.alloc_count.fetch_add(1, Ordering::Relaxed);
            unsafe { System.alloc(layout) }
        }
        // ...
    }

    #[global_allocator]
    static ALLOC: CountingAllocator = CountingAllocator {
        alloc_count: AtomicUsize::new(0),
    };

    #[test]
    fn matching_hot_path_zero_alloc() {
        // 预热 (让预分配完成)
        setup_book_with_orders();

        // 重置计数
        ALLOC.alloc_count.store(0, Ordering::SeqCst);

        // 执行撮合
        match_limit_order(&mut book, order);

        // 验证零分配
        assert_eq!(ALLOC.alloc_count.load(Ordering::SeqCst), 0);
    }
}
```

**常见的隐藏分配来源：**
- `Vec::push()` 触发 realloc — 解决：预分配 + 固定容量
- `String` 格式化 — 解决：使用 `ArrayString` 或栈上缓冲区
- `Box::new()` — 解决：使用对象池
- `HashMap` resize — 解决：`with_capacity()` 预分配
- 隐式 `Clone` — 解决：使用引用或 `Copy` 类型

**自定义 Arena Allocator**

使用 `bumpalo` crate 为每次撮合请求创建临时 arena：

```rust
use bumpalo::Bump;

fn match_order(arena: &Bump, book: &mut OrderBook, order: Order) -> &[EngineEvent] {
    // 所有临时分配都在 arena 中
    let events = bumpalo::vec![in arena];
    // ...
    // arena 在请求结束时整体释放 (一次 dealloc)
}
```

### 5.2 高级 Lock-Free 优化

**自定义 SPSC 替代方案**

如果 benchmark 数据显示 SPSC 是瓶颈（unlikely，但需要验证），考虑：

```
优化方向:
1. 使用 `core::hint::spin_loop()` 替代 yield
2. Busy-spinning consumer: 消费者持续轮询，不让出 CPU
   - 优点: 最低延迟 (~20ns)
   - 缺点: 消耗 100% CPU
   - 适用: 延迟优先的生产环境，有专用核心
3. 自适应策略: 先 spin N 次，然后 yield，最后 park
```

**Benchmark 目标：**
```
Median latency:  < 100ns
P99 latency:     < 1us
P99.9 latency:   < 5us
Throughput:      > 100M ops/sec (single-threaded)
```

### 5.3 编译器优化

#### PGO (Profile-Guided Optimization) Pipeline

```bash
# 第一步: 用 instrumentation 构建
RUSTFLAGS="-Cprofile-generate=/tmp/pgo-data" cargo build --release

# 第二步: 运行真实工作负载（不是 unit test，而是 benchmark scenario）
./target/release/tachyon-bench --scenario realistic --duration 60s

# 第三步: 合并 profile 数据
llvm-profdata merge -o /tmp/pgo-data/merged.profdata /tmp/pgo-data/*.profraw

# 第四步: 用 profile 数据重新构建
RUSTFLAGS="-Cprofile-use=/tmp/pgo-data/merged.profdata" cargo build --release
```

**预期提升：** PGO 通常带来 10-15% 的性能提升。主要优化：
- 更准确的分支预测 hint (`likely`/`unlikely`)
- 更激进的 hot function inlining
- 更优的代码布局（hot path 聚集）

#### LTO + PGO 组合

```toml
# Cargo.toml
[profile.release]
lto = "fat"              # 跨 crate 优化
codegen-units = 1        # 单代码生成单元（更多优化机会）
panic = "abort"          # 不生成 unwind 代码
target-cpu = "native"    # 使用当前 CPU 的全部指令集
```

**LTO + PGO 组合预期：** 15-25% 总体性能提升。

#### SIMD 批量价格比较

```rust
#[cfg(target_arch = "x86_64")]
use std::arch::x86_64::*;

/// 使用 AVX2 同时比较 4 个 i64 价格
/// 在扫描深度订单簿时有用（例如 FOK 预检查、get_depth）
unsafe fn compare_prices_avx2(prices: &[i64; 4], target: i64) -> [bool; 4] {
    let target_vec = _mm256_set1_epi64x(target);
    let prices_vec = _mm256_loadu_si256(prices.as_ptr() as *const __m256i);
    let cmp_result = _mm256_cmpgt_epi64(prices_vec, target_vec);
    // 解析比较结果...
}
```

**适用场景：** 当需要扫描多个价格级别时（如 FOK 的流动性预检查），SIMD 可以将 4 次价格比较压缩为 1 次 CPU 指令。但只有在价格级别数量较多时才有意义。

### 5.4 操作系统级优化

#### Huge Pages

```
配置步骤:
1. echo 1024 > /proc/sys/vm/nr_hugepages     # 预分配 1024 个 2MB 大页
2. 验证: cat /proc/meminfo | grep Huge
3. 在 Tachyon 中使用 mmap + MAP_HUGETLB 分配订单池内存

为什么:
- 正常 4KB 页面 -> TLB (Translation Lookaside Buffer) 只能覆盖 ~32MB 内存
- 2MB 大页 -> TLB 能覆盖 ~16GB 内存
- 订单簿频繁随机访问，TLB miss 每次 ~20ns，大页显著减少 miss
```

#### isolcpus 核心隔离

```
# /etc/default/grub
GRUB_CMDLINE_LINUX="isolcpus=2,3,4,5 nohz_full=2,3,4,5 rcu_nocbs=2,3,4,5"

解释:
- isolcpus=2,3,4,5   : 将核心 2-5 从 OS 调度器中隔离
- nohz_full=2,3,4,5  : 关闭这些核心上的定时器中断 (减少 jitter)
- rcu_nocbs=2,3,4,5  : 将 RCU 回调移到其他核心 (减少中断)

效果:
- 隔离的核心只运行 Tachyon 的撮合线程
- 不被 OS 调度其他任务抢占
- P99.9 延迟显著降低 (消除了调度 jitter)
```

#### 实时调度 (SCHED_FIFO)

```rust
use libc::{sched_param, sched_setscheduler, SCHED_FIFO};

/// 将当前线程设置为实时优先级
unsafe fn set_realtime_priority(priority: i32) {
    let param = sched_param { sched_priority: priority };
    sched_setscheduler(0, SCHED_FIFO, &param);
}
```

**警告：** SCHED_FIFO 线程不会被普通进程抢占。如果撮合线程进入死循环，会导致系统"假死"。必须配合 `watchdog` 使用。

#### 网络内核旁路讨论

```
io_uring (用于磁盘 I/O):
- Linux 5.1+ 提供的高性能异步 I/O 接口
- 避免 read()/write() 系统调用开销
- 适用于 WAL 写入和 Snapshot I/O
- 使用 tokio-uring crate

DPDK (用于网络 I/O):
- 完全绕过内核网络栈
- 用户态直接操作网卡
- 延迟从 ~10us 降到 ~1us
- 复杂度极高，建议仅在延迟要求 < 5us 的场景考虑
- 目前作为 P3 优先级，记录方案但不实现
```

### 5.5 高级订单类型

#### Iceberg Order (冰山单)

```rust
pub struct IcebergOrder {
    pub id: OrderId,
    pub total_quantity: Quantity,      // 总量 (外部不可见)
    pub display_quantity: Quantity,    // 显示量 (book 中可见)
    pub remaining_hidden: Quantity,   // 剩余隐藏量
}
```

**行为：** 只有 `display_quantity` 出现在订单簿中。当 display 部分完全成交后，自动从 hidden reserve 补充，并排到该价格级别的队尾（失去之前的时间优先级）。

#### Trailing Stop (追踪止损)

```rust
pub struct TrailingStop {
    pub id: OrderId,
    pub side: Side,
    pub trail_offset: Price,          // 与市场价的偏移量
    pub trigger_price: Price,         // 当前触发价 (随市场价移动)
    pub order_type: OrderType,        // 触发后挂 Limit 还是 Market
}
```

**行为：** Buy Trailing Stop 的触发价随市场价下跌而下移，但不随上涨而上移。当市场价从最低点反弹 `trail_offset` 时触发。Sell 方向相反。

#### TWAP (时间加权平均价格)

```
将大单拆分为小单，在固定时间间隔内分批执行:
1. 总量 Q，执行时间 T，间隔 30 秒
2. 每次子订单量 = Q / (T / 30)
3. 每个子订单以 Market Order 执行
4. 目标: 最终平均成交价接近 TWAP 基准

实现: Tachyon 内部维护 TWAP scheduler，定时向引擎提交子订单
```

#### Scaled Order (梯度单)

```
在价格区间内均匀分布 N 个订单:
- 价格范围: [price_low, price_high]
- 数量分布: 均匀 / 线性递增 / 指数递增
- 生成 N 个独立的 Limit Order

示例: 在 49000-50000 之间均匀放 10 个 buy limit order
每个价格: 49000, 49111.11, 49222.22, ..., 50000
```

---

### 里程碑 5 -- 验收标准

```
[x] 热路径零分配验证 (counting allocator test 通过)
[x] PGO + LTO pipeline 文档化并可一键执行
[x] 延迟: median < 100ns, P99 < 10us, P99.9 < 50us
[x] 吞吐量: > 5M ops/sec (单交易对)
[x] Huge pages 配置文档
[x] isolcpus 配置文档
[x] 至少 1 个高级订单类型 (Iceberg) 实现并测试
[x] 完整的性能分析报告 (各优化手段的量化收益)
```

---

## 基准测试方法论 (Benchmark Methodology)

> 正确的基准测试比正确的实现更重要——你无法优化你不能测量的东西。

### 延迟记录

使用 **HDR Histogram (High Dynamic Range Histogram)** 记录延迟分布：

```
为什么不用简单的 average/min/max:
- Average 隐藏了分布特征 (bimodal 分布被 average 掩盖)
- Min/Max 是极端值，不代表典型行为
- HDR Histogram 记录完整分布: P50, P90, P95, P99, P99.9, P99.99

为什么不用 Prometheus Histogram:
- Prometheus Histogram 的桶是固定的，精度有限
- HDR Histogram 使用对数压缩，在 1us-10ms 范围内都有精确记录
- 适合事后分析，不适合实时推送 (用 Prometheus 做实时，用 HDR 做详细分析)
```

### 真实工作负载分布

基于真实交易所的统计数据设计工作负载：

```
操作类型分布:
├── 82% Modify Order     (改价/改量是最频繁的操作)
├──  9% New GTC Order    (新挂单)
├──  3% New IOC Order    (立即成交)
└──  6% Cancel Order     (主动撤单)

价格模型:
- Markov Chain 随机游走: 下一个价格 = 当前价格 + delta
- Delta 服从 Pareto 分布: 大部分变动很小，偶尔有大变动
- 模拟真实市场的微观结构 (spread, depth, volatility)
```

### 测试场景

| 场景 | 描述 | 目的 |
|------|------|------|
| Empty Book | 空订单簿上的操作 | 基线延迟 |
| Shallow Book | 100 个订单的订单簿 | 典型流动性下的延迟 |
| Deep Book | 100,000 个订单的订单簿 | 高流动性下的性能退化 |
| Burst Test | 正常 10x 速率持续 1 秒 | 突发流量承受能力 |
| Cascade/Avalanche | 激进订单扫过 10+ 价格级别 | 最坏情况撮合延迟 |
| Sustained Load | 持续最大吞吐量运行 10 分钟 | 稳定性和内存泄漏检测 |

### 退化曲线

```
测量不同 book depth 下的操作延迟，绘制退化曲线:
X 轴: book depth (100, 1K, 10K, 100K, 1M orders)
Y 轴: 操作延迟 (insert, cancel, match)

理想曲线:
- insert: O(log N) 退化 (BTreeMap lookup)
- cancel: O(1) 无退化 (HashMap + slab)
- match: 取决于匹配深度，非 book depth

如果退化曲线异常 -> 说明有性能 bug 需要修复
```

### 基准对比

使用 [exchange-core](https://github.com/exchange-core/exchange-core) (Java) 作为参考基准：

```
exchange-core 公开的性能数据:
- 5.4M operations/sec (throughput)
- ~1us median latency

Tachyon 目标:
- > 2M ops/sec (Phase 2), > 5M ops/sec (Phase 5)
- < 500ns median (Phase 2), < 100ns (Phase 5)

Tachyon 的优势:
- Rust vs Java: 无 GC pause（Java P99 受 GC 影响严重）
- 定制化更强: 自定义 allocator + lock-free + core pinning
- 更低的基础开销: 无 JVM startup, 无 JIT compilation
```

---

## 开发原则

### 1. Test-First (测试先行)

```
每个模块的开发流程:
1. 先写接口定义 (trait / function signature)
2. 写测试 (unit test + property test)
3. 实现代码让测试通过
4. 写基准测试
5. 优化直到满足性能目标

关键测试类型:
- Unit Test: 每个函数的正常路径和边界条件
- Property Test (proptest): 生成随机输入验证不变量
- Integration Test: 跨组件交互
- Miri Test: 检测 unsafe 代码的未定义行为
- Determinism Test: 录制 -> 回放 -> 比较
```

### 2. Benchmark-First (基准测试先行)

```
Criterion 基准测试是一等公民:
- 每个 PR 必须包含受影响路径的 benchmark 结果
- CI 中运行 benchmark 检测性能退化 (criterion 的统计学方法)
- 新功能必须有对应的 benchmark (不只是测试)
```

### 3. Measure-Driven Optimization (测量驱动优化)

```
优化流程:
1. 确认有性能问题 (benchmark 数据说话)
2. 使用 profiler 定位瓶颈 (perf, flamegraph, cachegrind)
3. 形成假设
4. 实现优化
5. 用 benchmark 验证效果
6. 如果没有显著提升 -> revert

绝对禁止:
- "我觉得这里应该更快" -> 先 profile
- "这个算法理论上更优" -> 先 benchmark
- "减少了一次分配" -> 先验证是否在 hot path 上
```

### 4. Minimal Unsafe (最小化 unsafe)

```
规则:
- 每个 unsafe 块必须有 // SAFETY: 注释
- SAFETY 注释必须说明为什么这段代码是正确的 (不是 "我觉得没问题")
- 所有 unsafe 代码必须通过 Miri 测试
- unsafe 只允许在以下场景:
  - SPSC 队列内部 (MaybeUninit, ptr::read/write)
  - 对象池内部 (如有自定义实现)
  - SIMD intrinsics
  - FFI 调用 (core_affinity, mmap)
```

### 5. Deterministic (确定性)

```
核心不变量:
  相同的 input event log -> 经过相同的 Engine -> 产生完全相同的 output event log

这意味着:
- 撮合逻辑中不能使用系统时间 (时间由事件携带)
- 不能使用随机数
- 不能依赖线程调度顺序 (单线程撮合保证了这一点)
- HashMap 的迭代顺序不影响结果 (或使用确定性 HashMap)
```

### 6. Event-Sourced (事件溯源)

```
所有状态变更都是事件:
- 订单簿的状态 = 初始空状态 + 所有事件的顺序应用
- 可以从任意 snapshot + 后续事件恢复到任意时间点的状态
- 可以用事件日志进行回测 (backtesting)
- 可以用事件日志进行审计 (audit trail)
```

---

## 依赖选型 (Dependency Table)

| Crate | 版本 | 用途 | 为什么选它 |
|-------|------|------|-----------|
| `slab` | latest | 对象池 (Order/PriceLevel 存储) | 类型安全的 O(1) insert/remove，无 unsafe，Tokio 团队维护 |
| `crossbeam-utils` | latest | `CachePadded` 等并发工具 | 工业级 false sharing 防护，被 Tokio 等项目广泛使用 |
| `tokio` | 1.x | 异步运行时 | Gateway I/O 复用。**仅在 Gateway 层使用，不在撮合热路径上** |
| `tonic` | latest | gRPC 框架 | 高性能 Rust gRPC，基于 Tokio + Hyper |
| `axum` | latest | REST API | Tokio 官方团队项目，基于 tower::Service 生态，可复用大量 middleware |
| `tokio-tungstenite` | latest | WebSocket | 基于 Tokio 的异步 WebSocket 实现 |
| `fefix` | latest | FIX 4.4 协议 | 目前唯一可用的 Rust FIX 协议实现 |
| `serde` | 1.x | 序列化 / 反序列化 | 配置文件解析、REST API JSON、快照序列化的基础设施 |
| `rkyv` | latest | Zero-copy 反序列化 | Snapshot 恢复速度关键：反序列化 1M orders ~1ms (vs bincode ~50ms) |
| `lz4_flex` | latest | 压缩 | WAL 旧文件压缩。纯 Rust 实现，压缩速度 > 1 GB/s |
| `tracing` | latest | 结构化日志 | 零开销 structured logging + span + `#[instrument]` 宏 |
| `tracing-subscriber` | latest | 日志输出 | JSON / pretty 格式化，`EnvFilter` 运行时控制日志级别 |
| `prometheus` | latest | Prometheus 指标 | 行业标准监控方案，Grafana 原生支持 |
| `criterion` | latest | 基准测试 | 统计学 benchmark 框架，Bootstrap 置信区间，回归检测 |
| `proptest` | latest | 属性测试 | 随机输入生成 + shrinking，覆盖人类想不到的边界条件 |
| `core_affinity` | latest | CPU 线程绑核 | 跨平台 CPU affinity API (Linux sched_setaffinity, macOS thread_policy) |
| `mimalloc` | latest | 全局分配器 | 替代系统 malloc，小对象分配性能显著提升。**仅用于 Gateway 等非热路径** |
| `bumpalo` | latest | Arena 分配器 | Per-request 临时内存：一次分配多次使用，请求结束时整体释放 |
| `smallvec` | latest | 小容量 Vec 替代 | 内联存储 ≤ N 个元素避免堆分配，超出时自动 fallback。撮合事件输出使用 `SmallVec<[EngineEvent; 8]>` |
| `arrayvec` | latest | 固定容量栈上 Vec | 零堆分配的定长容器，用于 SPSC batch_pop 等对分配敏感的场景 |
| `hashbrown` | latest | HashMap 实现 | 基于 Swiss Table，比 std HashMap 快 20-40% (更好的 probing 策略) |
| `thiserror` | latest | 错误类型 | 自动生成 `Display` + `Error` trait 实现，零运行时开销 |
| `toml` | latest | 配置解析 | 解析 TOML 配置文件 |
| `crc32fast` | latest | CRC32 校验 | 利用硬件 CRC32 指令，用于 WAL entry 完整性校验 |

---

## 风险评估

| 风险 | 影响 | 概率 | 缓解措施 |
|------|------|------|----------|
| Lock-free 实现有 data race | 高 | 中 | Miri 检测 + Loom 并发模型检查 + 大量 stress test |
| 性能目标未达成 | 中 | 低 | 渐进式优化，每阶段有基准。Phase 2 目标故意保守 |
| rkyv 格式不向后兼容 | 中 | 中 | 快照带版本号，升级时先创建新格式快照再停旧版本 |
| 订单类型交互复杂度爆炸 | 中 | 中 | P0 先实现最简类型，高级类型有独立测试矩阵 |
| FIX 协议合规问题 | 低 | 中 | 使用 FIX 认证工具验证，对照 FIX 4.4 规范逐字段检查 |
| 跨平台兼容性 | 低 | 低 | Linux 优先生产部署，macOS 开发兼容。ARM / x86 都支持 |
| 依赖 crate 安全漏洞 | 低 | 低 | `cargo audit` 加入 CI，定期更新依赖 |
| 内存安全 (unsafe bug) | 高 | 低 | Rust 保证 + Miri + SAFETY 注释审查 + unsafe 代码最小化 |

---

## Implementation Status

> Updated: 2026-03-05

### Completed Phases

| Phase | Description | Test Count | Status |
|-------|-------------|-----------|--------|
| Phase 1 | Foundation: Core types (Price, Quantity, Order, Event) + OrderBook (BTreeMap + Slab + intrusive linked list) | 50 (core) + 49 (book) | Done |
| Phase 2 | Engine + IO: Matcher + STP + Risk + SymbolEngine + Lock-free SPSC/MPSC queues | 37 (engine) + 13 (io) | Done |
| Phase 3 | Networking: REST (axum) + WebSocket + TCP binary protocol + EngineBridge | 102 (gateway) + 51 (proto) | Done |
| Phase 4 | Persistence + Observability: WAL + Snapshots + Recovery + Config + Metrics + Docker | 23 (persist) + 31 (server) | Done |

**Total: 356 tests, all passing. Clippy clean, fmt clean.**

### Deterministic Replay (Post-Phase 4 Enhancement)

Key architectural change to the persistence layer:

- **WAL now stores Commands (input) instead of Events (output)**
  - `WalCommand { sequence, symbol_id, command, account_id, timestamp }`
  - True write-ahead: command logged BEFORE engine processes it
  - WAL v2 format with backward-compatible v1 legacy reader

- **Deterministic recovery**: replaying the same command sequence on a fresh engine produces identical book state (verified by tests)

- **Timestamp determinism**: `SymbolEngine::process_command` takes timestamp as parameter, never calls `SystemTime::now()`. Timestamps originate from bridge.rs for live traffic, from WAL entries for replay.

- **Command serialization**: `Command` enum now derives `serde::Serialize` + `serde::Deserialize`

### Remaining (Phase 5): Extreme Optimization

- Zero-allocation audit (counting allocator)
- PGO + LTO pipeline
- SIMD batch price comparison
- OS-level tuning (huge pages, isolcpus, SCHED_FIFO)
- Advanced order types (Iceberg, Trailing Stop)

### Benchmark Results

| Metric | Measured |
|--------|----------|
| Order placement | ~54ns |
| Mixed workload throughput | ~6.7M ops/sec |
| BBO query | sub-nanosecond |
| SPSC push/pop | ~11ns |
