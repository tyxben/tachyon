# 第六章：性能工程 — 从纳秒到微秒的极致优化

> **前置知识**：第五章无锁并发，Rust 基本类型系统和所有权概念。
>
> **本章目标**：掌握从硬件到编译器的全栈性能优化技术，理解 Tachyon 如何实现 <100ns 中位延迟。

---

## 目录

- [6.1 CPU 缓存层次](#61-cpu-缓存层次)
- [6.2 内存布局优化](#62-内存布局优化)
- [6.3 避免堆分配](#63-避免堆分配)
- [6.4 分配器选择](#64-分配器选择)
- [6.5 CPU 亲和性](#65-cpu-亲和性)
- [6.6 NUMA 架构](#66-numa-架构)
- [6.7 编译器优化](#67-编译器优化)
- [6.8 基准测试方法论](#68-基准测试方法论)
- [6.9 系统调优清单](#69-系统调优清单)

---

## 6.1 CPU 缓存层次

### 内存延迟金字塔

理解性能优化的第一步是记住这张延迟表。每一级缓存的访问速度差异约 3-10 倍：

```
┌──────────────┐
│   寄存器      │  ~0.3ns    ← 最快，但只有几十个
├──────────────┤
│   L1 Cache   │  ~1ns      32-64KB/核心，命中率目标 >95%
├──────────────┤
│   L2 Cache   │  ~3-5ns    256KB-1MB/核心
├──────────────┤
│   L3 Cache   │  ~10-20ns  共享，8-64MB
├──────────────┤
│   主内存 RAM  │  ~50-100ns
├──────────────┤
│   NVMe SSD   │  ~10-25μs  ← 比 RAM 慢 100-250 倍
├──────────────┤
│   网络        │  ~50-500μs  （数据中心内 RTT）
└──────────────┘
```

### 缓存行 — 数据传输的最小单位

CPU 不按字节读取内存，而是按 **缓存行（cache line）** 为单位：

```
缓存行大小：
  x86/x86_64:    64 字节
  Apple Silicon:  128 字节
  AWS Graviton:   64 字节

当你读取一个 8 字节的 u64 时：
  CPU 实际加载了 64 字节的缓存行到 L1
  如果相邻的数据也在同一缓存行中 → 免费获得（空间局部性）
```

### 对撮合引擎的意义

Tachyon 的目标延迟 <100ns，大约是 **一次 L3 缓存访问** 的时间。这意味着：

- 热路径上的所有数据必须在 L1/L2 缓存中。
- 任何缓存未命中（cache miss）都可能让延迟翻倍。
- 数据结构的内存布局直接决定缓存行利用率。

```
一次撮合操作的延迟预算（<100ns）：

BTreeMap 查找最佳价格:      ~10-20ns  (L1 命中)
Slab 读取 Order:            ~5ns      (L1 命中)
更新 remaining_qty:         ~1ns      (L1 命中)
HashMap 查找 order_id:      ~10-20ns  (L1/L2 命中)
更新 BBO 缓存:              ~2ns      (L1 命中)
SmallVec push 事件:         ~5ns      (栈上, L1 命中)
SPSC Release store:         ~1ns      (x86 MOV)
──────────────────────────────────────
总计:                        ~35-55ns  ✓ 在预算内
```

> **面试考点**
>
> Q: 为什么高频交易系统关心 L1 缓存命中率？
>
> A: L1 miss 到 L2 需要 ~3-5ns 额外延迟，到 L3 需要 ~10-20ns，到主内存需要 ~100ns。在撮合路径上如果有 2-3 次 L3 miss，延迟就会从 50ns 飙升到 250ns+，超出 100ns 的目标。高频交易中，延迟的确定性（低抖动）甚至比平均延迟更重要。

---

## 6.2 内存布局优化

### #[repr(C)] 与字段排序

Rust 默认的 `#[repr(Rust)]` 允许编译器自由重排字段以优化大小。但这意味着布局不可预测。`#[repr(C)]` 按声明顺序排列字段，让我们完全控制内存布局。

Tachyon 的 `Order` 结构体是精心设计的（`crates/tachyon-core/src/order.rs`）：

```rust
#[repr(C)]
pub struct Order {
    // === 8 字节对齐字段（48 字节）===
    pub id: OrderId,          // u64, 偏移 0
    pub price: Price,         // i64, 偏移 8
    pub quantity: Quantity,   // u64, 偏移 16
    pub remaining_qty: Quantity, // u64, 偏移 24
    pub timestamp: u64,       // u64, 偏移 32
    pub account_id: u64,      // u64, 偏移 40

    // === 16 字节枚举（tag + u64 payload）===
    pub time_in_force: TimeInForce, // 偏移 48

    // === 4 字节对齐字段（12 字节）===
    pub symbol: Symbol,       // u32, 偏移 64
    pub prev: u32,            // u32, 偏移 68
    pub next: u32,            // u32, 偏移 72

    // === 1 字节字段（2 字节 + 2 字节 padding = 4 字节）===
    pub side: Side,           // u8, 偏移 76
    pub order_type: OrderType, // u8, 偏移 77
    // 2 字节 padding 到 80 字节总大小
}
// 总大小：80 字节 = 1.25 个缓存行（x86）
```

### 字段排序原则

**按对齐从大到小排列**，避免内部 padding：

```
好的排列（Tachyon 的做法）：          差的排列（反面教材）：
┌─────────────────────────────┐    ┌─────────────────────────────┐
│ id: u64         (8B)        │    │ side: u8        (1B)        │
│ price: i64      (8B)        │    │ [7B padding]                │ ← 浪费！
│ quantity: u64   (8B)        │    │ id: u64         (8B)        │
│ remaining: u64  (8B)        │    │ order_type: u8  (1B)        │
│ timestamp: u64  (8B)        │    │ [3B padding]                │ ← 浪费！
│ account_id: u64 (8B)        │    │ symbol: u32     (4B)        │
│ tif: enum       (16B)       │    │ price: i64      (8B)        │
│ symbol: u32     (4B)        │    │ ...                         │
│ prev: u32       (4B)        │    └─────────────────────────────┘
│ next: u32       (4B)        │    总大小可能 96B+（多了 >16B padding）
│ side: u8        (1B)        │
│ order_type: u8  (1B)        │
│ [2B padding]                │
└─────────────────────────────┘
总大小：80B（最小 padding）
```

### 热/冷字段分离

热路径上频繁访问的字段应该在同一缓存行中。Tachyon 的 `Order` 前 64 字节（恰好一个 x86 缓存行）包含了撮合最常用的字段：

```
缓存行 0（偏移 0-63）← 热路径！
  id, price, quantity, remaining_qty, timestamp, account_id, tif
  → 撮合时每次都读

缓存行 1（偏移 64-79）← 较冷
  symbol, prev, next, side, order_type
  → 只在遍历链表或识别方向时读
```

> **面试考点**
>
> Q: 为什么 `#[repr(C)]` 比 `#[repr(Rust)]` 更适合性能关键结构体？
>
> A: `#[repr(Rust)]` 允许编译器任意重排字段，不同编译器版本可能产生不同布局。`#[repr(C)]` 保证按声明顺序排列，开发者可以精确控制哪些字段共享同一缓存行，实现可预测的内存访问模式。

---

## 6.3 避免堆分配

### 堆分配的隐藏成本

```
栈分配：      ~0ns    （编译时确定，调整 RSP 即可）
堆分配：      ~25-80ns （调用 malloc，可能涉及系统调用）
堆释放：      ~25-80ns （调用 free，可能触发 coalescing）
```

在 100ns 延迟预算内，一次 `malloc + free` 就可能消耗一半预算。

### Tachyon 的零分配策略

**策略 1：SmallVec — 小数组栈上，大数组堆上**

```rust
use smallvec::SmallVec;

// 撮合结果：大多数订单产生 <8 个事件
// SmallVec<[EngineEvent; 8]> 在栈上存储 8 个元素
// 超过 8 个时自动 spill 到堆上（极罕见）
let events: SmallVec<[EngineEvent; 8]> = engine.match_order(order);

// 内存布局：
// 常见情况（<=8 事件）：
// ┌──── 栈 ────────────────────────────────────┐
// │ len=3 │ event0 │ event1 │ event2 │ ... │    │
// └────────────────────────────────────────────┘
// 零堆分配！

// 罕见情况（>8 事件，如 iceberg 订单穿越多个价位）：
// ┌──── 栈 ──────┐    ┌──── 堆 ─────────────────────┐
// │ len=15 │ ptr ─┼───→│ event0 │ event1 │ ... │ event14 │
// └────────────────┘    └─────────────────────────────────┘
// 一次堆分配（但几乎不发生）
```

**策略 2：ArrayVec — 固定容量，编译时保证无堆分配**

```rust
use arrayvec::ArrayVec;

// SPSC 批量操作：最多 64 个命令
const BATCH_SIZE: usize = 64;
let mut batch = ArrayVec::<EngineCommand, BATCH_SIZE>::new();
queue.drain_batch(&mut batch);

// ArrayVec 保证：
// - 容量 = 64，编译时确定
// - 超过容量 → panic（而非 spill 到堆）
// - 完全在栈上，零堆分配
```

**策略 3：Slab — 预分配的对象池**

```rust
use slab::Slab;

// Slab<Order> 预分配连续内存块
// insert() 和 remove() 重用已释放的 slot
// 不调用 malloc/free — 内部通过空闲链表管理

let mut orders: Slab<Order> = Slab::with_capacity(1_000_000);

let key = orders.insert(order);   // O(1)，重用空闲 slot
let order = &orders[key];         // O(1)，直接索引
orders.remove(key);               // O(1)，归还到空闲链表
```

Slab 的连续内存布局还带来缓存友好性：相邻的订单存储在相邻的内存地址，提高空间局部性。

**策略 4：预分配 + 重用**

```rust
// SPSC 队列在构造时一次性分配所有 slot
pub fn new(capacity: usize) -> Self {
    let buffer: Vec<UnsafeCell<MaybeUninit<T>>> = (0..capacity)
        .map(|_| UnsafeCell::new(MaybeUninit::uninit()))
        .collect();
    // 之后的 try_push/try_pop 永远不分配/释放内存
}
```

### 分配跟踪

在 Phase 5 优化中，可以使用 counting allocator 验证热路径上的零分配：

```rust
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

static ALLOC_COUNT: AtomicUsize = AtomicUsize::new(0);

struct CountingAllocator;

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOC_COUNT.fetch_add(1, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

// 在热路径前后检查：
let before = ALLOC_COUNT.load(Ordering::Relaxed);
engine.process_command(cmd);
let after = ALLOC_COUNT.load(Ordering::Relaxed);
assert_eq!(before, after, "热路径上发生了堆分配！");
```

> **面试考点**
>
> Q: SmallVec 和 ArrayVec 的区别是什么？什么时候用哪个？
>
> A: SmallVec 可以在超过栈上容量时 spill 到堆上（灵活但可能分配），ArrayVec 永远不分配堆内存（超出容量直接 panic）。在确定上界时用 ArrayVec（如 SPSC 批量大小），在大部分情况小但偶尔可能大时用 SmallVec（如撮合结果事件数）。

---

## 6.4 分配器选择

### 为什么替换系统分配器？

系统默认的 `malloc`（glibc/macOS libmalloc）是通用设计，不针对低延迟场景优化。主要问题：

1. **全局锁**：多线程争用时需要获取锁。
2. **碎片化**：长时间运行后内存碎片导致分配变慢。
3. **syscall**：大块分配可能调用 `mmap`/`brk`，涉及内核态切换。

### mimalloc — Tachyon 的选择

```rust
// bin/tachyon-server/src/main.rs
use mimalloc::MiMalloc;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;
```

mimalloc（微软研究院开发）的核心优化：

```
┌─────────────────────────────────────────────────┐
│ mimalloc 架构                                    │
│                                                   │
│  Thread 1          Thread 2          Thread 3    │
│  ┌─────────┐      ┌─────────┐      ┌─────────┐ │
│  │ TL Heap │      │ TL Heap │      │ TL Heap │ │
│  │ (本地)   │      │ (本地)   │      │ (本地)   │ │
│  └────┬────┘      └────┬────┘      └────┬────┘ │
│       │                │                │       │
│       └────────────────┼────────────────┘       │
│                        ↓                         │
│                 ┌──────────────┐                  │
│                 │   OS Pages   │                  │
│                 └──────────────┘                  │
└─────────────────────────────────────────────────┘

TL = Thread-Local（线程本地）
每个线程有自己的堆，分配/释放无需全局锁！
```

| 特性 | glibc malloc | jemalloc | mimalloc |
|------|-------------|----------|----------|
| 线程本地缓存 | ptmalloc2 arena | thread cache | segment-based TL heap |
| 小对象分配延迟 | ~40-80ns | ~20-40ns | ~10-25ns |
| 碎片率 | 中 | 低 | 低 |
| 占用空间 | 小 | 中 | 小 |
| 特点 | 通用 | FreeBSD 默认 | 低延迟优先 |

Tachyon 选择 mimalloc 的原因：
- 小对象分配最快（Order, EngineEvent 等）
- 线程本地堆，引擎线程分配无争用
- `local_dynamic_tls` feature 进一步优化 TLS 访问

---

## 6.5 CPU 亲和性

### 为什么要绑核？

当线程在不同核心间迁移时：

```
时间线（无 CPU 亲和性）：

Core 0:  [engine_thread]─────────┐
                                 │ OS 调度器迁移
Core 2:                          └──[engine_thread]─────
                                     ↑
                                     L1/L2 缓存全部失效！
                                     冷启动：~10-50μs

时间线（绑核到 Core 2）：

Core 2:  [engine_thread]─────────────[engine_thread]─────
                                      ↑
                                      L1/L2 缓存温热
                                      继续执行：~0ns 额外开销
```

### Tachyon 的 CPU 亲和性实现

```rust
// bin/tachyon-server/src/main.rs

// 获取可用 CPU 核心列表
let core_ids = core_affinity::get_core_ids().unwrap_or_default();

// 为每个引擎线程分配核心（跳过 Core 0 留给 OS）
let pin_core = if core_ids.len() > 1 {
    Some(core_ids[1 + (idx % (core_ids.len() - 1))])
} else {
    None
};

// 在引擎线程启动时绑定到指定核心
if let Some(core_id) = pin_core {
    if core_affinity::set_for_current(core_id) {
        tracing::info!(symbol_id, core = core_id.id, "Engine thread pinned to core");
    }
}
```

### 绑核策略

```
典型服务器（16 核）部署示例：

Core 0:   OS + 中断处理         ← 永远不绑应用线程
Core 1:   BTC/USDT 引擎线程    ← 独占
Core 2:   ETH/USDT 引擎线程    ← 独占
Core 3:   SOL/USDT 引擎线程    ← 独占
Core 4-7: Gateway (tokio) 线程池
Core 8-15: 空闲 / 监控 / 持久化

关键原则：
  1. Core 0 留给 OS — 中断处理和内核线程优先
  2. 引擎线程独占核心 — 不与其他线程共享
  3. 引擎核心使用 isolcpus — 防止 OS 调度其他任务到这些核心
```

### isolcpus 内核参数

```bash
# /etc/default/grub (Linux)
GRUB_CMDLINE_LINUX="isolcpus=1,2,3 nohz_full=1,2,3 rcu_nocbs=1,2,3"

# isolcpus:  OS 调度器不会将任何进程放到这些核心
# nohz_full: 关闭这些核心的定时器中断（减少抖动）
# rcu_nocbs: 将 RCU 回调移到其他核心（减少延迟尖刺）
```

> **面试考点**
>
> Q: CPU 亲和性（core affinity）对延迟有什么影响？`isolcpus` 和普通绑核的区别？
>
> A: 绑核防止线程迁移导致的缓存失效（冷启动 ~10-50μs）。但普通绑核不能阻止 OS 将其他线程调度到同一核心，引起上下文切换。`isolcpus` 告诉 OS 调度器完全不使用指定核心，只有显式绑定的线程才能运行在上面，实现真正的核心独占。配合 `nohz_full` 关闭定时器中断，可以将延迟抖动从 μs 级降到 ns 级。

---

## 6.6 NUMA 架构

### 什么是 NUMA？

**Non-Uniform Memory Access（非一致性内存访问）**：多路服务器中，每个 CPU 插槽有自己的本地内存，访问远端内存需要通过互联总线。

```
┌─────────────────────────────┐    QPI/UPI 互联    ┌─────────────────────────────┐
│         Socket 0            │ ←───── ~40ns ────→ │         Socket 1            │
│  ┌────────┐  ┌───────────┐  │                    │  ┌────────┐  ┌───────────┐  │
│  │ Core 0 │  │ L3 Cache  │  │                    │  │ Core 8 │  │ L3 Cache  │  │
│  │ Core 1 │  │  (共享)    │  │                    │  │ Core 9 │  │  (共享)    │  │
│  │  ...   │  │           │  │                    │  │  ...   │  │           │  │
│  │ Core 7 │  │           │  │                    │  │ Core 15│  │           │  │
│  └────────┘  └───────────┘  │                    │  └────────┘  └───────────┘  │
│                             │                    │                             │
│  ┌──────────────────────┐   │                    │  ┌──────────────────────┐   │
│  │   DRAM (本地内存)     │   │                    │  │   DRAM (本地内存)     │   │
│  │   ~50ns 访问          │   │                    │  │   ~50ns 访问          │   │
│  └──────────────────────┘   │                    │  └──────────────────────┘   │
└─────────────────────────────┘                    └─────────────────────────────┘

Core 0 访问本地 DRAM:   ~50ns
Core 0 访问 Socket 1 DRAM: ~90-130ns  ← 多了 ~40-80ns！
```

### NUMA 对撮合引擎的影响

```
场景：BTC/USDT 引擎线程运行在 Socket 0 的 Core 1 上

如果 OrderBook 数据分配在 Socket 0 的本地内存：
  每次 order 读取 ~50ns

如果 OrderBook 数据不幸分配在 Socket 1 的远端内存：
  每次 order 读取 ~100ns  ← 延迟翻倍！
```

### NUMA 优化策略

**1. 线程绑定 + 内存分配在本地 NUMA 节点**

```bash
# Linux: numactl 控制内存分配策略
numactl --cpunodebind=0 --membind=0 ./tachyon-server

# --cpunodebind=0: 所有线程绑定到 NUMA 节点 0 的核心
# --membind=0:     所有内存分配在 NUMA 节点 0
```

**2. 引擎线程在启动时分配所有数据结构**

由于 "first-touch" 策略（Linux 默认），内存页在首次写入时分配到当前线程所在的 NUMA 节点。Tachyon 的引擎线程在绑核后立即创建 OrderBook 和 Slab，确保数据在本地节点。

**3. 多 Socket 部署**

```
Socket 0 (NUMA Node 0):
  Core 1: BTC/USDT engine ← OrderBook 在本地内存
  Core 2: ETH/USDT engine ← OrderBook 在本地内存

Socket 1 (NUMA Node 1):
  Core 9: SOL/USDT engine ← OrderBook 在本地内存
  Core 10: Gateway 线程池

SPSC 队列（跨 Socket 通信）：
  Gateway → Engine 的 SPSC 队列不可避免地跨 NUMA
  优化：增大批量大小（drain_batch），减少跨节点原子操作次数
```

注意：Apple Silicon 和大多数消费级 CPU 都是 UMA（统一内存访问），NUMA 优化主要用于多路服务器部署。

---

## 6.7 编译器优化

### Tachyon 的 Release Profile

```toml
# Cargo.toml
[profile.release]
lto = "fat"          # 跨 crate 全程序链接时优化
codegen-units = 1    # 单个代码生成单元，最大化优化机会
panic = "abort"      # 不生成 unwind 表，减小二进制和运行时开销
strip = true         # 剥离调试符号，减小二进制大小

[profile.bench]
inherits = "release"
debug = true         # benchmark 需要调试信息用于 profiling
```

### 各项优化详解

**LTO (Link-Time Optimization) — 链接时优化**

```
无 LTO:
  tachyon-core 编译为 .rlib → 内联只在 crate 内部生效
  tachyon-book 编译为 .rlib → 跨 crate 调用不能内联
  链接 → 最终二进制

Fat LTO:
  所有 crate 的 LLVM IR 合并 → 全程序分析
  → 跨 crate 内联（如 Price::new(), Quantity::raw()）
  → 跨 crate 常量传播
  → 跨 crate 死代码消除
  → 编译时间 ~2-5x，运行时性能 ~10-20% 提升
```

**codegen-units = 1 — 单代码生成单元**

```
默认 codegen-units = 16:
  将 crate 分成 16 块并行编译
  快但牺牲了优化：每块只能看到局部代码
  函数可能因为在不同块而无法内联

codegen-units = 1:
  整个 crate 作为一块编译
  编译器能看到所有代码 → 最大化内联和优化
  编译慢（不能并行），但运行快
```

**panic = "abort" — 直接终止**

```
默认 panic = "unwind":
  - 生成 unwind 表（DWARF 调试信息）→ 二进制增大
  - 每个可能 panic 的函数都有 landing pad
  - 运行时需要维护 unwind 上下文 → 轻微性能影响
  - 支持 catch_unwind

panic = "abort":
  - 不生成 unwind 表 → 二进制更小
  - 没有 landing pad → 编译器可以更激进地优化
  - panic 直接调用 abort() → 进程终止
  - 在撮合引擎中，panic 应该是致命错误，abort 是正确行为
```

### PGO (Profile-Guided Optimization) — 配置文件引导优化

PGO 是 Phase 5 的计划项，工作流程：

```bash
# Step 1: 使用 instrumented build 收集 profile 数据
RUSTFLAGS="-Cprofile-generate=/tmp/pgo-data" cargo build --release

# Step 2: 运行真实或模拟的工作负载
./target/release/tachyon-server --config bench_config.toml
# （让它处理真实的订单流量）

# Step 3: 合并 profile 数据
llvm-profdata merge -o /tmp/pgo-data/merged.profdata /tmp/pgo-data

# Step 4: 使用 profile 数据重新编译
RUSTFLAGS="-Cprofile-use=/tmp/pgo-data/merged.profdata" cargo build --release
```

PGO 的效果：
- 基于真实运行数据优化分支预测提示
- 将热函数放在一起（改善 I-cache 局部性）
- 优化热循环的展开策略
- 预期提升 ~5-15%

> **面试考点**
>
> Q: LTO 和 PGO 分别优化了什么？
>
> A: LTO 是编译器的静态优化 — 通过看到更多代码做更好的内联和常量传播。PGO 是基于运行时数据的优化 — 通过 profile 数据知道哪些分支更常走、哪些函数更热，据此调整代码布局和分支预测提示。两者互补：LTO 扩大了优化视野，PGO 提供了真实世界的热度信息。

---

## 6.8 基准测试方法论

### Criterion 框架

Tachyon 使用 Criterion 进行基准测试，它解决了 `#[bench]` 的多个问题：

```rust
// crates/tachyon-bench/benches/matching.rs

fn bench_place_no_match(c: &mut Criterion) {
    let mut group = c.benchmark_group("engine/place_no_match");

    for &depth in &[0, 100, 1000, 10_000] {
        group.bench_with_input(
            BenchmarkId::new("depth", depth),
            &depth,
            |b, &depth| {
                b.iter_custom(|iters| {
                    let mut total = std::time::Duration::ZERO;
                    for _ in 0..iters {
                        // 每次迭代重建引擎（避免状态污染）
                        let mut engine = SymbolEngine::new(/*...*/);
                        // 预填充 book
                        for i in 0..depth {
                            engine.process_command(/*...*/);
                        }
                        // 只测量目标操作
                        let start = std::time::Instant::now();
                        let _ = black_box(engine.process_command(/*...*/));
                        total += start.elapsed();
                    }
                    total
                });
            },
        );
    }
}
```

### iter_custom 的重要性

为什么 Tachyon 使用 `iter_custom` 而不是简单的 `b.iter()`？

```rust
// 错误：setup 的时间被计入了基准测试
b.iter(|| {
    let mut engine = setup_engine(10000);  // ← 这个很慢
    engine.process_command(cmd);            // ← 这才是要测的
});

// 正确：只计时目标操作
b.iter_custom(|iters| {
    let mut total = Duration::ZERO;
    for _ in 0..iters {
        let mut engine = setup_engine(10000);  // ← 不计时
        let start = Instant::now();
        let _ = black_box(engine.process_command(cmd));  // ← 只计时这里
        total += start.elapsed();
    }
    total
});
```

### 基准测试矩阵

Tachyon 的基准测试覆盖了多个维度：

```
=== SPSC 队列 (crates/tachyon-bench/benches/spsc.rs) ===

spsc/push_pop:
  - capacity: 1024, 4096, 65536

spsc/burst:
  - size: 16, 64, 256

spsc/roundtrip_100:
  - 100 次 push + 100 次 pop

=== 订单簿操作 (benches/orderbook.rs) ===

orderbook/add_order:
  - depth: 10, 100, 1000, 10000

orderbook/cancel_order:
  - depth: 10, 100, 1000, 10000

orderbook/get_depth:
  - levels: 5, 20, 50

orderbook/best_bid_price, best_ask_price:
  - 1000 levels 两侧

=== 撮合引擎 (benches/matching.rs) ===

engine/place_no_match:
  - depth: 0, 100, 1000, 10000

engine/single_fill:
  - depth: 1, 100, 1000

engine/sweep_levels:
  - levels: 1, 5, 10, 50

engine/cancel:
  - depth: 100, 1000, 10000

engine/fok:
  - success / reject

=== 吞吐量 (benches/throughput.rs) ===

throughput/mixed (60% limit + 30% cancel + 10% market):
  - 1K, 10K, 100K ops

throughput/insert_only:
  - 100K ops

throughput/insert_cancel:
  - 100K ops (50K insert + 50K cancel)
```

### 如何运行基准测试

```bash
# 运行全部基准测试
cargo bench

# 只运行 SPSC 基准测试
cargo bench --bench spsc

# 只运行撮合引擎基准测试
cargo bench --bench matching

# 使用 filter 运行特定测试
cargo bench -- "engine/single_fill"

# 生成 HTML 报告（在 target/criterion/ 下）
# 默认已开启 html_reports feature
```

### 正确的基准测试实践

| 做 | 不做 |
|----|------|
| 使用 `black_box()` 防止编译器优化掉结果 | 忽略 `black_box`，让编译器可能删除整个计算 |
| 使用 `iter_custom` 分离 setup 和测量 | 将 setup 包含在测量时间内 |
| 固定随机种子确保可重现 | 使用随机种子导致结果不稳定 |
| 关闭频率调节（`cpupower frequency-set -g performance`）| 在 powersave 模式下跑基准测试 |
| 多次运行取统计值 | 只运行一次就下结论 |
| 测试不同数据规模（10, 100, 1000, 10000） | 只测一个规模然后外推 |

> **面试考点**
>
> Q: `black_box` 在 Rust 基准测试中的作用是什么？
>
> A: `black_box(x)` 是一个编译器不透明的函数，它接受一个值并原样返回，但阻止编译器对传入值做任何假设或优化。没有 `black_box`，编译器可能发现函数结果未被使用，直接删除整个计算，导致基准测试测量的是空操作的时间。

---

## 6.9 系统调优清单

### 生产部署优化清单

以下是 Tachyon 在 Linux 生产环境中的推荐配置：

#### 1. CPU 相关

```bash
# 隔离引擎核心
# /etc/default/grub
GRUB_CMDLINE_LINUX="isolcpus=1,2,3 nohz_full=1,2,3 rcu_nocbs=1,2,3"

# 设置 CPU 调频策略为性能模式（禁用节能降频）
cpupower frequency-set -g performance

# 或者直接设置：
echo performance | tee /sys/devices/system/cpu/cpu*/cpufreq/scaling_governor

# 禁用超线程（HT 增加缓存争用，低延迟场景建议关闭）
# 在 BIOS 中关闭，或：
echo 0 > /sys/devices/system/cpu/cpu<HT-sibling>/online
```

#### 2. 内存相关

```bash
# 启用大页（Huge Pages）— 减少 TLB miss
# 普通页 = 4KB，大页 = 2MB
# 1GB 内存 = 262144 个 4KB 页 vs 512 个 2MB 大页
# TLB 命中率显著提高

# 预分配 1024 个 2MB 大页（= 2GB）
echo 1024 > /proc/sys/vm/nr_hugepages

# 或使用 1GB 大页（需要 BIOS 支持）：
# GRUB_CMDLINE_LINUX="hugepagesz=1G hugepages=2"

# 在 Rust 中使用大页（mimalloc 自动使用，如果可用）
# 或通过 mmap:
# mmap(NULL, size, PROT_READ|PROT_WRITE, MAP_PRIVATE|MAP_ANONYMOUS|MAP_HUGETLB, -1, 0)

# 禁用透明大页（THP）— THP 的合并/分裂会导致延迟尖刺
echo never > /sys/kernel/mm/transparent_hugepage/enabled
echo never > /sys/kernel/mm/transparent_hugepage/defrag

# 锁定内存防止 swap
# 在 /etc/security/limits.conf:
# tachyon  soft  memlock  unlimited
# tachyon  hard  memlock  unlimited
```

#### 3. 调度器相关

```bash
# 设置引擎线程为实时调度策略（SCHED_FIFO）
# 优先级 90（范围 1-99，越高越优先）
chrt -f 90 taskset -c 1 ./tachyon-server

# 或在 Rust 代码中：
# libc::sched_setscheduler(0, libc::SCHED_FIFO, &param)

# 减少调度器时间片的影响
echo 1 > /proc/sys/kernel/sched_rt_runtime_us  # 对 CFS 无影响
```

#### 4. 网络相关

```bash
# 增大 socket 缓冲区
sysctl -w net.core.rmem_max=16777216
sysctl -w net.core.wmem_max=16777216
sysctl -w net.core.rmem_default=1048576
sysctl -w net.core.wmem_default=1048576

# TCP 优化
sysctl -w net.ipv4.tcp_rmem="4096 1048576 16777216"
sysctl -w net.ipv4.tcp_wmem="4096 1048576 16777216"
sysctl -w net.ipv4.tcp_nodelay=1  # 禁用 Nagle 算法

# 网卡中断绑核（将网卡中断绑到非引擎核心）
# 查看中断号：cat /proc/interrupts | grep eth0
echo 0 > /proc/irq/<IRQ_NUM>/smp_affinity_list

# 开启 busy-polling（减少网络接收延迟）
sysctl -w net.core.busy_read=50
sysctl -w net.core.busy_poll=50
```

#### 5. 内核旁路（进阶）

对于极致低延迟需求，考虑内核旁路网络：

```
传统网络栈：
  NIC → 内核驱动 → 软中断 → TCP/IP 栈 → socket → 用户空间
  延迟：~10-50μs

DPDK/io_uring:
  NIC → 用户空间直接访问  (DPDK)
  NIC → 内核 → io_uring → 用户空间  (io_uring, 较新)
  延迟：~1-5μs

FPGA:
  NIC → FPGA 硬件解析 → 用户空间
  延迟：<1μs
```

Tachyon 当前使用标准 TCP（通过 tokio），适合大多数场景。内核旁路属于 Phase 5+ 的远期目标。

### 优化效果总结

```
优化项                          预期延迟改善
──────────────────────────────────────────────
mimalloc 替换系统分配器           -10-30ns/alloc
CPU 绑核 (core_affinity)         -10-50μs (消除迁移)
isolcpus                         减少延迟尖刺 ~90%
大页 (Huge Pages)                -5-20ns/TLB miss
SCHED_FIFO                       减少延迟尖刺 ~50%
Fat LTO + codegen-units=1        ~10-20% 整体提升
panic=abort                      ~2-5% 二进制减小
PGO (Phase 5)                    ~5-15% 整体提升
禁用超线程                       减少 L1 缓存争用
网络优化 (busy_poll)             -5-20μs 网络延迟
```

> **面试考点**
>
> Q: 如果让你优化一个交易系统的延迟，你会从哪些方面入手？按什么优先级？
>
> A: 按投入产出比排序：(1) 数据结构和算法优化（O(n) → O(1)）— 这通常是最大的杠杆；(2) 内存布局优化（减少 cache miss）— 紧密排列热数据；(3) 避免堆分配（SmallVec, Slab, 预分配）— 消除 malloc 延迟；(4) 编译器优化（LTO, PGO）— 几乎免费的提升；(5) 替换分配器（mimalloc）— 一行代码的改进；(6) 系统级调优（绑核、大页、调度器）— 需要运维配合；(7) 无锁数据结构 — 消除锁争用；(8) 内核旁路 — 投入最大，只在极端场景需要。

---

## 本章小结

| 优化层次 | 技术 | Tachyon 中的应用 |
|---------|------|----------------|
| 硬件层 | CPU 缓存层次理解 | 热数据 <64B，L1 命中优先 |
| 内存布局 | `#[repr(C)]`、字段排序 | Order 结构体 80B，热字段在第一缓存行 |
| 分配策略 | SmallVec/ArrayVec/Slab | 热路径零堆分配 |
| 分配器 | mimalloc | 线程本地堆，小对象 ~10ns |
| OS 调度 | core_affinity + isolcpus | 引擎线程独占核心 |
| NUMA | 本地内存分配 | numactl / first-touch 策略 |
| 编译器 | LTO + PGO + codegen-units=1 | Cargo.toml profile.release |
| 基准测试 | Criterion + iter_custom | 4 个基准测试套件 |
| 系统调优 | 大页 + SCHED_FIFO + 网络 | 生产部署清单 |

**下一章**：[第七章：系统集成 — 从网关到持久化](07-integration.md)
