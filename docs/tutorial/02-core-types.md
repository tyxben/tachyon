# 第2章：核心类型深度解析

[<< 上一章：系统架构](./01-architecture.md) | [返回目录](./README.md) | [下一章：订单簿 >>](./03-orderbook.md)

---

`tachyon-core` 是整个系统的类型基础。它只依赖 `serde`（序列化）和 `thiserror`（错误处理），
被其他所有 crate 共享。本章逐一分析每个核心类型的设计决策。

---

## 2.1 Price — 定点价格 {#price-定点价格}

**文件位置**：`crates/tachyon-core/src/price.rs`

```rust
// crates/tachyon-core/src/price.rs:10-24
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default,
         serde::Serialize, serde::Deserialize)]
#[repr(transparent)]
pub struct Price(pub i64);
```

### 为什么用定点数而不是浮点数

金融系统中，浮点数的精度问题是致命的：

```
0.1 + 0.2 = 0.30000000000000004  (IEEE 754 double)
```

如果用浮点数表示价格，两个看起来相同的价格可能不相等，导致订单无法匹配。

定点数方案：用整数存储，通过 `price_scale` 确定小数点位置。

```
价格 50001.50 USDT，price_scale = 2
存储值：Price(5000150)
实际值 = 5000150 / 10^2 = 50001.50
```

### 为什么用 `i64` 而不是 `u64`

价格差（spread）可以为负数：`bid - ask` 在某些计算场景下需要表示负值。
如果用 `u64`，减法会导致 underflow panic 或需要到处用 `checked_sub`。

`i64` 的范围 `[-9.2×10^18, 9.2×10^18]` 对于金融价格绰绰有余。
即使以最高精度 `price_scale = 8` 表示，`i64::MAX / 10^8 = 92,233,720,368.54` 足以覆盖任何资产价格。

### `#[repr(transparent)]` 的作用

```rust
#[repr(transparent)]
pub struct Price(pub i64);
```

`repr(transparent)` 保证 `Price` 在内存中与 `i64` 完全相同的布局。这意味着：
- `size_of::<Price>() == size_of::<i64>() == 8`
- 零抽象开销：传参、返回值与 `i64` 完全一致
- 可以安全地在 FFI 边界传递

### 关键方法

```rust
// crates/tachyon-core/src/price.rs:49-84

/// Checked addition. Returns `None` on overflow.
pub const fn checked_add(self, rhs: Price) -> Option<Price> { ... }

/// Checked subtraction. Returns `None` on overflow.
pub const fn checked_sub(self, rhs: Price) -> Option<Price> { ... }

/// Checked multiplication of Price by Quantity.
/// Uses i128 intermediate to avoid overflow.
pub const fn checked_mul_qty(self, qty: Quantity, scale_factor: u64) -> Option<Price> {
    let intermediate = (self.0 as i128) * (qty.raw() as i128);
    let result = intermediate / (scale_factor as i128);
    // ...
}
```

`checked_mul_qty` 的设计亮点：
- 用 `i128` 中间值避免乘法溢出（`i64 * u64` 可能超出 `i64` 范围）
- 除以 `scale_factor` 归一化结果
- 返回 `Option` 而非 panic，调用方决定如何处理溢出

```rust
// crates/tachyon-core/src/price.rs:101-108

/// Checks if the price is aligned to the given tick size.
pub const fn is_aligned(self, tick_size: Price) -> bool {
    if tick_size.0 == 0 { return false; }
    self.0 % tick_size.0 == 0
}
```

`is_aligned` 用于订单验证：确保价格是 tick_size 的整数倍。
例如 tick_size = 0.01（存储为 1），则价格 50001.505（存储为 5000150.5 → 不会出现，因为是整数）
只有整数值才能通过验证，这是定点数的天然优势。

> **面试考点**
>
> **Q: 为什么所有算术方法都是 `const fn`？**
>
> A: `const fn` 可以在编译期求值。虽然运行时调用和普通函数一样，但它允许用在 const 上下文中
> （如 `const TICK: Price = Price::new(100)`），并且向编译器传达了"这个函数是纯计算"的信号，
> 有助于优化。

---

## 2.2 Quantity — 定点数量 {#quantity-定点数量}

**文件位置**：`crates/tachyon-core/src/quantity.rs`

```rust
// crates/tachyon-core/src/quantity.rs:7-21
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default,
         serde::Serialize, serde::Deserialize)]
#[repr(transparent)]
pub struct Quantity(pub u64);
```

### 为什么用 `u64` 而不是 `i64`

数量永远非负。用 `u64` 可以：
1. **类型安全**：编译器阻止你构造负数量
2. **范围更大**：`u64::MAX = 1.8×10^19`，比 `i64::MAX` 大一倍
3. **语义清晰**：看到 `Quantity` 就知道 >= 0

### 饱和减法 vs Checked 减法

```rust
// crates/tachyon-core/src/quantity.rs:54-60
/// Checked subtraction. Returns `None` if `rhs > self`.
pub const fn checked_sub(self, rhs: Quantity) -> Option<Quantity> { ... }

// crates/tachyon-core/src/quantity.rs:74-77
/// Saturating subtraction. Result never goes below zero.
pub const fn saturating_sub(self, rhs: Quantity) -> Quantity {
    Quantity(self.0.saturating_sub(rhs.0))
}
```

两种减法的使用场景不同：
- `checked_sub`：订单验证，如果剩余数量不足则返回错误
- `saturating_sub`：PriceLevel 的 `sub_order_qty`，防止计数器下溢（防御性编程）

---

## 2.3 OrderId — 订单标识 {#orderid-订单标识}

**文件位置**：`crates/tachyon-core/src/order.rs:8-23`

```rust
// crates/tachyon-core/src/order.rs:8-23
/// Globally unique order identifier, assigned by the Sequencer.
/// Monotonically increasing. A 64-bit space supports ~1M orders/second for ~584,000 years.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, serde::Serialize, serde::Deserialize)]
pub struct OrderId(pub u64);
```

### ID 生成策略

```rust
// crates/tachyon-engine/src/engine.rs:40-42
// Encode symbol id in the upper bits so order IDs are globally unique across symbols.
let id_base = (symbol.raw() as u64) << 40;
// ...
self.next_order_id = id_base + 1;
```

OrderId 的高 24 位编码 Symbol ID，低 40 位是递增序号。这保证了：
- **全局唯一**：不同品种的引擎线程可以独立分配 ID，无需协调
- **可解码**：从 OrderId 可以反推出属于哪个品种
- **空间充足**：低 40 位支持每个品种 ~1 万亿个订单

```
OrderId 布局 (64 bits):
┌─────────────────┬──────────────────────────────────────┐
│  Symbol (24bit)  │          Sequence (40bit)             │
└─────────────────┴──────────────────────────────────────┘
```

> **面试考点**
>
> **Q: 为什么不用 UUID 做 OrderId？**
>
> A: UUID 是 128 位，OrderId 只需 64 位。在热路径上：
> - Hash 计算快一倍（u64 的 hash 通常是 identity 或 1-2 个指令）
> - 内存占用小一倍（Order 结构体中的 id 字段）
> - 比较操作是单条 CPU 指令
> - 单调递增的 u64 对 BTreeMap/排序更友好
>
> UUID 的"全球唯一"特性对于单进程撮合引擎是不必要的。

---

## 2.4 Symbol — 交易对标识

**文件位置**：`crates/tachyon-core/src/order.rs:31-50`

```rust
// crates/tachyon-core/src/order.rs:31-50
/// Trading pair identifier. Uses `u32` internally for minimal memory and fast comparison.
///
/// The mapping between human-readable names (e.g. "BTC/USDT") and `Symbol(u32)` values
/// is maintained externally in a `SymbolRegistry`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, serde::Serialize, serde::Deserialize)]
pub struct Symbol(pub u32);
```

`u32` 支持 42 亿个品种，足够任何交易所使用。`"BTC/USDT"` 这样的字符串只在 Gateway 层做映射，
内部全部用 `Symbol(u32)` 进行比较和索引——字符串比较是 O(n)，整数比较是 O(1)。

---

## 2.5 枚举类型 {#枚举类型}

### Side — 买卖方向

```rust
// crates/tachyon-core/src/order.rs:59-74
pub enum Side {
    Buy,
    Sell,
}

impl Side {
    pub const fn opposite(self) -> Side {
        match self {
            Side::Buy => Side::Sell,
            Side::Sell => Side::Buy,
        }
    }
}
```

`opposite()` 在撮合中频繁使用：买单匹配卖方 (asks)，卖单匹配买方 (bids)。
用 `const fn` 确保编译器可以内联甚至编译期求值。

### OrderType — 订单类型

```rust
// crates/tachyon-core/src/order.rs:77-81
pub enum OrderType {
    Limit,   // 限价单：指定价格
    Market,  // 市价单：按当前最优价格成交
}
```

### TimeInForce — 有效期策略

```rust
// crates/tachyon-core/src/order.rs:84-96
pub enum TimeInForce {
    GTC,         // Good-Til-Cancel: 直到取消
    IOC,         // Immediate-or-Cancel: 立即成交剩余取消
    FOK,         // Fill-or-Kill: 要么全部成交，要么全部拒绝
    PostOnly,    // 仅做 Maker：如果会立即成交则拒绝
    GTD(u64),    // Good-Til-Date: 到指定时间戳过期
}
```

`TimeInForce` 是带数据的枚举 (tagged union)：`GTD` 变体携带一个 `u64` 时间戳。
这让 Rust 编译器在 `match` 时要求穷尽所有分支，防止遗漏任何情况。

**内存布局**：`TimeInForce` 占 16 字节（8 字节 tag + 8 字节 `u64` payload，因为最大变体是 `GTD(u64)`）。

### RejectReason — 拒绝原因

```rust
// crates/tachyon-core/src/event.rs:5-16
pub enum RejectReason {
    InsufficientLiquidity,   // 流动性不足 (FOK 无法全部成交)
    PriceOutOfRange,         // 价格超出限制
    InvalidQuantity,         // 数量不合法
    InvalidPrice,            // 价格不合法 (未对齐 tick_size)
    BookFull,                // 订单簿满
    RateLimitExceeded,       // 超出频率限制
    SelfTradePrevented,      // 自成交防护
    PostOnlyWouldTake,       // PostOnly 订单会立即成交
    DuplicateOrderId,        // 重复的订单 ID
    OrderNotFound,           // 订单未找到 (取消时)
}
```

每种拒绝原因对应一个明确的业务场景，调用方可以通过 `match` 精确处理每种情况。

> **面试考点**
>
> **Q: Rust 的 enum 与 C/Java 的 enum 有什么区别？**
>
> A: Rust 的 enum 是 **代数数据类型 (ADT)**，也叫 tagged union 或 sum type。
> 每个变体可以携带不同类型的数据（如 `GTD(u64)`），而 C 的 enum 只是整数别名。
> Rust 编译器会在 match 时做穷尽性检查 (exhaustiveness check)，
> 确保你处理了所有变体，忘记一个就编译失败。这在金融系统中极其重要——
> 你不会因为忘记处理 `PostOnlyWouldTake` 而产生 bug。

---

## 2.6 Order — 订单结构体 {#order-订单结构体}

**文件位置**：`crates/tachyon-core/src/order.rs:101-129`

```rust
// crates/tachyon-core/src/order.rs:108-129
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[repr(C)]
pub struct Order {
    // 8-byte aligned fields (48 bytes)
    pub id: OrderId,           //  8 bytes — offset 0
    pub price: Price,          //  8 bytes — offset 8
    pub quantity: Quantity,     //  8 bytes — offset 16
    pub remaining_qty: Quantity,// 8 bytes — offset 24
    pub timestamp: u64,        //  8 bytes — offset 32
    pub account_id: u64,       //  8 bytes — offset 40

    // 16-byte enum (tag + u64 payload)
    pub time_in_force: TimeInForce, // 16 bytes — offset 48

    // 4-byte aligned fields (12 bytes)
    pub symbol: Symbol,        //  4 bytes — offset 64
    pub prev: u32,             //  4 bytes — offset 68
    pub next: u32,             //  4 bytes — offset 72

    // 1-byte fields (2 bytes + 2 bytes padding = 4 bytes)
    pub side: Side,            //  1 byte  — offset 76
    pub order_type: OrderType, //  1 byte  — offset 77
                               //  2 bytes padding — offset 78-79
}
// Total: 80 bytes
```

### 内存布局图

```
Offset  Field              Size    Align
──────  ─────              ────    ─────
 0      id (OrderId)        8       8
 8      price (Price)       8       8
16      quantity            8       8
24      remaining_qty       8       8
32      timestamp           8       8
40      account_id          8       8
48      time_in_force      16       8
64      symbol (Symbol)     4       4
68      prev (u32)          4       4
72      next (u32)          4       4
76      side (Side)         1       1
77      order_type          1       1
78      [padding]           2       -
──────                     ──
Total                      80 bytes
```

### 为什么用 `#[repr(C)]`

Rust 默认的 struct 布局 (`repr(Rust)`) 允许编译器重排字段以减少 padding。
但这有几个问题：

1. **不可预测**：字段顺序可能随编译器版本改变
2. **不利于序列化**：无法直接 `memcpy` 到磁盘或网络
3. **不利于调试**：`perf`/`gdb` 看到的偏移量不确定

`repr(C)` 遵循 C 语言的布局规则：按声明顺序排列，按类型对齐。
**我们手动将字段从大到小排列**，8 字节字段在前，4 字节字段居中，1 字节字段在末尾，
这样 padding 降到最低（仅末尾 2 字节）。

如果不手动排列，例如把 `side` 放在 `id` 后面：
```
id:     8 bytes  (offset 0)
side:   1 byte   (offset 8)
[pad]:  7 bytes  (offset 9-15)  ← 7 字节浪费！
price:  8 bytes  (offset 16)
```

### `prev` 和 `next` — 侵入式链表指针

```rust
// crates/tachyon-core/src/order.rs:98-99
/// Sentinel value indicating no linked-list link (prev/next).
pub const NO_LINK: u32 = u32::MAX;
```

订单在同一价格级别内按时间优先排序，形成双向链表。传统做法是用 `Box<Node<T>>` 指针，
但这意味着每个节点一次堆分配。侵入式设计将 `prev`/`next` 嵌入 Order 结构体本身，
指向 Slab 中的索引而非堆指针。

```
PriceLevel (price = 50000.00)
  head_idx ──► Slab[3]        Slab[7]        Slab[12]
               Order {        Order {        Order {
                 prev: NO_LINK  prev: 3        prev: 7
                 next: 7        next: 12       next: NO_LINK
               }              }              }  ◄── tail_idx
```

为什么用 `u32` 而不是 `usize`：
- 节省 4 字节（64 位系统上 `usize` = 8 字节）
- `u32::MAX` = 42 亿，超过任何合理的订单数量
- `NO_LINK = u32::MAX` 是天然的哨兵值

> **面试考点**
>
> **Q: `#[repr(C)]` 和 `#[repr(transparent)]` 的区别是什么？**
>
> A: `repr(transparent)` 只能用于单字段结构体（或其他字段都是 ZST），
> 保证与内部类型完全相同的布局和 ABI。用于 newtype pattern（如 `Price(i64)`）。
>
> `repr(C)` 用于多字段结构体，按 C 语言规则布局：字段按声明顺序，
> 每个字段对齐到其对齐要求。用于需要可预测布局的场景（如 Order 结构体）。
>
> **Q: Order 结构体是 80 字节，一条 cache line 是 64 字节，这有什么影响？**
>
> A: 一个 Order 跨越两条 cache line。访问 `side`/`order_type` 字段（offset 76-77）
> 时可能触发第二次 cache line load。好消息是：撮合热路径主要访问 `price`、`remaining_qty`、
> `prev`/`next` 这些字段，它们集中在前 80 字节的高频区域。
> 如果要进一步优化，可以把热字段拆成单独的 "hot struct"（64 字节）和冷字段分开存储。

---

## 2.7 Trade — 成交记录 {#trade-成交记录}

**文件位置**：`crates/tachyon-core/src/order.rs:132-142`

```rust
// crates/tachyon-core/src/order.rs:132-142
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Trade {
    pub trade_id: u64,           // 全局递增成交 ID
    pub symbol: Symbol,          // 交易品种
    pub price: Price,            // 成交价格（maker 的价格）
    pub quantity: Quantity,      // 成交数量
    pub maker_order_id: OrderId, // 挂单方 (被动成交)
    pub taker_order_id: OrderId, // 吃单方 (主动成交)
    pub maker_side: Side,        // maker 是买还是卖
    pub timestamp: u64,          // 成交时间 (纳秒)
}
```

关键语义：
- **价格**：始终是 maker 的价格（价格-时间优先原则：先挂的单决定成交价）
- **maker_side**：记录 maker 方向，可以推导出 taker 方向（取反即可）
- **timestamp**：纳秒精度，由引擎线程在处理命令时赋值

---

## 2.8 EngineEvent — 引擎事件 {#engineevent-引擎事件}

**文件位置**：`crates/tachyon-core/src/event.rs:23-56`

```rust
// crates/tachyon-core/src/event.rs:22-56
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub enum EngineEvent {
    /// 订单被接受并加入订单簿
    OrderAccepted {
        order_id: OrderId,
        symbol: Symbol,
        side: Side,
        price: Price,
        qty: Quantity,
        timestamp: u64,
    },
    /// 订单被拒绝
    OrderRejected {
        order_id: OrderId,
        reason: RejectReason,
        timestamp: u64,
    },
    /// 订单被取消
    OrderCancelled {
        order_id: OrderId,
        remaining_qty: Quantity,
        timestamp: u64,
    },
    /// 订单过期 (GTD)
    OrderExpired {
        order_id: OrderId,
        timestamp: u64,
    },
    /// 成交
    Trade {
        trade: Trade,
    },
    /// 订单簿深度变化
    BookUpdate {
        symbol: Symbol,
        side: Side,
        price: Price,
        new_total_qty: Quantity,
        timestamp: u64,
    },
}
```

### 事件流示例

假设一个限价买单 (Buy 10 @ 50000) 与两个卖单部分成交：

```
1. OrderAccepted { order_id: 100, side: Buy, price: 50000, qty: 10 }
2. Trade { maker_order_id: 50 (Sell 5 @ 49999), taker_order_id: 100, qty: 5 }
3. Trade { maker_order_id: 51 (Sell 3 @ 50000), taker_order_id: 100, qty: 3 }
4. BookUpdate { side: Buy, price: 50000, new_total_qty: 2 }  // 剩余 2 挂在簿上
```

**注意**：`OrderAccepted` 在所有撮合完成后才发出，且 `qty` 是剩余挂单的数量（不是原始数量）。
如果订单完全成交（被 IOC/FOK 吃掉），不会产生 `OrderAccepted` 事件。

---

## 2.9 SymbolConfig — 品种配置 {#symbolconfig-品种配置}

**文件位置**：`crates/tachyon-core/src/config.rs:1-29`

```rust
// crates/tachyon-core/src/config.rs:10-29
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct SymbolConfig {
    pub symbol: Symbol,         // 所属品种
    pub tick_size: Price,       // 最小价格变动 (e.g. Price(1) = 0.01 when scale=2)
    pub lot_size: Quantity,     // 最小数量变动
    pub price_scale: u8,        // 价格小数位数 (e.g. 2 → 除以 100)
    pub qty_scale: u8,          // 数量小数位数 (e.g. 8 → 除以 10^8)
    pub max_price: Price,       // 价格上限
    pub min_price: Price,       // 价格下限
    pub max_order_qty: Quantity, // 单笔最大数量
    pub min_order_qty: Quantity, // 单笔最小数量
}
```

**不同品种有不同的精度和限制**。例如：

| 品种 | tick_size | price_scale | lot_size | qty_scale |
|------|-----------|-------------|----------|-----------|
| BTC/USDT | 0.01 | 2 | 0.00001 | 5 |
| DOGE/USDT | 0.000001 | 6 | 1 | 0 |

`SymbolConfig` 在品种创建时设定，交易期间不可变。这保证了：
- 订单验证规则一致
- 价格比较语义不会改变
- 无需运行时加锁读取配置

---

## 2.10 EngineError — 错误类型 {#engineerror-错误类型}

**文件位置**：`crates/tachyon-core/src/error.rs:1-32`

```rust
// crates/tachyon-core/src/error.rs:4-32
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

    #[error("duplicate order id: {0}")]
    DuplicateOrderId(OrderId),
}
```

使用 `thiserror` derive 宏自动实现 `std::error::Error` trait 和 `Display`。
每个变体携带相关上下文（如 `OrderNotFound(OrderId)` 包含出错的订单 ID），
方便日志和调试。

**`EngineError` vs `RejectReason` 的区别**：
- `EngineError` 是内部错误类型，用于 `Result<T, EngineError>` 返回
- `RejectReason` 是外部面向客户端的拒绝原因，通过 `EngineEvent::OrderRejected` 传递

两者有映射关系但不完全一致，因为内部错误可能需要更多上下文，而外部只需要一个简洁的原因。

---

## 2.11 Newtype Pattern 总结

Tachyon 大量使用 Newtype Pattern：

| 类型 | 底层类型 | 为什么不直接用底层类型 |
|------|---------|---------------------|
| `Price` | `i64` | 防止与 `Quantity` 混淆；附加 `is_aligned`/`checked_mul_qty` 等领域方法 |
| `Quantity` | `u64` | 强制非负语义；防止与 `Price`/`OrderId` 混淆 |
| `OrderId` | `u64` | 不应该做算术运算（`order_id + 1` 是 bug）；仅做比较和 hash |
| `Symbol` | `u32` | 不应该做算术运算；语义明确 |

如果所有字段都是 `u64`：

```rust
// 危险：编译器不会阻止你交换参数顺序
fn create_trade(price: u64, quantity: u64, order_id: u64) { ... }
create_trade(order_id, price, quantity);  // 编译通过但完全错误！
```

用 Newtype：

```rust
fn create_trade(price: Price, quantity: Quantity, order_id: OrderId) { ... }
create_trade(order_id, price, quantity);  // 编译错误！类型不匹配
```

**零运行时开销**：所有 newtype 都是 `#[repr(transparent)]` 或单字段 `Copy` 类型，
编译后与裸 `i64`/`u64`/`u32` 完全一致。这是 Rust "零成本抽象" 的典型体现。

> **面试考点**
>
> **Q: Newtype pattern 的零成本是如何保证的？**
>
> A: `#[repr(transparent)]` 保证单字段结构体与内部类型有相同的 ABI。
> `#[derive(Clone, Copy)]` 让它像原始类型一样按值传递（寄存器传参）。
> 编译器在单态化 (monomorphization) 后，`Price::new(42)` 和直接用 `42i64` 生成的
> 机器码完全一致 —— newtype 的抽象在编译期被完全消除。
>
> **Q: 为什么 `OrderId` 不实现 `Ord`？**
>
> A: `OrderId` 只需要 `Eq` 和 `Hash`（用于 HashMap 查找）。
> 对订单 ID 排序在业务上没有意义（时间顺序由 `timestamp` 字段决定）。
> 不实现 `Ord` 可以防止误用，例如不小心用 OrderId 做 BTreeMap 的 key。

---

[<< 上一章：系统架构](./01-architecture.md) | [返回目录](./README.md) | [下一章：订单簿 >>](./03-orderbook.md)
