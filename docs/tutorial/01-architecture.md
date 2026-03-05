# 第1章：系统架构总览

[<< 返回目录](./README.md) | [下一章：核心类型 >>](./02-core-types.md)

---

## 1.1 Crate 依赖图

Tachyon 采用 Rust workspace 组织，9 个 crate 各司其职，依赖关系清晰：

```
                          ┌──────────────────┐
                          │  tachyon-server   │  bin: 入口、线程编排、配置
                          └────────┬─────────┘
                                   │
             ┌─────────────────────┼──────────────────────┐
             │                     │                      │
    ┌────────▼────────┐  ┌────────▼────────┐   ┌─────────▼────────┐
    │ tachyon-gateway  │  │ tachyon-persist │   │  tachyon-bench   │
    │ REST/WS/TCP/     │  │ WAL + Snapshot  │   │  性能基准测试     │
    │ Bridge           │  │ + Recovery      │   │                  │
    └────────┬─────────┘  └────────┬────────┘   └──────────────────┘
             │                     │
    ┌────────▼────────┐            │
    │ tachyon-engine   │◄──────────┘
    │ Matcher + STP +  │
    │ RiskManager      │
    └────────┬─────────┘
             │
    ┌────────▼────────┐
    │  tachyon-book    │
    │  OrderBook       │
    │  (BTreeMap+Slab) │
    └────────┬─────────┘
             │
    ┌────────▼────────┐     ┌──────────────────┐
    │  tachyon-core    │     │   tachyon-io      │
    │  Price, Quantity,│     │   SPSC + MPSC     │
    │  Order, Event    │     │   无锁队列         │
    └─────────────────┘     └──────────────────┘
```

**依赖规则**：箭头方向为"依赖"。下层 crate 永远不依赖上层。`tachyon-core` 和 `tachyon-io` 是叶子 crate，互不依赖。

### 为什么这样拆分

| crate | 职责 | 为什么独立 |
|-------|------|-----------|
| `tachyon-core` | 基础类型定义 | 被所有 crate 共享，零外部依赖（仅 serde） |
| `tachyon-io` | 无锁队列 | 通用并发原语，可独立复用于任何项目 |
| `tachyon-book` | 订单簿数据结构 | 纯数据结构，不含撮合逻辑 |
| `tachyon-engine` | 撮合逻辑 | 纯业务逻辑，不含 I/O 或网络 |
| `tachyon-gateway` | 网络层 | 异步 I/O (tokio)，与同步引擎解耦 |
| `tachyon-persist` | 持久化 | 可选功能，不影响核心撮合路径 |
| `tachyon-server` | 入口与编排 | 组装所有组件，配置管理 |

> **面试考点**
>
> **Q: 为什么撮合引擎不直接包含网络层？**
>
> A: 关注点分离 (Separation of Concerns)。网络层是异步 I/O (tokio runtime)，撮合引擎是同步的纯计算。
> 混在一起会让异步运行时的调度影响撮合延迟的确定性。通过 SPSC 队列将两个世界桥接，
> 网关线程 (async) 作为 producer，引擎线程 (sync) 作为 consumer，引擎线程永远不会被 tokio 调度器抢占。

---

## 1.2 数据流 {#数据流}

一个订单从客户端到成交的完整路径：

```
客户端 (REST/WS/TCP)
    │
    ▼
┌───────────────────────────────────────────┐
│             tachyon-gateway               │
│                                           │
│  REST Handler / WS Handler / TCP Handler  │
│         │                                 │
│         ▼                                 │
│    EngineBridge                           │
│    ├─ 根据 Symbol 查找对应 SPSC 队列       │
│    ├─ 创建 oneshot channel (response_tx)  │
│    └─ try_push(EngineCommand) 到 SPSC     │
└────────────┬──────────────────────────────┘
             │  SPSC Queue (wait-free, 无锁)
             ▼
┌───────────────────────────────────────────┐
│          engine thread (per-symbol)       │
│                                           │
│  loop {                                   │
│      drain_batch(&mut batch)  // 批量出队  │
│      for cmd in batch {                   │
│          WAL.append(cmd)      // 先写日志  │
│          engine.process(cmd)  // 再撮合    │
│          response_tx.send(events)         │
│      }                                    │
│  }                                        │
└───────────────────────────────────────────┘
             │
             ▼
     events: SmallVec<[EngineEvent; 8]>
     ├── OrderAccepted  → 更新 order_registry
     ├── Trade          → 推送 WebSocket + 写 TradeStore
     ├── OrderCancelled → 更新 order_registry
     ├── OrderRejected  → 指标计数
     └── BookUpdate     → 更新深度快照 + 推送 WS
```

### 关键路径分析

| 阶段 | 位置 | 耗时量级 | 是否分配内存 |
|------|------|---------|-------------|
| HTTP 解析 | gateway (async) | ~us | 是 (JSON 反序列化) |
| SPSC 入队 | gateway → bridge | ~ns | 否 (wait-free push) |
| WAL 写入 | engine thread | ~us | 否 (预分配 buffer) |
| 撮合匹配 | engine thread | **<100ns** | 否 (SmallVec 栈分配) |
| 结果回传 | oneshot channel | ~ns | 否 |
| WS 推送 | broadcast channel | ~us | 是 (序列化) |

**热路径**（hot path）是从 SPSC 出队到撮合完成的这段，目标是完全零分配。
**冷路径**（cold path）包括 JSON 序列化、WebSocket 推送等，允许分配但不影响撮合延迟。

> **面试考点**
>
> **Q: 为什么用 SPSC 而不是 MPSC 或 channel？**
>
> A: 每个品种 (symbol) 有独立的引擎线程，该线程是唯一的消费者。
> SPSC 只需要两个原子变量 (head/tail)，每次操作仅需一个 Release/Acquire store/load，
> 是理论最优的线程间通信原语。MPSC 需要 CAS 循环，channel 需要互斥锁和条件变量，都更慢。

---

## 1.3 线程模型 {#线程模型}

```
┌──────────────────────────────────────────────────────────┐
│                     tokio runtime                        │
│                                                          │
│  ┌──────────┐  ┌──────────┐  ┌──────────┐  ┌─────────┐  │
│  │REST task 1│  │REST task 2│  │WS task 1 │  │TCP task │  │
│  └─────┬────┘  └────┬─────┘  └────┬─────┘  └────┬────┘  │
│        └──────┬─────┴──────┬──────┘              │       │
│               ▼            ▼                     ▼       │
│         EngineBridge (per-symbol SPSC queues)            │
└───────────┬────────────────┬─────────────────────────────┘
            │                │
    ┌───────▼──────┐ ┌──────▼───────┐
    │ engine-0     │ │ engine-1     │    ... (每个品种一个 OS 线程)
    │ (BTC/USDT)   │ │ (ETH/USDT)  │
    │ pinned to    │ │ pinned to    │
    │ core 1       │ │ core 2       │
    └──────────────┘ └──────────────┘
```

### 线程分类

| 线程类型 | 数量 | 调度方式 | CPU 绑核 | 说明 |
|---------|------|---------|---------|------|
| tokio worker | N (默认=CPU核数) | 协作式 | 否 | 处理 REST/WS/TCP I/O |
| engine thread | 每个品种 1 个 | 自旋轮询 | 是 (round-robin) | 热路径：撮合 + WAL |
| trade writer | 1 | tokio spawn | 否 | 异步批量写 TradeStore |

### Engine 线程的轮询策略

```rust
// bin/tachyon-server/src/main.rs:610-637
loop {
    let mut batch = ArrayVec::<EngineCommand, 64>::new();
    queue.drain_batch(&mut batch);

    if batch.is_empty() {
        if shutdown.load(Ordering::Acquire) {
            break;  // 优雅退出
        }
        // 渐进式退避：先自旋，再 yield
        spin_count += 1;
        if spin_count < 1000 {
            std::hint::spin_loop();  // CPU hint: 降低功耗
        } else {
            std::thread::yield_now();  // 让出 CPU
            spin_count = 0;
        }
        continue;
    }
    // ... process batch
}
```

**为什么不用条件变量 (Condvar) 等待？**

条件变量涉及 Mutex 锁定和内核态切换，唤醒延迟在 ~1-10us。
自旋轮询的唤醒延迟是 ~0ns（CPU 已经在检查了），代价是在空闲时会消耗一个 CPU 核心。
对于撮合引擎这种延迟敏感场景，用一个核心的 idle CPU 时间换取零唤醒延迟是值得的。

**渐进式退避 (Progressive Backoff)** 是折中方案：前 1000 次用 `spin_loop()`（只是暗示 CPU 降低功耗），
超过后用 `yield_now()` 让出时间片，避免在完全空闲时浪费电力。

> **面试考点**
>
> **Q: 如果有 100 个交易品种，就需要 100 个 CPU 核心吗？**
>
> A: 不一定。活跃品种（如 BTC/USDT, ETH/USDT）分配独立核心，冷门品种可以共享核心
> （通过修改轮询策略为 Condvar 等待）。实际生产中，头部 5-10 个品种占据 95%+ 的交易量。
> Tachyon 的 per-symbol 线程模型保证了热门品种之间互不干扰。

---

## 1.4 设计哲学 {#设计哲学}

### 原则一：零分配热路径

热路径（SPSC 出队 → 撮合 → 结果回传）不允许任何堆内存分配：

| 组件 | 策略 | 避免了什么 |
|------|------|-----------|
| 撮合结果 | `SmallVec<[EngineEvent; 8]>` | 大多数订单产生 <8 个事件，栈上完成 |
| SPSC 批量出队 | `ArrayVec<EngineCommand, 64>` | 固定大小栈数组，不 `Vec::push` |
| 订单存储 | `Slab<Order>` | 预分配、复用槽位，不 `Box::new` |
| 价格级别 | `Slab<PriceLevel>` | 同上 |
| 订单索引 | `hashbrown::HashMap` | 比 `std::HashMap` 更少分配，更好的 cache 行为 |

### 原则二：Wait-free 关键路径

| 操作 | 实现 | 保证 |
|------|------|------|
| SPSC push/pop | 单原子 store/load | wait-free（有限步完成） |
| Slab insert/remove | 索引数组操作 | O(1)，无锁 |
| BBO 查询 | 缓存 `Option<Price>` | O(1)，无需遍历 |

引擎线程内部是**完全单线程**的——无需任何同步原语。线程间通信全部通过 SPSC 队列，
只有 `head`/`tail` 两个原子变量参与同步。

### 原则三：事件溯源 (Event Sourcing)

所有状态变化都表达为 `EngineEvent` 枚举：

```rust
// crates/tachyon-core/src/event.rs:23-56
pub enum EngineEvent {
    OrderAccepted { order_id, symbol, side, price, qty, timestamp },
    OrderRejected { order_id, reason, timestamp },
    OrderCancelled { order_id, remaining_qty, timestamp },
    OrderExpired { order_id, timestamp },
    Trade { trade: Trade },
    BookUpdate { symbol, side, price, new_total_qty, timestamp },
}
```

好处：
1. **确定性重放**：相同的 Command 序列 → 相同的 Event 序列 → 相同的最终状态
2. **崩溃恢复**：从 WAL 重放 Command，引擎自动重建到崩溃前的状态
3. **审计追踪**：每个 Trade、Cancel、Reject 都有完整记录
4. **实时推送**：Event 直接转发到 WebSocket 广播

### 原则四：异步与同步分离

```
    异步世界 (tokio)          │        同步世界 (OS thread)
    ─────────────────         │        ─────────────────────
    REST/WS/TCP I/O           │        撮合引擎
    JSON 序列化/反序列化       │        纯计算
    允许分配，允许 await       │        零分配，零等待
    延迟不敏感 (~ms)          │        延迟极敏感 (<100ns)
                              │
    ◄───── SPSC Queue ──────►
```

两个世界通过 SPSC 队列桥接：
- **Gateway → Engine**：`EngineCommand` 通过 SPSC push
- **Engine → Gateway**：`Vec<EngineEvent>` 通过 `oneshot::Sender` 回传

`oneshot` channel 是一次性的，创建和发送都是 O(1) 且几乎零开销。

> **面试考点**
>
> **Q: 为什么 response 用 tokio::oneshot 而不也用 SPSC？**
>
> A: SPSC 是持久性队列（一直存在），适合高频单向消息流。
> Response 是 request-response 模式，每个请求需要一个独立的回传通道。
> oneshot channel 正好是为此设计的：创建开销极小，只传递一次值后自动销毁，
> 语义上也更清晰（每个请求有独立的 future 可以 await）。

---

## 1.5 编译优化配置

```toml
# Cargo.toml (workspace)
[profile.release]
lto = "fat"          # 全链接时优化 — 跨 crate 内联
codegen-units = 1    # 单编译单元 — 最大优化机会
panic = "abort"      # 不生成 unwind 表 — 更小二进制、更快
strip = true         # 去除符号表 — 更小二进制

[profile.bench]
inherits = "release"
debug = true         # bench profile 保留调试信息用于 perf 分析
```

| 选项 | 效果 | 代价 |
|------|------|------|
| `lto = "fat"` | 跨 crate 内联，消除函数调用开销 | 编译时间 2-5x |
| `codegen-units = 1` | LLVM 能看到所有代码，全局优化 | 编译时间 1.5-2x |
| `panic = "abort"` | 无需 unwind 表，减小代码体积 | panic 时直接终止，无法 catch |
| `strip = true` | 去除 `.debug` 和 `.symtab` | 无法 `perf record` 看函数名 |

**全局分配器**替换为 mimalloc：

```rust
// bin/tachyon-server/src/main.rs:31-32
#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;
```

mimalloc 相比系统 malloc 在小对象频繁分配/释放场景下快 2-3x，
对于冷路径上的 JSON 序列化等操作有明显提升。

---

## 1.6 Workspace 依赖管理

所有外部依赖统一在 workspace 根 `Cargo.toml` 声明版本：

```toml
[workspace.dependencies]
slab = "0.4"
hashbrown = "0.14"
smallvec = { version = "1.13", features = ["const_generics", "union"] }
arrayvec = "0.7"
tokio = { version = "1", features = ["full"] }
axum = { version = "0.7", features = ["ws", "json"] }
# ...
```

各 crate 的 `Cargo.toml` 只需写 `slab.workspace = true`，保证版本一致性。

关键依赖分类：

| 类别 | 依赖 | 用途 | 在热路径上？ |
|------|------|------|------------|
| 核心数据结构 | `slab`, `hashbrown`, `smallvec`, `arrayvec` | 订单存储、结果缓冲 | 是 |
| 序列化 | `serde`, `serde_json` | 类型定义、REST JSON | 仅冷路径 |
| 异步 | `tokio`, `axum` | 网关 I/O | 仅冷路径 |
| 持久化 | `rkyv`, `lz4_flex`, `crc32fast` | WAL、快照 | 引擎线程但非撮合 |
| 分配器 | `mimalloc` | 全局分配器替换 | 冷路径受益 |
| 并发 | `crossbeam-utils`, `core_affinity` | CPU 绑核 | 初始化阶段 |

> **面试考点**
>
> **Q: 为什么热路径上不用 tokio？**
>
> A: tokio 是协作式调度器，任务在 `.await` 点才能被调度。撮合引擎是纯计算（不含 I/O），
> 不需要也不应该被 tokio 调度。如果将撮合逻辑放在 tokio task 中：
> 1. 其他 I/O 任务可能抢占 CPU 时间
> 2. `spawn_blocking` 有线程切换开销
> 3. 无法做 CPU 绑核
>
> 使用独立 OS 线程 + 自旋轮询是延迟最低的方案。

---

[<< 返回目录](./README.md) | [下一章：核心类型 >>](./02-core-types.md)
