# Tachyon 高性能撮合引擎 — 完整教程与面试指南

> **Tachyon** 是一个用 Rust 从零构建的高性能交易撮合引擎，灵感来自 Hyperliquid 架构。
> 当前状态：**223+ 测试全部通过**，撮合延迟 **<100ns 中位数**，单品种吞吐 **>5M ops/sec**。

---

## 目录

### 第一部分：架构基础

| 章节 | 标题 | 核心知识点 |
|------|------|-----------|
| [第1章](./01-architecture.md) | 系统架构总览 | crate 依赖图、线程模型、数据流、设计哲学 |
| [第2章](./02-core-types.md) | 核心类型深度解析 | newtype pattern、`#[repr(C)]`、定点数、枚举设计 |

### 第二部分：数据结构与算法

| 章节 | 标题 | 核心知识点 |
|------|------|-----------|
| [第3章](./03-orderbook.md) | 订单簿实现 | BTreeMap + Slab + 侵入式链表、BBO 缓存、价格级别管理 |
| [第4章](./04-matching.md) | 撮合引擎 | 价格-时间优先、Limit/Market/IOC/FOK/PostOnly、STP |

### 第三部分：并发与性能

| 章节 | 标题 | 核心知识点 |
|------|------|-----------|
| [第5章](./05-lock-free-io.md) | 无锁通信 | SPSC 环形缓冲区、MPSC 队列、cache line padding、内存序 |
| [第6章](./06-performance.md) | 极致性能优化 | SmallVec、mimalloc、CPU 亲和性、零分配热路径 |

### 第四部分：系统集成

| 章节 | 标题 | 核心知识点 |
|------|------|-----------|
| [第7章](./07-integration.md) | 系统集成 — 从请求到响应的完整链路 | 线程模型、SPSC 桥接、REST/WS/TCP 网关、认证限流、优雅关闭 |
| [第8章](./08-persistence.md) | 持久化与恢复 | WAL (CRC32 + rotation)、原子快照、崩溃恢复 |

### 第五部分：面试实战

| 章节 | 标题 | 核心知识点 |
|------|------|-----------|
| [第9章](./09-interview.md) | 面试指南 — 高频交易系统面试全攻略 | 系统设计、数据结构、并发、OS/硬件、Rust 特定、项目讲解 |

---

## 快速导航：按知识点分类

### 数据结构
- [BTreeMap 用于有序价格级别](./03-orderbook.md#btreemap-价格索引) — 为什么不用 HashMap
- [Slab 分配器实现 O(1) 分配/释放](./03-orderbook.md#slab-分配器) — 对比 Vec、Arena
- [侵入式双向链表 (u32 prev/next)](./03-orderbook.md#侵入式链表) — 为什么不用 `LinkedList<T>`
- [定点数表示 (Price/Quantity)](./02-core-types.md#price-定点价格) — 为什么不用浮点数

### 并发编程
- [Wait-free SPSC 环形缓冲区](./05-lock-free-io.md#spsc-队列) — 仅需 Release/Acquire
- [Cache line padding 防止 false sharing](./05-lock-free-io.md#false-sharing) — 为什么 head 和 tail 要分开
- [per-symbol 线程模型](./01-architecture.md#线程模型) — 无锁热路径

### 系统设计
- [Gateway → Bridge → SPSC → Engine 数据流](./01-architecture.md#数据流) — 异步到同步的桥接
- [WAL + Snapshot 崩溃恢复](./08-persistence.md) — 确定性重放
- [事件溯源 (Event Sourcing)](./01-architecture.md#设计哲学) — 所有状态变化表达为事件

### 性能优化
- [SmallVec 避免堆分配](./06-performance.md#smallvec) — 栈上小缓冲区
- [mimalloc 全局分配器](./06-performance.md#mimalloc) — 替换系统 malloc
- [CPU 亲和性绑核](./06-performance.md#cpu-亲和性) — 减少上下文切换
- [`#[repr(C)]` 内存布局控制](./02-core-types.md#order-订单结构体) — 消除编译器 padding

---

## 项目结构

```
thelight/
├── Cargo.toml                    # workspace 根配置
├── crates/
│   ├── tachyon-core/             # 核心类型：Price, Quantity, Order, Event
│   ├── tachyon-book/             # 订单簿：BTreeMap + Slab + 侵入式链表
│   ├── tachyon-engine/           # 撮合引擎：Matcher + STP + RiskManager
│   ├── tachyon-io/               # 无锁队列：SPSC + MPSC
│   ├── tachyon-proto/            # 协议定义 (protobuf)
│   ├── tachyon-gateway/          # 网关：REST + WebSocket + TCP + Bridge
│   ├── tachyon-persist/          # 持久化：WAL + Snapshot + Recovery
│   └── tachyon-bench/            # 性能基准测试
├── bin/
│   └── tachyon-server/           # 服务器入口：线程编排、配置、指标
└── docs/
    └── tutorial/                 # 本教程
```

---

## 性能指标

| 指标 | 目标 | 说明 |
|------|------|------|
| 撮合延迟（中位数） | <100ns | 纯撮合逻辑，不含网络 I/O |
| 撮合延迟（P99） | <10us | 包含复杂订单类型 |
| 撮合延迟（P99.9） | <50us | 极端情况 |
| 单品种吞吐 | >=5M ops/sec | 单线程处理 |
| 总吞吐 | >=20M ops/sec | 多品种并行 |
| 内存占用 | <1GB | 单品种 100 万订单 |

---

## 开始阅读

如果你是 Rust 初学者，建议从 [第2章：核心类型](./02-core-types.md) 开始，理解基础类型后再进入架构和数据结构章节。

如果你已有系统编程经验，建议从 [第1章：系统架构](./01-architecture.md) 开始，自上而下理解全局设计。

如果你在准备面试，可以直接跳到 [第9章：面试指南](./09-interview.md)，然后按需回溯深入特定章节。
