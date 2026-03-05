# 第四章：撮合引擎 — 价格时间优先算法

> **阅读本章后，你将理解**：Tachyon 如何实现价格-时间优先（Price-Time Priority）撮合算法，六种订单类型的完整处理逻辑，以及自成交防护（STP）和前置风控如何在不牺牲性能的前提下保障交易安全。

---

## 目录

- [4.1 价格-时间优先算法](#41-价格-时间优先算法)
- [4.2 订单类型](#42-订单类型)
- [4.3 撮合主循环](#43-撮合主循环)
- [4.4 自成交防护 (STP)](#44-自成交防护-stp)
- [4.5 前置风控 (Risk Manager)](#45-前置风控-risk-manager)
- [4.6 SymbolEngine — 协调者模式](#46-symbolengine--协调者模式)
- [4.7 事件生成机制](#47-事件生成机制)

---

## 4.1 价格-时间优先算法

### 核心规则

价格-时间优先（Price-Time Priority）是全球主流交易所使用的撮合算法。其规则简洁：

1. **价格优先**：更优的价格先成交。买单价格越高越优先，卖单价格越低越优先。
2. **时间优先**：同一价格下，先到的订单先成交（FIFO）。

### 完整撮合示例

假设当前订单簿状态（BTC/USDT）：

```
         买方 (Bids)                    卖方 (Asks)
  ┌──────────────────────┐      ┌──────────────────────┐
  │ 50200  [A:100] [B:50]│      │ 50300  [E:80] [F:120]│
  │ 50100  [C:200]       │      │ 50400  [G:300]       │
  │ 50000  [D:150]       │      │ 50500  [H:100]       │
  └──────────────────────┘      └──────────────────────┘
  (从高到低排列)                 (从低到高排列)
  best_bid = 50200               best_ask = 50300
```

现在一个 **Buy Limit 250 @ 50400** 的吃单（taker）进入：

```
Step 1: 对手方最优价 = best_ask = 50300
        50300 <= 50400 (买价 >= 卖价) → 可以成交

Step 2: 在 50300 层级匹配（时间优先）
        E: fill 80, taker remaining = 170
        F: fill 120, taker remaining = 50
        50300 层级清空，删除该价格层级

Step 3: 对手方最优价变为 50400
        50400 <= 50400 → 可以成交

Step 4: 在 50400 层级匹配
        G: fill 50, taker remaining = 0, G remaining = 250

Step 5: taker 完全成交，撮合结束

结果:
  Trade 1: Taker buy vs E sell, price=50300, qty=80
  Trade 2: Taker buy vs F sell, price=50300, qty=120
  Trade 3: Taker buy vs G sell, price=50400, qty=50

  成交价格是 maker（被动方）的挂单价，不是 taker 的限价。
  这确保了 taker 获得价格改善（想买 50400，实际部分在 50300 成交）。
```

撮合后的订单簿：

```
         买方 (Bids)                    卖方 (Asks)
  ┌──────────────────────┐      ┌──────────────────────┐
  │ 50200  [A:100] [B:50]│      │ 50400  [G:250]       │  ← G 部分成交
  │ 50100  [C:200]       │      │ 50500  [H:100]       │
  │ 50000  [D:150]       │      └──────────────────────┘
  └──────────────────────┘
  best_bid = 50200               best_ask = 50400  ← 更新
```

> **面试考点 4.1**
> - Q: 为什么成交价取 maker 的价格而不是 taker 的价格？
> - A: 这是"被动方价格优先"原则。Maker 在先前挂单时表达了"我愿意以这个价格交易"的承诺，taker 的限价是上限/下限。取 maker 价格可以给 taker 价格改善（price improvement），也是全球交易所的标准做法。
> - Q: 价格-时间优先 vs 按比例分配（pro-rata）？
> - A: Pro-rata 在同一价位按订单大小比例分配成交量，常见于期货交易所。价格-时间优先更简单、更公平（鼓励先报价的人），且实现更高效——只需遍历链表头部，不需要扫描整个价格层级再按比例计算。

---

## 4.2 订单类型

Tachyon 支持两种订单类型（`OrderType`）和五种有效期策略（`TimeInForce`），组合成六种行为：

### 订单类型对照表

| 类型 | OrderType | TimeInForce | 立即撮合? | 挂簿? | 应用场景 |
|------|----------|-------------|----------|-------|---------|
| 市价单 | Market | — | 是 | 否（余量撤销） | 立即成交，不关心价格 |
| GTC 限价单 | Limit | GTC | 可能 | 是 | 标准挂单，直到撤销 |
| GTD 限价单 | Limit | GTD(ts) | 可能 | 是 | 同 GTC，到期自动过期 |
| IOC 限价单 | Limit | IOC | 是 | 否（余量撤销） | 尽量成交，不挂簿 |
| FOK 限价单 | Limit | FOK | 全部或拒绝 | 否 | 全量成交或全部拒绝 |
| PostOnly | Limit | PostOnly | 否（否则拒绝） | 是 | 做市商专用，只挂不吃 |

### 各类型处理逻辑

**Market（市价单）**：
```
1. 发出 OrderAccepted 事件
2. 执行 match_aggressive()（无价格限制）
3. 如果有剩余量 → 发出 OrderCancelled（市价单永不挂簿）
```

**Limit + GTC/GTD**：
```
1. 发出 OrderAccepted 事件
2. 执行 match_aggressive()（有价格限制）
3. 如果有剩余量 → book.add_order()（挂到订单簿）
4. 如果挂簿失败 → 发出 OrderCancelled
```

**Limit + IOC（Immediate-or-Cancel）**：
```
1. 发出 OrderAccepted 事件
2. 执行 match_aggressive()
3. 如果有剩余量 → 发出 OrderCancelled（不挂簿）
```

IOC 和 Market 的区别：IOC 有价格限制。例如 "IOC Buy 100 @ 50300" 只会和 <= 50300 的卖单成交。

**Limit + FOK（Fill-or-Kill）**：
```
1. 预检查: check_fok_liquidity() — 检查对手方是否有足够流动性
2. 如果不足 → 发出 OrderRejected (InsufficientLiquidity)，结束
3. 如果足够 → 发出 OrderAccepted
4. 执行 match_aggressive()（保证全部成交）
```

FOK 的预检查是**只读**操作——遍历对手方价格层级，累加可成交量，不修改订单簿。只有确认可以全量成交后，才真正执行匹配。

**Limit + PostOnly**：
```
1. 预检查: would_cross() — 检查是否会与对手方成交
2. 如果会成交 → 发出 OrderRejected (PostOnlyWouldTake)，结束
3. 如果不会 → 发出 OrderAccepted
4. book.add_order()（直接挂簿，不撮合）
```

PostOnly 保证订单只做 maker（被动方），不做 taker（主动方）。这对做市商很重要，因为 maker 通常享受更低的手续费（甚至负费率/返佣）。

### 代码中的分派逻辑

```rust
// matcher.rs:45-184
pub fn match_order(&mut self, book: &mut OrderBook, mut order: Order)
    -> SmallVec<[EngineEvent; 8]>
{
    match order.order_type {
        OrderType::Market => { /* 市价单逻辑 */ }
        OrderType::Limit => match order.time_in_force {
            TimeInForce::GTC | TimeInForce::GTD(_) => { /* GTC/GTD 逻辑 */ }
            TimeInForce::IOC => { /* IOC 逻辑 */ }
            TimeInForce::FOK => { /* FOK 逻辑 */ }
            TimeInForce::PostOnly => { /* PostOnly 逻辑 */ }
        },
    }
}
```

使用 `match` 而非 `if-else` 链，编译器会检查穷尽性——如果未来新增 `OrderType` 或 `TimeInForce` 变体，编译器会强制处理新分支。

> **面试考点 4.2**
> - Q: IOC 和 Market 的区别是什么？
> - A: Market 没有价格限制，会以任何价格成交（可能产生巨大滑点）。IOC 有限价保护，超过限价的部分不会成交。两者都不挂簿。
> - Q: 为什么 FOK 需要预检查而不是直接撮合再看结果？
> - A: 如果直接撮合，部分成交后发现不够再回滚，需要复杂的 undo 逻辑（把已成交的订单恢复回去）。预检查是只读的，不修改任何状态，简单且安全。
> - Q: PostOnly 被拒绝时，哪些信息不会泄露给市场？
> - A: PostOnly 拒绝只产生 `OrderRejected` 事件，不包含对手方的具体价格或数量。这防止了探测（probing）攻击——恶意用户不能通过反复提交 PostOnly 订单来推断对手方的精确挂单。

---

## 4.3 撮合主循环

撮合的核心在 `match_aggressive()` 方法中（`matcher.rs:191-249`）。这是价格-时间优先算法的实际实现。

### match_aggressive 逐步解析

```rust
fn match_aggressive(
    &mut self,
    book: &mut OrderBook,
    order: &mut Order,
    events: &mut SmallVec<[EngineEvent; 8]>,
) {
    let is_market = order.order_type == OrderType::Market;

    while !order.remaining_qty.is_zero() {          // ①
        // 获取对手方最优价格
        let best_price = match order.side {          // ②
            Side::Buy => book.best_ask_price(),      //   买单吃卖方
            Side::Sell => book.best_bid_price(),     //   卖单吃买方
        };

        let best_price = match best_price {
            Some(p) => p,
            None => break,                           // ③ 无对手方流动性
        };

        // 限价检查
        if !is_market {                              // ④
            match order.side {
                Side::Buy => { if best_price > order.price { break; } }
                Side::Sell => { if best_price < order.price { break; } }
            }
        }

        // 获取该价格层级的 slab 索引
        let level_key = ...;                         // ⑤

        // 在该价格层级内逐笔撮合
        self.match_at_level(book, order, level_key, best_price, events);  // ⑥

        if order.remaining_qty.is_zero() { break; }
        // 循环回到 ①，检查下一个价格层级
    }
}
```

**控制流图**：

```
                  ┌──────────────┐
                  │ remaining > 0?│
                  └──────┬───────┘
                    yes  │  no → 退出
                         ▼
                  ┌──────────────┐
                  │获取对手方最优价│
                  └──────┬───────┘
                    有   │  无 → 退出（无流动性）
                         ▼
                  ┌──────────────┐
                  │限价检查 (Limit│
                  │orders only)  │
                  └──────┬───────┘
                   通过  │  不通过 → 退出（价格不够）
                         ▼
                  ┌──────────────┐
                  │match_at_level│
                  │(逐笔匹配)    │
                  └──────┬───────┘
                         │
                         ▼
                  回到循环顶部
```

### match_at_level 逐步解析

在单个价格层级内，`match_at_level` 沿着侵入式链表从头到尾依次匹配（`matcher.rs:253-409`）：

```rust
fn match_at_level(&mut self, book, order, level_key, match_price, events) {
    loop {
        if order.remaining_qty.is_zero() { break; }

        // 获取链表头部（最早的订单）
        let head_idx = book.levels()[level_key].head_idx;    // ①
        if head_idx == NO_LINK { break; }                    // ② 层级已空

        let resting_key = head_idx as usize;
        // 读取被动方订单信息
        let resting_account = book.orders()[resting_key].account_id;
        let resting_id = book.orders()[resting_key].id;
        let resting_remaining = book.orders()[resting_key].remaining_qty;

        // STP 检查（详见 4.4）
        let stp_action = check_self_trade(...);              // ③
        match stp_action { /* 处理自成交 */ }

        // 计算成交数量
        let fill_qty = order.remaining_qty.min(resting_remaining);  // ④

        // 更新 taker 剩余量
        order.remaining_qty = order.remaining_qty.saturating_sub(fill_qty);

        // 生成成交事件
        let trade_id = self.next_trade_id();                 // ⑤
        events.push(EngineEvent::Trade { trade: Trade {
            trade_id,
            symbol: order.symbol,
            price: match_price,        // 成交价 = maker 的挂单价
            quantity: fill_qty,
            maker_order_id: resting_id,
            taker_order_id: order.id,
            maker_side: resting_side,
            timestamp: order.timestamp,
        }});

        // 更新或删除 maker 订单
        let new_resting_remaining = resting_remaining.saturating_sub(fill_qty);
        if new_resting_remaining.is_zero() {
            book.remove_order_by_key(resting_key);           // ⑥ 完全成交
        } else {
            // 部分成交：更新剩余量
            book.orders_mut()[resting_key].remaining_qty = new_resting_remaining;
            book.levels_mut()[level_key].total_quantity -= fill_qty;
        }

        // 生成 BookUpdate 事件
        events.push(EngineEvent::BookUpdate { ... });        // ⑦
    }
}
```

### 关键设计决策

1. **成交价取 maker 价格**（`price: match_price`）：match_price 是从 BTreeMap 获取的对手方层级价格，即 maker 的挂单价。

2. **Trade ID 单调递增**：`next_trade_id()` 内部简单 `+= 1`，保证全局唯一。在单线程模型下无需原子操作。

3. **部分成交的原地更新**：当 maker 被部分成交时，不需要删除再插入——直接修改 `remaining_qty` 和 `total_quantity`。这保留了 maker 的时间优先权。

4. **完全成交的直接删除**：调用 `remove_order_by_key`，该方法处理链表摘除、层级清理、BBO 更新等所有善后工作。

### 复杂度分析

| 场景 | 时间复杂度 | 说明 |
|------|-----------|------|
| 未成交（无对手方） | O(1) | 一次 BBO 查找即返回 |
| 单价位成交 K 笔 | O(K) | 遍历链表 K 个节点 |
| 跨 M 个价位成交 | O(M + K) | M 次 BBO 更新 + K 笔成交 |
| FOK 预检查 | O(M) | 遍历 M 个对手方价格层级 |

其中 BBO 更新在层级清空时是 O(log N)，但因为上一层级被清空后 BTreeMap 自动返回下一个最优价，实际上每个层级只需要一次 BTreeMap 查找。

> **面试考点 4.3**
> - Q: `match_aggressive` 中为什么每次循环都要重新获取 `best_price` 而不是预先遍历所有层级？
> - A: 因为 `match_at_level` 可能清空整个价格层级（删除最后一个 maker 时 `remove_order_by_key` 会清理层级并更新 BBO），所以每次循环必须重新查询当前的 best price。BBO 是 O(1) 缓存，所以代价极小。
> - Q: 如果一个 taker 的 IOC 订单可以吃掉 1000 个价格层级上的 10000 个 maker 订单，性能还能保证吗？
> - A: 是的。每笔成交都是 O(1) 的链表操作和 slab 操作。10000 笔成交总共也只是 10000 次常数时间操作。但会生成大量事件（每笔成交一个 Trade + 一个 BookUpdate），SmallVec 在超过 8 个元素后会 spill 到堆上——这是唯一的分配开销。

---

## 4.4 自成交防护 (STP)

### 什么是自成交？

同一个账户（或关联账户）的买单和卖单互相成交叫做自成交（self-trade）。在大多数交易所，自成交是被禁止或限制的，原因包括：

- **洗交易（wash trading）**：通过自己和自己交易制造虚假成交量
- **市场操纵**：影响市场价格而不承担真实风险
- **监管合规**：多数司法管辖区明确禁止

### STP 模式

Tachyon 的 STP 定义在 `crates/tachyon-engine/src/stp.rs`：

```rust
pub enum StpMode {
    None,          // 不做自成交检查
    CancelNewest,  // 取消 taker（新来的订单）
    CancelOldest,  // 取消 maker（簿上的订单）
    CancelBoth,    // 取消双方
}
```

### 检查逻辑

```rust
// stp.rs:29-39
pub fn check_self_trade(
    incoming_account: u64,
    resting_account: u64,
    mode: StpMode,
) -> StpAction {
    // 不同账户 或 STP 关闭 → 继续撮合
    if incoming_account != resting_account || mode == StpMode::None {
        return StpAction::Proceed;
    }
    // 同一账户 + STP 开启 → 根据模式处理
    match mode {
        StpMode::None => StpAction::Proceed,
        StpMode::CancelNewest => StpAction::CancelIncoming,
        StpMode::CancelOldest => StpAction::CancelResting,
        StpMode::CancelBoth => StpAction::CancelBoth,
    }
}
```

### STP 在撮合循环中的位置

STP 检查发生在 `match_at_level` 的每次迭代中，在计算成交量**之前**：

```rust
// matcher.rs:290-357
let stp_action = check_self_trade(order.account_id, resting_account, self.stp_mode);
match stp_action {
    StpAction::Proceed => { /* 继续正常撮合 */ }
    StpAction::CancelIncoming => {
        // 取消 taker，设置 remaining = 0 终止循环
        events.push(EngineEvent::OrderCancelled { ... });
        order.remaining_qty = Quantity::ZERO;
        return;
    }
    StpAction::CancelResting => {
        // 取消该 maker，但 taker 继续匹配下一个
        events.push(EngineEvent::OrderCancelled { ... });
        book.remove_order_by_key(resting_key);
        events.push(EngineEvent::BookUpdate { ... });
        continue;  // 跳过成交，继续下一个 maker
    }
    StpAction::CancelBoth => {
        // 两者都取消
        events.push(EngineEvent::OrderCancelled { ... }); // resting
        book.remove_order_by_key(resting_key);
        events.push(EngineEvent::OrderCancelled { ... }); // incoming
        order.remaining_qty = Quantity::ZERO;
        return;
    }
}
```

### 各模式的行为对比

假设 Account A 在 50300 有一个卖单（maker），现在 Account A 又提交一个买单（taker）@ 50400：

| STP 模式 | Maker (卖单) | Taker (买单) | 成交? | 说明 |
|----------|-------------|-------------|-------|------|
| None | 保留 | 继续匹配 | 是 | 允许自成交 |
| CancelNewest | 保留 | 立即取消 | 否 | 保护簿上订单 |
| CancelOldest | 从簿上移除 | 继续匹配下一个 | 否 | taker 继续找其他 maker |
| CancelBoth | 从簿上移除 | 立即取消 | 否 | 最激进的防护 |

> **面试考点 4.4**
> - Q: CancelOldest 模式下，如果同一账户在多个价格层级都有 maker 订单会怎样？
> - A: taker 会逐层遍历，每遇到同一账户的 maker 就触发 CancelResting（移除 maker、生成 BookUpdate），然后 `continue` 到下一个 maker。如果该层级内下一个 maker 也是同一账户，同样取消。直到遇到不同账户的 maker 才真正成交。
> - Q: STP 检查的性能开销是多少？
> - A: 一次 `u64` 比较（`incoming_account != resting_account`），在不触发时几乎零开销。即使触发，也只是多了一次 `remove_order_by_key`（O(1)）和一两个事件 push。

---

## 4.5 前置风控 (Risk Manager)

### 设计理念

风控在撮合**之前**执行——如果订单不通过风控检查，不会进入撮合循环。这是"fail fast"原则的体现。

```rust
// risk.rs:14-16
pub struct RiskManager {
    config: RiskConfig,
}
```

```rust
// risk.rs:6-11
pub struct RiskConfig {
    pub price_band_pct: u64,     // 价格偏离带（基点），例如 500 = 5%
    pub max_order_qty: Quantity,  // 单笔最大数量
}
```

### 两项风控检查

**1. 最大数量检查（`check_max_qty`）**

```rust
// risk.rs:34-38
fn check_max_qty(&self, order: &Order) -> Result<(), EngineError> {
    if order.quantity > self.config.max_order_qty {
        return Err(EngineError::InvalidQuantity(order.quantity));
    }
    Ok(())
}
```

简单直接——拒绝数量超标的订单，防止"胖手指"（fat finger）错误。

**2. 价格偏离带检查（`check_price_band`）**

```rust
// risk.rs:46-85
fn check_price_band(&self, order: &Order, book: &OrderBook) -> Result<(), EngineError> {
    // 市价单跳过（没有价格）
    if order.order_type == OrderType::Market { return Ok(()); }
    // 无配置则跳过
    if self.config.price_band_pct == 0 { return Ok(()); }
    // 需要双边报价才能计算中间价
    let (bid, ask) = match (book.best_bid_price(), book.best_ask_price()) {
        (Some(b), Some(a)) => (b, a),
        _ => return Ok(()),
    };
    // 中间价 = (bid + ask) / 2
    let mid = Price::new((bid.raw() + ask.raw()) / 2);
    // 偏离度 = |order_price - mid| * 10000 / |mid|
    let deviation_bps = ...;
    if deviation_bps > self.config.price_band_pct {
        return Err(EngineError::PriceOutOfRange);
    }
    Ok(())
}
```

**价格偏离带的工作原理**：

```
假设: best_bid = 50000, best_ask = 50100
       mid = 50050
       price_band_pct = 500 (5%)

允许范围: 50050 * (1 - 5%) = 47547.5 到 50050 * (1 + 5%) = 52552.5

如果提交 Buy @ 60000:
  deviation = |60000 - 50050| / 50050 ≈ 19.88% = 1988 bps
  1988 > 500 → 拒绝 (PriceOutOfRange)
```

这防止了两种情况：
- **胖手指买入**：以远高于市场价的价格买入，造成巨大滑点
- **胖手指卖出**：以远低于市场价的价格卖出

### 风控在调用链中的位置

```
InboundCommand
       │
       ▼
 SymbolEngine::process_command()
       │
       ├── Risk Manager: check()           ◀── 在此拦截
       │     ├── check_max_qty()
       │     └── check_price_band()
       │
       ├── 通过 → Matcher::match_order()
       │
       └── 拒绝 → OrderRejected 事件
```

> **面试考点 4.5**
> - Q: 为什么价格偏离带需要双边报价（best_bid AND best_ask）才生效？
> - A: 中间价 = (bid + ask) / 2。如果只有一侧报价，中间价不可靠——例如空簿时第一个订单无论什么价格都应该被接受。单侧报价可能出现在交易开盘初期或极端行情下，此时不应限制价格。
> - Q: 市价单为什么跳过价格带检查？
> - A: 市价单没有用户指定的价格，它的执行价格取决于对手方的挂单。如果对手方价格在合理范围内（它们在挂单时已经通过了风控），市价单以这些价格成交是安全的。限制市价单没有意义——它的本意就是"以当前市场价立即成交"。

---

## 4.6 SymbolEngine — 协调者模式

### 架构角色

`SymbolEngine` 是每个交易品种的顶层协调者，组合了三个核心组件（`engine.rs:23-30`）：

```rust
pub struct SymbolEngine {
    symbol: Symbol,
    book: OrderBook,        // 订单簿
    matcher: Matcher,       // 撮合器
    risk: RiskManager,      // 风控
    sequence: u64,          // 全局序列号
    next_order_id: u64,     // 下一个订单 ID
}
```

```
                  SymbolEngine
            ┌───────────────────────┐
            │                       │
            │  ┌─────────────────┐  │
Command ──→ │  │  RiskManager    │  │
            │  │  (前置风控)      │  │
            │  └────────┬────────┘  │
            │           │pass       │
            │  ┌────────▼────────┐  │
            │  │  Matcher        │  │
            │  │  (撮合引擎)     │  │──→ SmallVec<[EngineEvent; 8]>
            │  └────────┬────────┘  │
            │           │           │
            │  ┌────────▼────────┐  │
            │  │  OrderBook      │  │
            │  │  (订单簿)       │  │
            │  └─────────────────┘  │
            │                       │
            └───────────────────────┘
```

### 命令处理流程

`process_command()` 是唯一的外部入口（`engine.rs:54-163`）：

```rust
pub fn process_command(
    &mut self,
    cmd: Command,
    account_id: u64,
    timestamp: u64,
) -> SmallVec<[EngineEvent; 8]> {
    self.sequence += 1;

    match cmd {
        Command::PlaceOrder(mut order) => {
            // 1. 分配订单 ID（如果未提供）
            if order.id.raw() == 0 {
                order.id = OrderId::new(self.next_order_id);
                self.next_order_id += 1;
            }
            // 2. 填充元数据
            order.account_id = account_id;
            order.timestamp = timestamp;
            order.symbol = self.symbol;
            // 3. 风控检查
            if let Err(err) = self.risk.check(&order, &self.book) {
                return /* OrderRejected */;
            }
            // 4. 撮合
            self.matcher.match_order(&mut self.book, order)
        }
        Command::CancelOrder(order_id) => { /* 撤单 */ }
        Command::ModifyOrder { .. } => { /* 改单 */ }
    }
}
```

### 订单 ID 生成策略

```rust
// engine.rs:41-48
let id_base = (symbol.raw() as u64) << 40;
// ...
next_order_id: id_base + 1,
```

订单 ID 将 symbol ID 编码在高 24 位，保证**跨品种全局唯一**：

```
OrderId 位布局 (64 bit):
┌──────────────────┬──────────────────────────────────┐
│ symbol (24 bits) │        序列号 (40 bits)           │
└──────────────────┴──────────────────────────────────┘

例如: Symbol(1) 的第 3 个订单
  = (1 << 40) + 3
  = 1_099_511_627_779

40 bits 支持 ~1 万亿个订单/品种，24 bits 支持 ~1600 万个品种
```

### 三种命令

```rust
pub enum Command {
    PlaceOrder(Order),                // 下单
    CancelOrder(OrderId),             // 撤单
    ModifyOrder {                     // 改单（cancel + re-insert）
        order_id: OrderId,
        new_price: Option<Price>,
        new_qty: Option<Quantity>,
    },
}
```

**ModifyOrder 的实现**：先 `cancel_order`，再以新参数 `add_order`。修改后的订单会**失去时间优先权**。这是有意为之——如果允许"免费修改"保留优先权，做市商可以不断调整价格而永远占据队列前端。

> **面试考点 4.6**
> - Q: 为什么一个 Symbol 对应一个独立的 SymbolEngine，而不是所有 Symbol 共享一个引擎？
> - A: (1) 隔离性——一个品种的故障不影响其他品种；(2) 并行性——不同品种的引擎可以在不同 CPU 核心上并发运行（每核一个 SymbolEngine）；(3) 简洁性——无需跨品种加锁，单线程处理单品种即可。
> - Q: `next_order_id` 为什么不用 AtomicU64？
> - A: SymbolEngine 被设计为单线程访问（per-symbol-per-core），不需要原子操作。避免原子操作的缓存行同步开销，在高频场景下这很重要。

---

## 4.7 事件生成机制

### 事件驱动架构

Tachyon 的撮合引擎是**纯函数式**的——给定相同的输入（Command），总是产生相同的输出（`SmallVec<[EngineEvent; 8]>`）。所有状态变化都通过事件表达：

```rust
pub enum EngineEvent {
    OrderAccepted { order_id, symbol, side, price, qty, timestamp },
    OrderRejected { order_id, reason, timestamp },
    OrderCancelled { order_id, remaining_qty, timestamp },
    OrderExpired { order_id, timestamp },
    Trade { trade: Trade },
    BookUpdate { symbol, side, price, new_total_qty, timestamp },
}
```

### 为什么是 SmallVec<[EngineEvent; 8]>？

```rust
SmallVec<[EngineEvent; 8]>
```

`SmallVec` 是 `Vec` 的优化替代品，当元素数量不超过内联容量（这里是 8）时，数据存储在栈上，**零堆分配**：

```
SmallVec<[T; 8]> 的内存布局:

当 len <= 8 (内联模式):
┌──────────────────────────────────────────┐
│ [T; 8]  (栈上数组)        │ len │ 标志位 │
└──────────────────────────────────────────┘
  完全在栈上，零堆分配

当 len > 8 (溢出模式):
┌─────────────────────────────────────┐
│ *ptr (堆指针) │ len │ cap │ 标志位  │
└────────┬────────────────────────────┘
         │
         ▼ 堆上
┌──────────────────────┐
│ T T T T T T T T T T  │
└──────────────────────┘
```

**为什么选 8？**

分析一个典型的 GTC Limit Order 的事件序列：

| 事件 # | 事件类型 | 场景 |
|--------|---------|------|
| 1 | OrderAccepted | 总是第一个 |
| 2 | Trade | 与第 1 个 maker 成交 |
| 3 | BookUpdate | 第 1 个 maker 的价格层级更新 |
| 4 | Trade | 与第 2 个 maker 成交 |
| 5 | BookUpdate | 第 2 个 maker 的价格层级更新 |
| 6 | Trade | 与第 3 个 maker 成交 |
| 7 | BookUpdate | 第 3 个 maker 的价格层级更新 |

一个订单匹配 3 个 maker 会产生 7 个事件，刚好在内联容量内。大多数撮合在 1-3 个 maker 内完成（特别是限价单），所以 8 覆盖了绝大多数场景。

### 事件顺序的一致性

每种订单类型都遵循固定的事件序列：

```
Market 订单:
  OrderAccepted → [Trade + BookUpdate]* → OrderCancelled (如果有余量)

GTC/GTD 限价单:
  OrderAccepted → [Trade + BookUpdate]* → (挂簿或OrderCancelled)

IOC 限价单:
  OrderAccepted → [Trade + BookUpdate]* → OrderCancelled (如果有余量)

FOK 限价单:
  OrderRejected                          (流动性不足)
  或
  OrderAccepted → [Trade + BookUpdate]*  (全量成交)

PostOnly 限价单:
  OrderRejected                          (会穿越价差)
  或
  OrderAccepted                          (成功挂簿)

撤单:
  OrderCancelled                         (成功)
  或
  OrderRejected (OrderNotFound)          (订单不存在)
```

### 事件溯源 (Event Sourcing) 的基础

这些事件不仅用于通知外部系统，还是 Tachyon 实现**确定性重放**和**崩溃恢复**的基础：

1. **确定性重放**：给定相同的 Command 序列，引擎总是生成完全相同的 Event 序列
2. **WAL 持久化**：每个 Command 被写入预写日志（WAL），崩溃后重放即可恢复状态
3. **审计追踪**：完整的事件流可以重建任意时刻的订单簿快照

```
Commands (WAL)              Engine              Events
┌─────────────┐        ┌─────────────┐        ┌─────────────┐
│ PlaceOrder 1│───────→│             │───────→│ Accepted    │
│ PlaceOrder 2│───────→│ SymbolEngine│───────→│ Trade       │
│ CancelOrder │───────→│             │───────→│ BookUpdate  │
│ PlaceOrder 3│───────→│  (纯函数)   │───────→│ Cancelled   │
└─────────────┘        └─────────────┘        └─────────────┘
       │                                             │
       ▼                                             ▼
  持久化到磁盘                                 推送给网关/客户端
```

> **面试考点 4.7**
> - Q: 为什么用 `SmallVec<[EngineEvent; 8]>` 而不是 `Vec<EngineEvent>`？
> - A: 零分配的关键优化。在撮合热路径上，每个订单都要创建一个事件容器。Vec 每次都要堆分配（至少 1 次 malloc），而 SmallVec 在大多数情况下（<= 8 个事件）完全在栈上。对于 <100ns 的目标延迟，一次 malloc/free (~50ns) 是不可接受的开销。
> - Q: `EngineEvent` 里为什么 `Trade` 是包装在 `Trade` 结构体中的，而其他事件是内联字段？
> - A: `Trade` 结构体有 8 个字段，如果内联到枚举变体会让枚举过大。封装成独立结构也方便独立序列化（存入交易历史数据库）和传输。
> - Q: 如果引擎在生成事件的中途崩溃（例如处理了 Trade 但还没 BookUpdate），如何恢复？
> - A: 恢复不依赖事件而依赖 Command 重放。WAL 记录的是输入（Command），不是输出（Event）。重放时引擎会重新生成完整的事件序列，所以不存在"半截事件"的问题。

---

## 本章小结

| 组件 | 文件 | 核心职责 |
|------|------|---------|
| `Matcher` | `matcher.rs` | 价格-时间优先撮合，STP 检查 |
| `OrderBook` | `book.rs` | 三层索引存储，BBO 缓存 |
| `RiskManager` | `risk.rs` | 前置风控（数量上限 + 价格偏离带） |
| `SymbolEngine` | `engine.rs` | 协调者，组合上述三个组件 |
| `StpMode` | `stp.rs` | 自成交防护策略 |
| `Command` | `command.rs` | 引擎输入（PlaceOrder / CancelOrder / ModifyOrder） |
| `EngineEvent` | `event.rs` | 引擎输出（Accepted / Trade / BookUpdate / ...） |

**核心复杂度**：

| 操作 | 时间复杂度 | 空间复杂度 |
|------|-----------|-----------|
| 下单（无成交，挂簿） | O(log N) | O(1) 均摊 |
| 下单（成交 K 笔） | O(K + log N) | O(K) 事件 |
| 撤单 | O(1) 均摊 | O(1) |
| 改单 | O(log N) | O(1) |
| FOK 预检查 | O(M) | O(1) |
| BBO 查询 | O(1) | O(1) |

其中 N = 价格层级数，K = 成交笔数，M = 需要检查的价格层级数。

---

> **下一章预告**：[第五章：并发模型 — 无锁队列与核间通信](./05-concurrency.md) 将深入 `tachyon-io` 的 SPSC/MPSC 无锁队列设计，以及 Tachyon 如何通过 per-core 架构避免锁竞争。
