# 第五章：无锁并发 — 从内存模型到 SPSC 队列

> **前置知识**：第二章架构概览中的线程模型，Rust 基本所有权和借用概念。
>
> **本章目标**：深入理解 Tachyon 的无锁通信基础设施，掌握从 CPU 缓存行到内存排序的全栈并发知识。

---

## 目录

- [5.1 为什么需要无锁](#51-为什么需要无锁)
- [5.2 内存排序模型](#52-内存排序模型)
- [5.3 Happens-before 关系](#53-happens-before-关系)
- [5.4 SPSC 环形缓冲区实现](#54-spsc-环形缓冲区实现)
- [5.5 代码逐行分析](#55-代码逐行分析)
- [5.6 CachePadded 与伪共享](#56-cachepadded-与伪共享)
- [5.7 x86 TSO vs ARM 弱内存模型](#57-x86-tso-vs-arm-弱内存模型)
- [5.8 批量操作与 LMAX Disruptor 模式](#58-批量操作与-lmax-disruptor-模式)
- [5.9 Send + Sync + UnsafeCell](#59-send--sync--unsafecell)

---

## 5.1 为什么需要无锁

### 互斥锁的代价

在一个目标延迟为 **<100ns** 的撮合引擎中，传统的 `Mutex` 和 `RwLock` 有三个致命问题：

| 问题 | 说明 | 延迟影响 |
|------|------|----------|
| **内核态切换** | `Mutex` 争用时线程被挂起，涉及 syscall（`futex`/`pthread_mutex_lock`） | ~1-10μs |
| **优先级反转** | 持锁线程被低优先级线程抢占，高优先级线程饥饿 | 不确定，可达 ms 级 |
| **护航效应** | 多线程排队等锁，吞吐量退化为串行 | 随争用线性恶化 |

```
时间线（使用 Mutex）：

Thread A (Producer):   [获取锁]---[写入]---[释放锁]
                              ↑                ↓
Thread B (Consumer):          [等待..........][获取锁]---[读取]---[释放锁]
                                   ~1-10μs

时间线（使用 SPSC 无锁队列）：

Thread A (Producer):   [写入 slot]---[Release store tail]
Thread B (Consumer):   [Acquire load tail]---[读取 slot]
                           ~几纳秒
```

### 无锁 vs 无等待

这两个概念经常被混淆，需要严格区分：

- **Lock-free（无锁）**：至少一个线程在有限步内完成操作。系统整体保证前进，但个别线程可能暂时被延迟。
- **Wait-free（无等待）**：每个线程都在有限步内完成操作。最强保证。

Tachyon 的 SPSC 队列是 **wait-free** 的：

```
try_push: 1次 Relaxed load + 1次 Acquire load + 1次写入 + 1次 Release store
try_pop:  1次 Relaxed load + 1次 Acquire load + 1次读取 + 1次 Release store
```

每个操作都在常数步内完成，不存在 CAS 重试循环。

> **面试考点**
>
> Q: 无锁和无等待的区别是什么？CAS 循环是无锁还是无等待？
>
> A: CAS 循环是无锁但不是无等待。在极端情况下，某个线程可能被反复抢占导致 CAS 一直失败（ABA 问题或竞争激烈）。SPSC 队列因为只有一个生产者和一个消费者，不存在 CAS，所以是无等待的。

---

## 5.2 内存排序模型

### C++11/Rust 内存模型

现代 CPU 为了性能会重排指令。编译器也会重排。内存排序（Memory Ordering）是程序员与 CPU/编译器之间的契约，规定了多线程可见性的保证。

Rust 的 `std::sync::atomic::Ordering` 直接映射 C++11 内存模型：

```
强 ←————————————————————————————————→ 弱
SeqCst   AcqRel   Release   Acquire   Relaxed
  │        │         │         │         │
  │        │         │         │         └─ 无任何排序保证
  │        │         │         └─────────── 该 load 之后的读写不会重排到该 load 之前
  │        │         └───────────────────── 该 store 之前的读写不会重排到该 store 之后
  │        └─────────────────────────────── Acquire + Release 组合
  └──────────────────────────────────────── 全局全序，所有线程看到相同的操作顺序
```

### 每种排序的直觉理解

**Relaxed** — "我只需要原子性，不关心顺序"

```rust
// 只保证读写本身是原子的，不建立任何 happens-before 关系
let val = counter.load(Ordering::Relaxed);
counter.store(val + 1, Ordering::Relaxed);
```

适用场景：统计计数器、近似值查询。Tachyon 中 `len()` 使用 Relaxed。

**Acquire** — "从这里开始，我能看到对方 Release 之前的所有写入"

```rust
// 加载 tail 时使用 Acquire：保证能看到生产者在 Release store 之前写入的数据
let tail = self.tail.value.load(Ordering::Acquire);
// ↑ 此 load 之后的所有读操作，都能看到配对的 Release store 之前的写操作
```

**Release** — "我之前的所有写入，对 Acquire 方都可见"

```rust
// 先写入数据到 slot
unsafe { std::ptr::write(slot_ptr, value); }
// 然后 Release store 推进 tail：保证数据写入对消费者可见
self.tail.value.store(tail + 1, Ordering::Release);
// ↑ 此 store 之前的所有写操作，对配对的 Acquire load 可见
```

**SeqCst** — "所有线程看到完全相同的操作顺序"

代价最高，Tachyon 中从未使用。大多数场景下 Acquire/Release 已足够。

### 时间线图示

```
Producer Thread                    Consumer Thread
═══════════════                    ════════════════
  ① ptr::write(slot, data)
  ② tail.store(T+1, Release) ─────→ ③ tail.load(Acquire) == T+1
     ↑ 围栏：①的写入                    ↓ 围栏：④能看到①的写入
     必须在②之前完成                  ④ ptr::read(slot) → data ✓
```

这就是 Release/Acquire 配对的核心：**Release store 之前的所有写入，对后续配对的 Acquire load 之后的所有读取可见**。

> **面试考点**
>
> Q: 为什么 SPSC 队列不需要 SeqCst？
>
> A: SeqCst 保证全局全序，但 SPSC 只有两个线程通信，Acquire/Release 配对就足以建立 happens-before 关系。SeqCst 在 x86 上需要额外的 MFENCE 指令，在 ARM 上需要 DMB，代价更高。

---

## 5.3 Happens-before 关系

### 形式化定义

如果操作 A **happens-before** 操作 B（记作 A →hb B），那么 A 的效果对 B 保证可见。

在 Tachyon SPSC 中，happens-before 链条如下：

```
Producer 线程内                      跨线程                         Consumer 线程内
================                   ========                       ================

ptr::write(slot, data)   →seq

tail.store(T+1, Release) →sync  tail.load(Acquire) == T+1
                                                          →seq
                                                                  ptr::read(slot) → data

→seq = 同一线程内的程序顺序 (sequenced-before)
→sync = Release-Acquire 同步关系 (synchronizes-with)
```

组合这些关系：

```
ptr::write(slot, data) →seq tail.store(Release) →sync tail.load(Acquire) →seq ptr::read(slot)
```

因此 `ptr::write` 的效果对 `ptr::read` **保证可见**。这就是 SPSC 正确性的数学基础。

### 具体到 try_push / try_pop

```rust
// Producer: try_push
fn try_push(&self, value: T) -> Result<(), T> {
    let tail = self.tail.value.load(Relaxed);      // ← 自己的 tail，Relaxed 足够
    let head = self.head.value.load(Acquire);       // ← 同步点：看到消费者释放的 head

    if tail - head >= self.capacity { return Err(value); }

    unsafe { std::ptr::write(slot_ptr, value); }    // ← 写入数据

    self.tail.value.store(tail + 1, Release);       // ← 发布点：数据对消费者可见
    Ok(())
}
```

```rust
// Consumer: try_pop
fn try_pop(&self) -> Option<T> {
    let head = self.head.value.load(Relaxed);       // ← 自己的 head，Relaxed 足够
    let tail = self.tail.value.load(Acquire);       // ← 同步点：看到生产者发布的 tail

    if head >= tail { return None; }

    let value = unsafe { std::ptr::read(slot_ptr) }; // ← 读取数据

    self.head.value.store(head + 1, Release);        // ← 发布点：slot 可被生产者重用
    Some(value)
}
```

注意对称性：

| 操作 | 读自己的指针 | 读对方的指针 | 写数据/读数据 | 推进自己的指针 |
|------|------------|------------|-------------|-------------|
| try_push | tail = Relaxed | head = Acquire | ptr::write | tail = Release |
| try_pop | head = Relaxed | tail = Acquire | ptr::read | head = Release |

**自己的指针用 Relaxed**：只有自己写，不需要跨线程同步。

**对方的指针用 Acquire**：需要看到对方 Release 之前的所有写入。

**推进自己的指针用 Release**：让对方能看到本次操作的数据效果。

---

## 5.4 SPSC 环形缓冲区实现

### 数据结构设计

```rust
pub struct SpscQueue<T> {
    buffer: Box<[UnsafeCell<MaybeUninit<T>>]>,  // 预分配 slot 数组
    capacity: usize,                             // 容量（必须是 2 的幂）
    mask: usize,                                 // capacity - 1，用于快速取模
    head: CachePadded<AtomicUsize>,              // 消费者读位置
    tail: CachePadded<AtomicUsize>,              // 生产者写位置
}
```

### 为什么容量必须是 2 的幂？

这是经典的位运算技巧：

```
传统取模：  slot = index % capacity     // 除法指令，~20-40 个时钟周期
位掩码：    slot = index & mask          // AND 指令，1 个时钟周期
```

**约束**：`capacity = 2^n`，`mask = capacity - 1 = 2^n - 1`

```
capacity = 8 = 0b1000
mask     = 7 = 0b0111

index = 0:  0b0000 & 0b0111 = 0
index = 3:  0b0011 & 0b0111 = 3
index = 7:  0b0111 & 0b0111 = 7
index = 8:  0b1000 & 0b0111 = 0  ← 自动回绕！
index = 11: 0b1011 & 0b0111 = 3  ← 回绕后索引正确
```

### head/tail 协议

```
buffer:  [ slot0 | slot1 | slot2 | slot3 | slot4 | slot5 | slot6 | slot7 ]
              ↑                       ↑
             head=2                  tail=5

可读区间：[head, tail) = [2, 5) → slot2, slot3, slot4
可写位置：tail=5 → slot5
剩余空间：capacity - (tail - head) = 8 - 3 = 5

注意：head 和 tail 是单调递增的，永远不回绕。
回绕由 & mask 处理：实际 slot 索引 = head & mask。
```

**空队列**：`head == tail`

**满队列**：`tail - head == capacity`

这种设计的巧妙之处：head/tail 单调递增不会溢出（`usize` 为 64 位，即使每纳秒自增一次也需要约 584 年才会溢出）。

### UnsafeCell + MaybeUninit

```rust
buffer: Box<[UnsafeCell<MaybeUninit<T>>]>
```

- `UnsafeCell<T>`：Rust 中唯一合法的内部可变性原语。因为 `&SpscQueue` 是共享引用（producer 和 consumer 都持有），要修改 buffer slot 必须通过 `UnsafeCell`。
- `MaybeUninit<T>`：允许 slot 处于未初始化状态，避免在构造函数中为每个 slot 创建 `T` 的默认值。

---

## 5.5 代码逐行分析

### try_push — 生产者入队

```rust
#[inline]
pub fn try_push(&self, value: T) -> Result<(), T> {
    // ① 读取自己的写位置。只有自己写 tail，所以 Relaxed 就够了。
    let tail = self.tail.value.load(Ordering::Relaxed);

    // ② Acquire load head：同步消费者的 Release store。
    //    这让我们看到消费者已经消费了哪些 slot（可以重用）。
    let head = self.head.value.load(Ordering::Acquire);

    // ③ 判断是否满：tail - head >= capacity 意味着所有 slot 都被占用。
    //    注意使用减法而非比较，即使在理论上 tail 溢出也能正确工作
    //    （但 usize 溢出实际上不可能发生）。
    if tail - head >= self.capacity {
        return Err(value);
    }

    // ④ 计算实际 slot 索引。& self.mask 是 % capacity 的快速替代。
    let slot = tail & self.mask;

    // SAFETY: 单生产者保证，没有其他线程在写这个 slot。
    // 消费者不会读这个 slot，因为 tail 还没推进。
    // ⑤ 直接写入裸指针，跳过 Drop（slot 是 MaybeUninit）。
    unsafe {
        std::ptr::write((*self.buffer[slot].get()).as_mut_ptr(), value);
    }

    // ⑥ Release store 推进 tail。
    //    这是发布点：保证 ⑤ 的数据写入对消费者的 Acquire load 可见。
    self.tail.value.store(tail + 1, Ordering::Release);
    Ok(())
}
```

### try_pop — 消费者出队

```rust
#[inline]
pub fn try_pop(&self) -> Option<T> {
    // ① 读取自己的读位置。只有自己写 head，Relaxed 足够。
    let head = self.head.value.load(Ordering::Relaxed);

    // ② Acquire load tail：同步生产者的 Release store。
    //    这让我们看到生产者写入了哪些新数据。
    let tail = self.tail.value.load(Ordering::Acquire);

    // ③ 空队列判断。
    if head >= tail {
        return None;
    }

    let slot = head & self.mask;

    // SAFETY: 单消费者保证，没有其他线程在读这个 slot。
    // 生产者不会覆写这个 slot，因为 head 还没推进。
    // ④ 读取数据。ptr::read 会按位复制，不会触发 Drop。
    let value = unsafe { std::ptr::read((*self.buffer[slot].get()).as_ptr()) };

    // ⑤ Release store 推进 head。
    //    通知生产者这个 slot 已被消费，可以重用。
    self.head.value.store(head + 1, Ordering::Release);
    Some(value)
}
```

### drain_batch — 批量出队

```rust
#[inline]
pub fn drain_batch<const N: usize>(&self, out: &mut ArrayVec<T, N>) -> usize {
    let head = self.head.value.load(Ordering::Relaxed);
    let tail = self.tail.value.load(Ordering::Acquire);

    let available = tail - head;
    let remaining_capacity = N - out.len();
    // 取可用数量和输出缓冲剩余容量的较小值
    let count = available.min(remaining_capacity);
    if count == 0 {
        return 0;
    }

    // 批量读取所有可用 slot
    for i in 0..count {
        let slot = (head + i) & self.mask;
        unsafe {
            out.push_unchecked(std::ptr::read((*self.buffer[slot].get()).as_ptr()));
        }
    }

    // 关键优化：N 个 item 只需要 1 次 Release store！
    // 而逐个 try_pop 需要 N 次 Release store。
    self.head.value.store(head + count, Ordering::Release);
    count
}
```

> **面试考点**
>
> Q: `drain_batch` 相比循环调用 `try_pop` 的优势是什么？
>
> A: 两方面。第一，原子操作数量从 2N（每次 try_pop 需要 1 次 Acquire load + 1 次 Release store）降到 2（1 次 Acquire load + 1 次 Release store），减少了 CPU 流水线停顿。第二，`push_unchecked` 跳过了 ArrayVec 的容量检查，减少分支预测压力。

---

## 5.6 CachePadded 与伪共享

### 什么是伪共享（False Sharing）？

现代 CPU 以 **缓存行（cache line）** 为单位管理缓存，通常为 64 字节。当两个不同变量位于同一缓存行时，即使它们被不同核心独立修改，也会触发缓存一致性协议（MESI）的失效操作。

```
                    ┌─── 64 字节缓存行 ───────────────┐
                    │ head (8B) │ tail (8B) │ padding  │
                    └─────────────────────────────────┘
                         ↑                ↑
                    Consumer 写       Producer 写

Consumer 修改 head → 整条缓存行失效 → Producer 的 L1 缓存中 tail 也失效！
Producer 修改 tail → 整条缓存行失效 → Consumer 的 L1 缓存中 head 也失效！

两个核心互相 "乒乓" 缓存行 → 延迟从 ~1ns 退化到 ~40-70ns
```

### MESI 协议状态转换

```
缓存行状态（MESI）:
  M (Modified)  — 本核心独占修改，其他核心无效副本
  E (Exclusive) — 本核心独占，与内存一致
  S (Shared)    — 多核心共享，只读
  I (Invalid)   — 无效，需要从其他核心或内存获取

伪共享的乒乓过程：
  Core 0 写 head → 缓存行变 M → Core 1 的缓存行变 I
  Core 1 写 tail → 需要先从 Core 0 获取最新行 → Core 0 的缓存行变 I
  Core 0 写 head → 需要先从 Core 1 获取最新行 → ...

  每次 "获取" = ~40-70ns（跨核心 L2/L3 访问）
```

### Tachyon 的 CachePadded 解决方案

```rust
// x86/x86_64: 缓存行 64 字节
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[repr(align(64))]
struct CachePadded<T> {
    value: T,
}

// aarch64 (Apple Silicon M1-M4, AWS Graviton): 缓存行 128 字节
#[cfg(not(any(target_arch = "x86", target_arch = "x86_64")))]
#[repr(align(128))]
struct CachePadded<T> {
    value: T,
}
```

效果：

```
Without CachePadded:
┌──────────────────────────── 64B cache line ─────────────────────────────┐
│ head: AtomicUsize (8B) │ tail: AtomicUsize (8B) │ ... other fields ... │
└─────────────────────────────────────────────────────────────────────────┘
  ↑ Core 0 写               ↑ Core 1 写          → 互相失效！

With CachePadded:
┌──── 64B cache line ─────┐  ┌──── 64B cache line ─────┐
│ head (8B) + 56B padding │  │ tail (8B) + 56B padding │
└─────────────────────────┘  └─────────────────────────┘
  ↑ Core 0 独占                ↑ Core 1 独占           → 互不干扰！
```

### 为什么 Apple Silicon 用 128 字节？

Apple M 系列芯片的 L1 缓存使用 128 字节缓存行（性能核心和效率核心都是）。如果只对齐到 64 字节，在 M1/M2/M3/M4 上仍会发生伪共享。Tachyon 根据编译目标自动选择对齐大小。

> **面试考点**
>
> Q: 什么是伪共享？如何检测和解决？
>
> A: 伪共享是两个独立变量共享同一缓存行，导致一个核心的写操作使另一个核心的缓存失效。检测方法：Linux perf 的 `perf c2c` 可以检测缓存行争用。解决方法：`#[repr(align(64))]` 或 `crossbeam_utils::CachePadded`。在 ARM 上需要 128 字节对齐。

---

## 5.7 x86 TSO vs ARM 弱内存模型

### x86 的 Total Store Order (TSO)

x86 提供了较强的内存模型保证：

1. **Store-Store 有序**：写操作不会被重排（store buffer 是 FIFO）。
2. **Load-Load 有序**：读操作不会被重排。
3. **Load-Store 有序**：读在写之前不会被重排到写之后。
4. **Store-Load 可能重排**：这是唯一允许的重排（需要 MFENCE 阻止）。

```
x86 上 Acquire/Release 的汇编：

// Acquire load → 普通 MOV 就行（x86 天然保证 load-load 和 load-store 有序）
let tail = self.tail.value.load(Ordering::Acquire);
// 编译为:  mov rax, [tail_addr]    ← 就是普通 MOV！

// Release store → 普通 MOV 就行（x86 天然保证 store-store 有序）
self.tail.value.store(tail + 1, Ordering::Release);
// 编译为:  mov [tail_addr], rax    ← 还是普通 MOV！

// 只有 SeqCst store 需要额外的围栏：
self.tail.value.store(tail + 1, Ordering::SeqCst);
// 编译为:  xchg [tail_addr], rax   ← 或 mov + mfence
```

结论：在 x86 上，**Acquire/Release 是零开销的**。

### ARM/AArch64 的弱内存模型

ARM 允许几乎所有的指令重排，所以需要显式的屏障指令：

```
ARM (AArch64) 上的 Acquire/Release 编译结果：

// Acquire load → 使用 LDAR (Load-Acquire Register)
let tail = self.tail.value.load(Ordering::Acquire);
// 编译为:  ldar x0, [tail_addr]    ← 专用 Acquire 指令

// Release store → 使用 STLR (Store-Release Register)
self.tail.value.store(tail + 1, Ordering::Release);
// 编译为:  stlr x0, [tail_addr]    ← 专用 Release 指令

// 对比 Relaxed：
let tail = self.tail.value.load(Ordering::Relaxed);
// 编译为:  ldr x0, [tail_addr]     ← 普通 Load，无屏障
```

### 性能对比

```
                        x86 (Intel/AMD)          ARM (Apple M/Graviton)
Relaxed load/store      MOV (~1ns)               LDR/STR (~1ns)
Acquire load            MOV (~1ns) = 免费！       LDAR (~1-2ns)
Release store           MOV (~1ns) = 免费！       STLR (~1-2ns)
SeqCst load             MOV (~1ns)               LDAR (~1-2ns)
SeqCst store            XCHG (~20ns)             STLR + DMB (~5-10ns)
```

关键结论：

1. 在 x86 上，Acquire/Release 相比 Relaxed 没有额外开销。
2. 在 ARM 上，Acquire/Release 有轻微开销（LDAR/STLR 比普通 LDR/STR 慢约 1ns），但远低于 SeqCst。
3. **永远不要为了"优化"在 ARM 上把 Acquire/Release 降级为 Relaxed** — 这会导致正确性 bug，且 1ns 差异微不足道。

> **面试考点**
>
> Q: 为什么在 x86 上 Acquire/Release 是"免费"的？
>
> A: x86 的 TSO 模型天然保证了 load-load、load-store、store-store 有序，唯一允许的重排是 store-load（一个 store 可能被延迟到后续 load 之后，因为 store buffer）。而 Acquire 只需要阻止 load-load 和 load-store 重排（x86 已保证），Release 只需要阻止 store-store 和 load-store 重排（x86 已保证）。因此编译器只需要阻止编译器重排（compiler fence），不需要任何 CPU 屏障指令。

---

## 5.8 批量操作与 LMAX Disruptor 模式

### LMAX Disruptor 的启示

LMAX Exchange（伦敦金属交易所）的 Disruptor 框架证明了一个关键模式：**批量摊销原子操作成本**。

传统逐条处理：

```
每条消息成本：1 Acquire load + 1 Release store = 2 次原子操作
处理 N 条消息 = 2N 次原子操作
```

批量处理（Tachyon 的 `drain_batch`）：

```
N 条消息成本：1 Acquire load + 1 Release store = 2 次原子操作
摊销成本 = 2/N 次原子操作/消息
```

### Tachyon 的引擎线程循环

在 `bin/tachyon-server/src/main.rs` 中，引擎线程使用批量模式：

```rust
const BATCH_SIZE: usize = 64;

fn engine_thread_loop(mut ctx: EngineThreadCtx) {
    loop {
        let mut batch = ArrayVec::<EngineCommand, BATCH_SIZE>::new();
        ctx.queue.drain_batch(&mut batch);

        if batch.is_empty() {
            // 渐进退避：先自旋，然后 yield
            spin_count += 1;
            if spin_count < 1000 {
                std::hint::spin_loop();    // PAUSE 指令，降低功耗
            } else {
                std::thread::yield_now();  // 让出 CPU
                spin_count = 0;
            }
            continue;
        }

        for cmd in batch {
            // 处理命令...
        }
    }
}
```

这里的设计要点：

1. **ArrayVec<_, 64>**：栈上分配 64 个 slot，零堆分配。
2. **drain_batch**：一次 Acquire + 一次 Release，最多读 64 条命令。
3. **渐进退避**：空队列时先 `spin_loop()`（1000 次），然后 `yield_now()`。平衡延迟和 CPU 占用。

### 吞吐量对比

```
假设每秒 5M 消息：

逐条 try_pop:
  5M × 2 atomics = 10M 原子操作/秒
  平均批量大小 = 1

drain_batch(64):
  假设平均批量 = 32（队列经常有积压）
  5M / 32 × 2 atomics = 312K 原子操作/秒
  减少 97% 的原子操作！
```

### MPSC：多 SPSC 组合

Tachyon 的 MPSC 不是传统的 CAS-based MPSC（如 `crossbeam::channel`），而是 **N 个独立 SPSC 队列的集合**：

```rust
pub struct MpscQueue<T> {
    queues: Vec<SpscQueue<T>>,        // 每个生产者一个专用 SPSC
    next: std::cell::Cell<usize>,     // 轮询游标（只有消费者访问）
}
```

优势：

```
传统 MPSC (CAS-based):         Tachyon MPSC (N × SPSC):
- 所有生产者争用同一 head       - 每个生产者写自己的 SPSC，零争用
- CAS 失败 → 重试循环          - wait-free push
- 缓存行在多核间乒乓           - 每个 SPSC 的 tail 在不同缓存行
- 性能随生产者数量退化          - 性能与生产者数量无关
```

---

## 5.9 Send + Sync + UnsafeCell

### Rust 的并发安全模型

Rust 通过两个 marker trait 在编译时保证线程安全：

- `Send`：类型可以安全地**发送**到另一个线程（转移所有权）。
- `Sync`：类型可以安全地被多线程**共享引用**（`&T` 可以跨线程使用）。

大多数类型自动实现这两个 trait。但 `UnsafeCell<T>` 是特例：

```rust
// UnsafeCell 既不是 Send 也不是 Sync（即使 T 是）
// 因为通过 &UnsafeCell<T> 可以获取 *mut T，可能导致数据竞争

// SpscQueue 包含 UnsafeCell，所以自动派生失败
// 我们需要手动 impl：
unsafe impl<T: Send> Send for SpscQueue<T> {}
unsafe impl<T: Send> Sync for SpscQueue<T> {}
```

### 为什么这是安全的？

`unsafe impl Sync` 的 **安全证明** 依赖于 SPSC 协议不变量：

1. **单生产者**：只有一个线程调用 `try_push`。它修改 tail 和写入 `buffer[tail & mask]`。
2. **单消费者**：只有一个线程调用 `try_pop`。它修改 head 和读取 `buffer[head & mask]`。
3. **不重叠**：在任意时刻，生产者写入的 slot 范围 `[tail, ...)` 和消费者读取的 slot 范围 `[head, tail)` 不重叠。因为：
   - 生产者只写 `tail` 位置，写完后才推进 tail。
   - 消费者只读 `[head, tail)` 范围，读完后才推进 head。
   - `tail` 的推进对消费者通过 Acquire 可见。
   - `head` 的推进对生产者通过 Acquire 可见。

因此，没有两个线程会同时访问同一个 `UnsafeCell<MaybeUninit<T>>`。

### UnsafeCell 的角色

```rust
buffer: Box<[UnsafeCell<MaybeUninit<T>>]>
```

为什么不能用 `Box<[MaybeUninit<T>]>`？因为我们通过 `&self`（共享引用）修改 buffer：

```rust
pub fn try_push(&self, value: T) -> Result<(), T> {
    // self 是 &SpscQueue，是共享引用
    // 但我们需要写入 buffer slot → 需要内部可变性
    unsafe {
        std::ptr::write((*self.buffer[slot].get()).as_mut_ptr(), value);
        //                                  ^^^
        //              UnsafeCell::get() → *mut MaybeUninit<T>
        //              这是获取可变指针的唯一合法途径
    }
}
```

Rust 的规则：**通过 `&T` 修改 `T` 的唯一合法方式是 `UnsafeCell`**（或基于它的安全包装如 `Cell`、`RefCell`、`Mutex`）。

### Drop 实现

```rust
impl<T> Drop for SpscQueue<T> {
    fn drop(&mut self) {
        // 消费掉队列中剩余的所有元素
        while self.try_pop().is_some() {}
    }
}
```

这保证了 `T` 实现 `Drop` 时不会泄漏资源。注意 `drop` 拿到 `&mut self`，此时已经是独占访问，不存在并发问题。

> **面试考点**
>
> Q: 为什么 SpscQueue 需要 `unsafe impl Sync`？直接不实现 Sync，用 `Arc<Mutex<SpscQueue>>` 不行吗？
>
> A: 技术上可以，但这违背了 SPSC 的设计目标。加 Mutex 就变成了锁保护的队列，失去了无锁的延迟优势。SPSC 的正确性由协议保证（单生产者 + 单消费者 + Release/Acquire 同步），而不是由锁保证。手动 `unsafe impl Sync` 是告诉编译器："我已经通过其他机制保证了线程安全，请信任我。"

---

## 本章小结

| 概念 | Tachyon 中的应用 |
|------|----------------|
| Wait-free SPSC | 引擎线程间通信的核心原语 |
| Release/Acquire | try_push/try_pop 的同步机制，x86 上零开销 |
| CachePadded | 防止 head/tail 伪共享，x86=64B, ARM=128B |
| 批量操作 | drain_batch 将 2N 原子操作减为 2 |
| MPSC = N×SPSC | 避免 CAS 争用，每个生产者独立队列 |
| UnsafeCell + unsafe impl Sync | 内部可变性 + 手动安全证明 |

**下一章**：[第六章：性能工程 — 从纳秒到微秒的极致优化](06-performance.md)
