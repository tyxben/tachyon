# 第三章：订单簿 — 高性能数据结构设计

> **阅读本章后，你将理解**：为什么撮合引擎的订单簿不使用 `Vec` 或 `LinkedList`，而是用 BTreeMap + Slab + 侵入式链表构建三层索引架构，以及每层如何协作实现 O(1) 增删和 O(log N) 价格排序。

---

## 目录

- [3.1 整体架构：三层索引模型](#31-整体架构三层索引模型)
- [3.2 为什么选择 BTreeMap](#32-为什么选择-btreemap)
- [3.3 Slab 分配器：O(1) 内存池](#33-slab-分配器o1-内存池)
- [3.4 侵入式双向链表](#34-侵入式双向链表)
- [3.5 价格层级操作](#35-价格层级操作)
- [3.6 BBO 缓存机制](#36-bbo-缓存机制)
- [3.7 核心代码走读](#37-核心代码走读)

---

## 3.1 整体架构：三层索引模型

Tachyon 的订单簿为每个交易品种（Symbol）维护一个 `OrderBook` 实例。每个 `OrderBook` 包含买方（bids）和卖方（asks）两棵独立的价格树。其核心结构定义在 `crates/tachyon-book/src/book.rs:19-38`：

```rust
pub struct OrderBook {
    symbol: Symbol,
    config: SymbolConfig,
    bids: BTreeMap<Price, usize>,      // 买方：价格 -> levels slab 索引
    asks: BTreeMap<Price, usize>,      // 卖方：价格 -> levels slab 索引
    orders: Slab<Order>,               // 所有订单的 O(1) 存储池
    levels: Slab<PriceLevel>,          // 所有价格层级的 O(1) 存储池
    order_map: HashMap<OrderId, usize>,// 订单ID -> orders slab 索引
    best_bid: Option<Price>,           // 缓存的最优买价
    best_ask: Option<Price>,           // 缓存的最优卖价
    sequence: u64,                     // 本地操作计数器
}
```

三层索引的数据流如下图所示：

```
                         OrderBook
                ┌──────────────────────┐
                │  best_bid: 50200     │
                │  best_ask: 50300     │
                └──────────────────────┘
                         │
          ┌──────────────┼──────────────┐
          ▼                             ▼
    bids (BTreeMap)              asks (BTreeMap)
    ┌─────────────┐              ┌─────────────┐
    │ 50200 → lk3 │              │ 50300 → lk5 │
    │ 50100 → lk2 │              │ 50400 → lk6 │
    │ 50000 → lk1 │              │ 50500 → lk7 │
    └──────┬──────┘              └──────┬──────┘
           │                            │
           ▼                            ▼
   ┌─── levels: Slab<PriceLevel> ───────────┐    ◀── 第二层：Slab 分配器
   │  lk3: { price: 50200,                  │
   │         total_qty: 350,                 │
   │         order_count: 3,                 │
   │         head_idx: 7, tail_idx: 12 }     │
   └─────────────────────┬──────────────────┘
                         │
                         ▼
   ┌─── orders: Slab<Order> ────────────────┐    ◀── 第三层：侵入式链表
   │                                         │
   │  [7]  ──next→  [9]  ──next→  [12]      │
   │       ◀prev──       ◀prev──             │
   │  qty:100       qty:150       qty:100    │
   └─────────────────────────────────────────┘

   ┌─── order_map: HashMap<OrderId, usize> ──┐   ◀── 快速撤单索引
   │  OrderId(1001) → 7                       │
   │  OrderId(1005) → 9                       │
   │  OrderId(1009) → 12                      │
   └──────────────────────────────────────────┘
```

**三层各自的职责**：

| 层级 | 数据结构 | 职责 | 时间复杂度 |
|------|---------|------|-----------|
| 第一层 | `BTreeMap<Price, usize>` | 价格排序，快速找到最优价 | O(log N) 插入/删除 |
| 第二层 | `Slab<PriceLevel>` | 价格层级元数据 + 链表头尾指针 | O(1) 访问 |
| 第三层 | `Slab<Order>` + 侵入式链表 | 同价位订单的时间优先队列 | O(1) 头部弹出/尾部追加/按键删除 |
| 辅助 | `HashMap<OrderId, usize>` | 订单 ID 到 slab 索引的映射 | O(1) 查找 |

> **面试考点 3.1**
> - Q: 为什么需要三层索引？一个 `BTreeMap<Price, Vec<Order>>` 不行吗？
> - A: `Vec<Order>` 在中间删除是 O(n)，而撤单（cancel）是交易所最频繁的操作之一（通常占 90%+ 的消息量）。侵入式链表可以通过 `order_map` 直接定位到 slab 索引，然后 O(1) 完成双向链表摘除。此外 `Vec` 的增长会触发 realloc 和 memcpy，而 Slab 是稳定索引的内存池，不会移动已有元素。

---

## 3.2 为什么选择 BTreeMap

价格层级需要排序存储——买方从高到低遍历（最优买价 = 最大值），卖方从低到高遍历（最优卖价 = 最小值）。以下是几种候选数据结构的对比：

| 特性 | BTreeMap | 跳表 (Skip List) | 排序数组 | HashMap |
|------|----------|-----------------|---------|---------|
| 插入 | O(log N) | O(log N) 期望 | O(N) 移动 | O(1) 均摊 |
| 删除 | O(log N) | O(log N) 期望 | O(N) 移动 | O(1) 均摊 |
| 查找最优价 | O(log N)* | O(1)** | O(1) | O(N) |
| 有序遍历 | O(K) 极快 | O(K) | O(K) 极快 | 不支持 |
| 缓存友好性 | 优秀 (B-tree 节点) | 差 (指针跳跃) | 极好 (连续内存) | 一般 |
| 实现复杂度 | 零 (标准库) | 高 | 低 | 零 (标准库) |
| 内存开销 | 中 | 高 (多层指针) | 低 | 中 |

*注：BTreeMap 的 `iter().next()` 和 `iter().next_back()` 是 O(log N)，但 Tachyon 通过 BBO 缓存将其降为 O(1)。*

**BTreeMap 胜出的关键原因**：

1. **标准库零成本引入**：`std::collections::BTreeMap` 经过大量优化和实战验证，无需引入额外依赖
2. **缓存行友好**：B-tree 的每个内部节点存储多个 key-value 对（Rust 实现中通常 B=6，每节点最多 11 个元素），远优于红黑树或跳表的逐指针跳跃
3. **稳定的最坏情况**：O(log N) 是确定性的，不像跳表依赖随机数生成器的期望值
4. **价格层级数量有限**：实际交易中活跃价格层级通常在几百到几千个量级，BTreeMap 在这个规模下 log N 极小（log2(1000) ≈ 10）

**为什么不用 HashMap？** 虽然 HashMap 的插入/删除是 O(1)，但撮合引擎需要"找到对手方最优价格"——即有序遍历。HashMap 无法高效排序，每次都需要 O(N) 扫描所有价格。

**为什么不用跳表？** 跳表在 Rust 生态中没有标准库级别的高质量实现。自行实现跳表的代码量大、正确性难保证，且其指针密集的结构对 CPU 缓存不友好。对于 Tachyon 的规模（<10K 价格层级），BTreeMap 的性能已经足够，没必要引入额外复杂度。

> **面试考点 3.2**
> - Q: BTreeMap vs HashMap 在订单簿中的取舍？
> - A: HashMap 查找 O(1) 但无序，BTreeMap O(log N) 但有序。撮合需要"按价格从优到劣遍历"，必须有序。且价格层级数量有限（通常 <10K），log N < 14，HashMap 的常数因子优势在这个量级下可忽略。
> - Q: 为什么不直接用 BTreeMap 的 `first_key_value()` / `last_key_value()` 获取 BBO，而要单独缓存？
> - A: 虽然 BTreeMap 这两个方法是 O(log N)，但在热路径上每次都走 B-tree 会增加不必要的缓存行访问。Tachyon 在 `add_order` / `cancel_order` 中增量维护 `best_bid` / `best_ask`，使 BBO 查询降为 O(1)。详见 [3.6 BBO 缓存机制](#36-bbo-缓存机制)。

---

## 3.3 Slab 分配器：O(1) 内存池

### 什么是 Slab？

`slab` crate 提供了一个基于 `Vec` 的固定容量内存池。其核心思想：

```
逻辑视图：
┌─────┬─────┬─────┬─────┬─────┬─────┬─────┬─────┐
│  0  │  1  │  2  │  3  │  4  │  5  │  6  │  7  │  ← slab 索引
│ Ord │FREE │ Ord │ Ord │FREE │ Ord │FREE │ Ord │
└─────┴──┬──┴─────┴─────┴──┬──┴─────┴──┬──┴─────┘
         │                 │           │
         └────── free list: 1 → 4 → 6 ─┘

物理视图（底层 Vec<Entry<T>>）：
enum Entry<T> {
    Occupied(T),         // 存放实际数据
    Vacant(usize),       // 存放下一个空闲槽位的索引
}
```

### Slab 的关键特性

| 操作 | 时间复杂度 | 说明 |
|------|-----------|------|
| `insert(val)` | O(1) | 从 free list 头部取一个空槽，返回索引 |
| `remove(key)` | O(1) | 将槽位标记为 Vacant，挂回 free list |
| `slab[key]` | O(1) | 直接数组下标访问 |
| `contains(key)` | O(1) | 检查该槽位是否 Occupied |

### 为什么用 Slab 而不是 Vec？

传统做法是把订单放在 `Vec<Order>` 中，但这有三个致命问题：

1. **删除是 O(N)**：`Vec::remove(i)` 需要将 `i` 之后的所有元素左移。如果用 `swap_remove`，索引会变化，所有引用都要更新。
2. **索引不稳定**：`Vec` 增长时可能 realloc，所有指向旧内存的指针/索引都会失效。
3. **内存碎片**：频繁的插入/删除在 Vec 上要么留空洞（浪费内存），要么 compact（O(N) 移动）。

Slab 完美解决了这三个问题：

- **O(1) 删除**：标记为 Vacant 即可，无需移动
- **稳定索引**：元素一旦插入，其索引永远不变（直到被 remove）
- **零碎片**：空闲槽位通过 free list 复用，内存利用率高

### Tachyon 中的两个 Slab

```rust
orders: Slab<Order>,        // 容量 1024 预分配
levels: Slab<PriceLevel>,   // 容量 256 预分配
```

- `orders` Slab：存储所有活跃订单。slab 索引（`usize`）是订单在内存池中的位置，被 `order_map`、侵入式链表的 `prev`/`next` 指针引用。
- `levels` Slab：存储所有价格层级。slab 索引由 BTreeMap 的 value 引用。

预分配容量避免了初始阶段的频繁扩容。实际运行中 Slab 会根据需要自动增长（底层 `Vec::push`），但不会缩小——这对撮合引擎来说是理想的行为，因为高峰期分配的内存在低谷期仍可复用。

> **面试考点 3.3**
> - Q: Slab 和对象池 (Object Pool) 有什么区别？
> - A: 本质相同，都是预分配+复用。Slab 基于 `Vec` 实现，利用 Vacant 链表管理空闲槽位。关键区别在于 Slab 返回的是稳定的数组索引（usize），而传统对象池可能返回指针或 handle。在 Rust 中，slab 索引比 `Box`/`Rc` 更轻量，且不需要引用计数或 GC。
> - Q: 为什么 `Slab<PriceLevel>` 预分配 256 而 `Slab<Order>` 预分配 1024？
> - A: 订单数远多于价格层级数。一个典型的订单簿可能有几百个价格层级，但每个层级上有几十到几百个订单。256 和 1024 是合理的初始值，避免前几百次操作的频繁扩容。

---

## 3.4 侵入式双向链表

### 为什么叫"侵入式"？

传统链表（如 `std::collections::LinkedList`）中，链表节点包装数据：

```rust
// 传统链表：节点持有数据
struct Node<T> {
    data: T,
    prev: *mut Node<T>,
    next: *mut Node<T>,
}
```

侵入式链表反过来——**数据本身包含链表指针**：

```rust
// 侵入式：Order 自身存储 prev/next
#[repr(C)]
pub struct Order {
    // ... 业务字段 ...
    pub prev: u32,   // 前一个订单的 slab 索引
    pub next: u32,   // 后一个订单的 slab 索引
}
```

### 为什么用 u32 而不是 usize 或指针？

| 选择 | 大小 | 原因 |
|------|------|------|
| `u32` | 4 字节 | 最多支持 ~42 亿个同时在线订单，绰绰有余 |
| `usize` | 8 字节 | 浪费 4 字节，订单数不可能超过 u32::MAX |
| `*mut Order` | 8 字节 | 裸指针需要 unsafe，且 Slab 增长后指针失效 |
| `Option<u32>` | 8 字节 | Option<u32> 需要 tag 字节 + 对齐填充 |

`NO_LINK = u32::MAX` 作为哨兵值表示"没有前驱/后继"，避免了 `Option` 的额外开销：

```rust
// crates/tachyon-core/src/order.rs:99
pub const NO_LINK: u32 = u32::MAX;
```

### 链表内存布局

```
PriceLevel (50200)
  head_idx: 7
  tail_idx: 12

  orders Slab:
  ┌──────────────────────────────────────────────────┐
  │ [7] Order { id:1001, qty:100, prev:MAX, next:9 } │ ◀── 头部（最早）
  │                         │                        │
  │ [9] Order { id:1005, qty:150, prev:7,   next:12} │
  │                         │                        │
  │ [12] Order { id:1009, qty:100, prev:9,  next:MAX}│ ◀── 尾部（最晚）
  └──────────────────────────────────────────────────┘

  prev:MAX  表示 NO_LINK（链表头）
  next:MAX  表示 NO_LINK（链表尾）
```

### 侵入式链表 vs Box\<Node\> 链表

| 特性 | 侵入式链表 (Tachyon) | Box\<Node\> 链表 |
|------|---------------------|-----------------|
| 内存分配 | 零额外分配（prev/next 在 Order 内） | 每个节点一次堆分配 |
| 缓存局部性 | 优秀（Order 连续存储在 Slab 的 Vec 中） | 差（每个 Box 独立堆地址） |
| 按键 O(1) 删除 | 支持（通过 slab 索引定位 + 双向摘除） | 需要额外的 HashMap 维护 Node 指针 |
| 代码复杂度 | 中（需手动维护 prev/next） | 低（标准库 LinkedList，但不支持按键删除） |
| 内存开销/订单 | +8 字节（两个 u32） | +16 字节（两个指针）+ Box 头部 |

侵入式链表的核心优势：**零额外堆分配** + **O(1) 按键删除**。撮合引擎的热路径是撤单（cancel），需要通过 `order_map` 拿到 slab 索引后直接摘除该订单，而不需要从头遍历链表。

### O(1) 按键删除的实现

```
删除 [9]（OrderId: 1005）之前：
  [7] ──next:9──→  [9] ──next:12──→ [12]
  [7] ◀──prev:7── [9] ◀──prev:9──  [12]

步骤 1: order_map.get(1005) → slab_key = 9
步骤 2: 读取 orders[9].prev = 7, orders[9].next = 12
步骤 3: orders[7].next = 12   (前驱指向后继)
步骤 4: orders[12].prev = 7   (后继指向前驱)

删除 [9]（OrderId: 1005）之后：
  [7] ──next:12──→ [12]
  [7] ◀──prev:7── [12]
```

> **面试考点 3.4**
> - Q: 为什么不用 `std::collections::LinkedList<Order>`？
> - A: 三个原因：(1) Rust 的 `LinkedList` 不支持按键 O(1) 删除——只有 `pop_front`/`pop_back` 是 O(1)，按值查找删除是 O(N)；(2) 每个节点是独立的 `Box` 分配，破坏缓存局部性；(3) 侵入式设计让 Order 同时是业务数据和链表节点，无需额外包装。
> - Q: 如果 slab 索引超过 u32::MAX 怎么办？
> - A: u32::MAX ≈ 42 亿。即使每秒处理 500 万订单，也需要运行 ~14 分钟才会用完——但 Slab 的索引是回收复用的（删除后的空槽会被后续 insert 重用），所以并发活跃订单数才是瓶颈，而不是累计总量。42 亿个同时在线订单在实际中不可能出现。

---

## 3.5 价格层级操作

### PriceLevel 结构

每个价格层级维护该价位下所有订单的汇总信息（`crates/tachyon-book/src/level.rs:8-16`）：

```rust
pub struct PriceLevel {
    pub price: Price,
    pub total_quantity: Quantity,  // 该价位总数量（增量更新，不遍历）
    pub order_count: u32,         // 该价位订单数
    pub head_idx: u32,            // 链表头（最早的订单）
    pub tail_idx: u32,            // 链表尾（最晚的订单）
}
```

### add_order 完整流程

当一个限价单未完全成交、需要挂到订单簿时，`add_order()` 执行以下步骤（`book.rs:135-201`）：

```
输入: Order { id: 1009, side: Buy, price: 50200, qty: 100 }

Step 1: 验证订单
  ├── 价格对齐 tick_size ✓
  ├── 价格在 [min_price, max_price] 范围内 ✓
  ├── 数量对齐 lot_size ✓
  └── 数量在 [min_order_qty, max_order_qty] 范围内 ✓

Step 2: 重复检查
  └── order_map.contains_key(1009) → false ✓

Step 3: 重置链表指针
  └── order.prev = NO_LINK, order.next = NO_LINK

Step 4: 插入 orders Slab
  └── order_key = orders.insert(order) → 12

Step 5: 注册到 order_map
  └── order_map.insert(OrderId(1009), 12)

Step 6: 查找或创建 PriceLevel
  ├── bids.get(50200) → Some(lk3)  (已有该价位)
  └── 如果没有: levels.insert(PriceLevel::new(50200))

Step 7: 追加到链表尾部
  ├── old_tail = levels[lk3].tail_idx = 9
  ├── orders[9].next = 12          (旧尾 → 新节点)
  ├── orders[12].prev = 9          (新节点 → 旧尾)
  └── levels[lk3].tail_idx = 12    (更新尾指针)

Step 8: 更新层级汇总
  └── levels[lk3].total_quantity += 100
      levels[lk3].order_count += 1

Step 9: 更新 BBO 缓存
  └── best_bid = max(best_bid, 50200) = 50200 (不变)
```

### cancel_order 完整流程

撤单是订单簿最频繁的操作（`book.rs:208-276`）：

```
输入: cancel_order(OrderId(1005))

Step 1: 查找 slab 索引
  └── order_map.get(1005) → order_key = 9

Step 2: 读取订单信息
  └── side=Buy, price=50200, remaining=150, prev=7, next=12

Step 3: 查找价格层级
  └── bids.get(50200) → level_key = lk3

Step 4: 双向链表摘除
  ├── orders[7].next = 12     (前驱绕过被删节点)
  └── orders[12].prev = 7     (后继绕过被删节点)

Step 5: 更新 PriceLevel head/tail
  ├── head_idx 仍是 7 (未变)
  └── tail_idx 仍是 12 (未变)
  (如果删除的是 head，则 head_idx = next)
  (如果删除的是 tail，则 tail_idx = prev)

Step 6: 更新层级汇总
  └── total_quantity -= 150, order_count -= 1

Step 7: 检查层级是否为空
  └── order_count > 0 → 层级保留

Step 8: 清理 order_map 和 slab
  └── order_map.remove(1005), orders.remove(9)

  (如果层级为空，额外执行:)
  └── levels.remove(lk3)
      bids.remove(50200)
      if best_bid == 50200:
          best_bid = bids.keys().next_back()  // 重新扫描
```

### 空层级自动清理

当一个价格层级的最后一个订单被删除时（`order_count` 降为 0），Tachyon 自动执行清理：

1. 从 `levels` Slab 中移除该 PriceLevel
2. 从 `bids`/`asks` BTreeMap 中移除该价格的映射
3. 如果被移除的价格恰好是 BBO，重新从 BTreeMap 取最优价

这确保了 BTreeMap 中不会积累"幽灵"价格层级，保持结构干净。

> **面试考点 3.5**
> - Q: `add_order` 的时间复杂度是多少？
> - A: O(log N)，其中 N 是价格层级数。瓶颈在 `BTreeMap::get` / `BTreeMap::insert`（O(log N)）。其余操作——slab insert、HashMap insert、链表追加——都是 O(1)。
> - Q: `cancel_order` 的时间复杂度呢？
> - A: 最佳情况 O(1)（HashMap 查找 + 链表摘除 + slab remove），但如果删除后层级为空且该层级是 BBO，需要 O(log N) 从 BTreeMap 重新查找最优价。均摊来看接近 O(1)，因为大多数撤单不会清空整个价格层级。
> - Q: `modify_order` 是怎么实现的？
> - A: Cancel + Re-insert。修改后的订单会**失去时间优先权**，被追加到新价位的链表尾部。这是业界标准做法——如果修改保留优先权，做市商可以无限"刷新"自己的队列位置而不承担风险。

---

## 3.6 BBO 缓存机制

BBO（Best Bid and Offer）是订单簿最频繁被查询的数据——撮合引擎在每次匹配前都要获取对手方最优价。如果每次都从 BTreeMap 查找，需要 O(log N) 的树遍历。

Tachyon 用两个 `Option<Price>` 字段缓存 BBO：

```rust
best_bid: Option<Price>,   // 最优买价 = bids 中最大的 key
best_ask: Option<Price>,   // 最优卖价 = asks 中最小的 key
```

### 增量更新策略

BBO 缓存只在必要时更新，避免每次都全量扫描：

**添加订单时（`add_order`）**：

```rust
// book.rs:187-198
match side {
    Side::Buy => match self.best_bid {
        None => self.best_bid = Some(price),                    // 空簿：直接设置
        Some(current) if price > current => self.best_bid = Some(price), // 更优价
        _ => {}                                                  // 不影响 BBO
    },
    Side::Sell => match self.best_ask {
        None => self.best_ask = Some(price),
        Some(current) if price < current => self.best_ask = Some(price),
        _ => {}
    },
}
```

只有新订单价格**严格优于**当前 BBO 时才更新。大多数订单不影响 BBO，所以这个分支几乎总是走 `_ => {}` 空操作。

**撤单/删除时**：

```rust
// book.rs:256-267 (cancel_order 中)
// 只有层级被清空、且被清空的恰好是 BBO 价位时，才需要重新计算
if self.best_bid == Some(price) {
    self.best_bid = self.bids.keys().next_back().copied();  // O(log N) 重新查找
}
```

重新查找只在"BBO 价位的最后一个订单被删除"时触发——这是低频事件。

**查询 BBO**：

```rust
// book.rs:465-472
pub fn best_bid_price(&self) -> Option<Price> {
    self.best_bid   // O(1) 直接返回缓存值
}
pub fn best_ask_price(&self) -> Option<Price> {
    self.best_ask   // O(1) 直接返回缓存值
}
```

### 正确性保证

缓存的正确性依赖于一个不变量：**每次修改 BTreeMap 后，如果变动涉及 BBO 价位，必须同步更新缓存**。Tachyon 在以下所有路径中都维护了这个不变量：

- `add_order()` — 新订单可能改善 BBO
- `cancel_order()` — 删除可能使 BBO 恶化
- `remove_order_by_key()` — 撮合时填充完一个订单
- `pop_front_order()` — 撮合时弹出队首订单
- `refresh_bbo()` — 兜底：强制从 BTreeMap 重新计算

`refresh_bbo()` 是防御性方法，在恢复（recovery）等场景中使用，确保任何路径遗漏不会导致永久性的 BBO 不一致。

> **面试考点 3.6**
> - Q: 为什么不用 BTreeMap 的 `first_key_value()` / `last_key_value()` 直接获取 BBO？
> - A: 它们是 O(log N)。在撮合热路径上，Matcher 每次循环都要检查 BBO——如果对手方有 100 个价格层级，每次 `match_aggressive` 可能调用 best_ask_price() 上百次（每成交一个订单查一次）。O(1) 缓存 vs O(log N) 遍历在这种场景下差异显著。
> - Q: 缓存会不会和 BTreeMap 不一致？
> - A: 理论上有 bug 风险，但 Tachyon 在所有写路径（add/cancel/remove）上都同步维护缓存，并提供 `refresh_bbo()` 作为兜底。可以在 debug 模式下加断言验证一致性。

---

## 3.7 核心代码走读

### OrderBook 构造函数

```rust
// book.rs:42-55
pub fn new(symbol: Symbol, config: SymbolConfig) -> Self {
    OrderBook {
        symbol,
        config,
        bids: BTreeMap::new(),
        asks: BTreeMap::new(),
        orders: Slab::with_capacity(1024),   // 预分配 1024 个订单槽位
        levels: Slab::with_capacity(256),    // 预分配 256 个价格层级槽位
        order_map: HashMap::with_capacity(1024),
        best_bid: None,
        best_ask: None,
        sequence: 0,
    }
}
```

**关键细节**：`Slab::with_capacity` 和 `HashMap::with_capacity` 避免了初始阶段的频繁 realloc。

### validate_order — 订单验证

```rust
// book.rs:79-104
fn validate_order(&self, order: &Order) -> Result<(), EngineError> {
    // 1. 价格必须对齐 tick_size
    if !price.is_aligned(self.config.tick_size) { ... }
    // 2. 价格在 [min_price, max_price] 范围内
    if price < self.config.min_price || price > self.config.max_price { ... }
    // 3. 数量对齐 lot_size
    if self.config.lot_size.raw() != 0 && qty.raw() % self.config.lot_size.raw() != 0 { ... }
    // 4. 数量在 [min_order_qty, max_order_qty] 范围内
    if qty < self.config.min_order_qty || qty > self.config.max_order_qty { ... }
}
```

验证在 `add_order` 的最开始执行——fast reject 原则，不合法的订单不会进入任何数据结构。

### remove_order_by_key — 撮合引擎的低层接口

```rust
// book.rs:487-549
pub fn remove_order_by_key(&mut self, order_key: usize) {
    // 1. 读取订单的 side, price, prev, next
    // 2. 找到对应的 PriceLevel
    // 3. 双向链表摘除
    // 4. 更新 PriceLevel 汇总
    // 5. 如果层级为空，清理 BTreeMap 和 BBO 缓存
    // 6. 从 order_map 和 slab 中移除
}
```

这个方法是 `cancel_order` 的低层版本，区别在于它接收 slab 索引（`order_key`）而不是 `OrderId`，省去一次 HashMap 查找。撮合引擎在匹配过程中已经知道 slab 索引，所以直接使用这个方法以获得最佳性能。

### get_depth — 深度快照

```rust
// book.rs:295-312
pub fn get_depth(&self, n: usize) -> (DepthSide, DepthSide) {
    let bids_depth = self.bids.iter().rev().take(n)  // 买方：从高到低
        .map(|(&price, &level_key)| (price, self.levels[level_key].total_quantity))
        .collect();
    let asks_depth = self.asks.iter().take(n)         // 卖方：从低到高
        .map(|(&price, &level_key)| (price, self.levels[level_key].total_quantity))
        .collect();
    (bids_depth, asks_depth)
}
```

BTreeMap 的有序性在这里体现了优势：`.iter()` 天然按升序遍历，`.iter().rev()` 按降序遍历。`take(n)` 只取前 n 个价格层级，常见的 depth 请求（n=5 或 n=20）非常高效。

### 复杂度汇总表

| 操作 | 时间复杂度 | 热路径? | 说明 |
|------|-----------|---------|------|
| `add_order` | O(log N) | 是 | 瓶颈在 BTreeMap 查找/插入 |
| `cancel_order` | O(1) 均摊 | 是 | HashMap + 链表摘除；极少触发 BBO 重算 |
| `remove_order_by_key` | O(1) 均摊 | 是 | 同上，省去一次 HashMap 查找 |
| `best_bid_price` | O(1) | 是 | 直接返回缓存 |
| `best_ask_price` | O(1) | 是 | 直接返回缓存 |
| `get_order` | O(1) | 否 | HashMap + slab 索引访问 |
| `get_depth(n)` | O(n) | 否 | BTreeMap 迭代器取前 n 个 |
| `modify_order` | O(log N) | 否 | cancel + add_order |
| `spread` | O(1) | 否 | 两个缓存值相减 |

> **面试考点 3.7**
> - Q: `remove_order_by_key` 和 `cancel_order` 有什么区别？何时用哪个？
> - A: `cancel_order` 接收 `OrderId`，需要先通过 `order_map` 查找 slab 索引（O(1) HashMap 查找），适用于外部撤单请求。`remove_order_by_key` 直接接收 slab 索引，跳过 HashMap 查找，适用于撮合引擎内部——因为 Matcher 在匹配过程中已经知道被成交订单的 slab 索引。
> - Q: 如果要支持 100 万个活跃订单，内存占用大概多少？
> - A: 每个 `Order` 结构约 80 字节（`#[repr(C)]` 布局），100 万订单 ≈ 80 MB。加上 `order_map`（~40 字节/条目）≈ 40 MB，`PriceLevel`（~28 字节/条目）和 `BTreeMap` 节点开销可忽略。总计约 120-150 MB，远低于 Tachyon 的 1GB 目标。

---

> **下一章预告**：[第四章：撮合引擎 — 价格时间优先算法](./04-matching-engine.md) 将深入 `Matcher` 的核心匹配循环，讲解 Market/Limit/IOC/FOK/PostOnly 各种订单类型的处理逻辑，以及自成交防护（STP）机制。
