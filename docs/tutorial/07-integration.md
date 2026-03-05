# 第七章：系统集成 — 从请求到响应的完整链路

> **源码路径**: `bin/tachyon-server/src/main.rs`, `crates/tachyon-gateway/src/`
>
> **前置阅读**: [第5章：无锁通信](./05-lock-free-io.md), [第6章：极致性能优化](./06-performance.md)

本章将把前面几章介绍的各个组件——订单簿、撮合引擎、无锁队列——串联成一个完整的生产系统。我们将跟踪一个下单请求从 HTTP 到达、经过异步-同步桥接、进入引擎处理、再返回响应的完整链路。

---

## 目录

- [7.1 线程模型 — 为什么不用 tokio 处理撮合](#71-线程模型--为什么不用-tokio-处理撮合)
- [7.2 SPSC 桥接 — 异步网关到同步引擎](#72-spsc-桥接--异步网关到同步引擎)
- [7.3 REST 网关 — axum 路由与请求验证](#73-rest-网关--axum-路由与请求验证)
- [7.4 WebSocket 实时推送](#74-websocket-实时推送)
- [7.5 TCP 二进制协议](#75-tcp-二进制协议)
- [7.6 认证与限流](#76-认证与限流)
- [7.7 共享状态管理](#77-共享状态管理)
- [7.8 优雅关闭](#78-优雅关闭)
- [7.9 完整请求流程图](#79-完整请求流程图)

---

## 7.1 线程模型 — 为什么不用 tokio 处理撮合

Tachyon 采用**混合线程模型**：网关层运行在 tokio 异步运行时上，而撮合引擎运行在**专用 OS 线程**上。

```
                    ┌─ tokio runtime ──────────────────┐
                    │  REST server (axum)               │
                    │  WebSocket server                 │
                    │  TCP binary server                │
                    └─────────┬────────────────────────┘
                              │ SPSC queue (per symbol)
                    ┌─────────▼────────────────────────┐
                    │  OS thread: engine-0 (BTCUSDT)    │ ← CPU core 1
                    │  OS thread: engine-1 (ETHUSDT)    │ ← CPU core 2
                    │  OS thread: engine-2 (SOLUSDT)    │ ← CPU core 3
                    └──────────────────────────────────┘
```

**为什么撮合不用 tokio？**

| 因素 | tokio 任务 | 专用 OS 线程 |
|------|-----------|-------------|
| 调度延迟 | 微秒级（work-stealing） | 无调度开销 |
| 抢占 | 其他任务可能抢占 | 独占 CPU 核心 |
| cache 效率 | 任务可能在不同核心执行 | 数据始终在同一 L1/L2 |
| 延迟确定性 | 受 I/O 任务影响 | 可预测的纳秒级延迟 |

在 `main.rs` 中，每个品种对应一个专用线程：

```rust
// bin/tachyon-server/src/main.rs:345-391
let handle = std::thread::Builder::new()
    .name(format!("engine-{}", symbol_id))
    .spawn(move || {
        // 绑定 CPU 核心
        if let Some(core_id) = pin_core {
            core_affinity::set_for_current(core_id);
        }
        engine_thread_loop(ctx);
    })
    .unwrap();
```

### CPU 亲和性绑核

引擎线程会跳过核心 0（留给 OS），使用 round-robin 分配到剩余核心：

```rust
// main.rs:339-343
let pin_core = if core_ids.len() > 1 {
    Some(core_ids[1 + (idx % (core_ids.len() - 1))])
} else {
    None
};
```

这确保了每个引擎线程独占一个物理核心，L1/L2 cache 中的订单簿数据不会被其他线程淘汰。

### 引擎主循环

引擎线程运行一个紧凑的 spin-loop，批量读取命令并处理：

```rust
// main.rs:610-637 (简化)
fn engine_thread_loop(mut ctx: EngineThreadCtx) {
    loop {
        let mut batch = ArrayVec::<EngineCommand, 64>::new();
        ctx.queue.drain_batch(&mut batch);

        if batch.is_empty() {
            if ctx.shutdown.load(Ordering::Acquire) {
                // 退出前再排空一次
                ctx.queue.drain_batch(&mut batch);
                if batch.is_empty() { break; }
            } else {
                // 渐进式退让：先 spin，再 yield
                spin_count += 1;
                if spin_count < 1000 {
                    std::hint::spin_loop();
                } else {
                    std::thread::yield_now();
                    spin_count = 0;
                }
                continue;
            }
        }

        for cmd in batch {
            // WAL 写入 → 撮合 → 更新共享状态 → 快照
        }
    }
}
```

关键设计点：
- **批量处理**: `drain_batch` 一次取出最多 64 条命令，摊薄循环开销
- **渐进退让**: 空闲时先用 `spin_loop()` 自旋（保持 CPU 热），超过 1000 次后 `yield_now()` 让出时间片
- **关闭排空**: 收到 shutdown 信号后，再做一次 drain 确保不丢命令

---

## 7.2 SPSC 桥接 — 异步网关到同步引擎

`EngineBridge` 是连接 tokio 异步世界和引擎同步世界的桥梁。

```
  Gateway (async/tokio)          Bridge           Engine (sync/OS thread)
  ┌──────────────┐         ┌──────────────┐     ┌──────────────┐
  │ REST handler │───push──▶│ SPSC Queue   │──pop─▶│ engine_loop  │
  │              │         │ (per symbol) │     │              │
  │ await rx ... │◀────────│ oneshot::tx  │◀────│ tx.send(evts)│
  └──────────────┘         └──────────────┘     └──────────────┘
```

### EngineCommand 结构

```rust
// crates/tachyon-gateway/src/bridge.rs:11-17
pub struct EngineCommand {
    pub command: Command,          // PlaceOrder / CancelOrder / ModifyOrder
    pub symbol: Symbol,            // 目标品种
    pub account_id: u64,           // 账户 ID
    pub timestamp: u64,            // 纳秒时间戳
    pub response_tx: oneshot::Sender<Vec<EngineEvent>>,  // 回调通道
}
```

每个命令携带一个 tokio `oneshot` 通道的发送端。引擎处理完毕后，通过这个通道将结果返回给等待中的 gateway handler。

### 发送流程与背压

```rust
// bridge.rs:58-98
pub async fn send_order(&self, command: Command, symbol: Symbol, account_id: u64)
    -> Result<Vec<EngineEvent>, BridgeError>
{
    let queue = self.queues.get(&symbol.raw())
        .ok_or(BridgeError::UnknownSymbol)?;

    let (tx, rx) = oneshot::channel();
    let cmd = EngineCommand { command, symbol, account_id, timestamp, response_tx: tx };

    // 带超时的 spin-push：如果队列满了，yield 给 tokio 而不是阻塞
    let deadline = tokio::time::Instant::now() + Duration::from_millis(100);
    let mut cmd = cmd;
    loop {
        match queue.try_push(cmd) {
            Ok(()) => break,
            Err(returned) => {
                cmd = returned;
                if tokio::time::Instant::now() >= deadline {
                    return Err(BridgeError::QueueFull);
                }
                tokio::task::yield_now().await;  // 不阻塞 executor
            }
        }
    }

    rx.await.map_err(|_| BridgeError::ResponseDropped)
}
```

关键设计：
1. **不用 `block_on`**: 在异步上下文中使用 `yield_now()` 而非阻塞等待，避免饿死 tokio worker
2. **100ms 超时**: 防止队列满时无限等待，返回 `QueueFull` 错误码（对应 HTTP 503）
3. **per-symbol 队列**: 不同品种的命令不会互相阻塞

### 为什么选择 SPSC 而不是 MPSC？

虽然多个 REST handler 同时发送命令看起来需要 MPSC，但实际上 SPSC 已经足够：
- `SpscQueue::try_push` 是线程安全的（基于原子操作的单生产者-单消费者）
- Gateway 层通过 axum 框架序列化了对同一品种 bridge 的访问
- SPSC 比 MPSC 更快：只需要 Release/Acquire 而非 CAS 循环

---

## 7.3 REST 网关 — axum 路由与请求验证

REST API 基于 axum 框架构建，提供完整的交易接口。

### 路由表

```rust
// crates/tachyon-gateway/src/rest.rs:53-68
pub fn rest_router(state: AppState) -> Router {
    Router::new()
        .route("/api/v1/orderbook/:symbol", get(get_orderbook))
        .route("/api/v1/trades/:symbol", get(get_trades))
        .route("/api/v1/history/trades/:symbol", get(get_trade_history))
        .route("/api/v1/klines/:symbol", get(get_klines))
        .route("/api/v1/order", post(place_order))
        .route("/api/v1/order/:id", delete(cancel_order))
        .route("/api/v1/symbols", get(list_symbols))
        .route("/api/v1/metrics", get(get_metrics_json))
        .route("/api/v1/status", get(get_status))
        .route("/metrics", get(get_metrics_prometheus))
        .route("/health", get(health_check))
        .layer(DefaultBodyLimit::max(1024 * 1024))  // 1MB body limit
        .with_state(state)
}
```

### 价格解析 — 字符串到定点数

REST API 接收字符串格式的价格和数量（如 `"50000.50"`），需要转换为内部定点数表示：

```rust
// rest.rs 中的 parse_scaled_value（简化）
fn parse_scaled_value(s: &str, scale: u8) -> Option<u64> {
    // "50000.50" with scale=2 → 5000050
    let parts: Vec<&str> = s.split('.').collect();
    let whole: u64 = parts[0].parse().ok()?;
    let frac: u64 = if parts.len() > 1 {
        // 对齐到 scale 位小数
        let frac_str = &parts[1][..parts[1].len().min(scale as usize)];
        frac_str.parse::<u64>().ok()? * 10u64.pow((scale as u32) - frac_str.len() as u32)
    } else {
        0
    };
    Some(whole * 10u64.pow(scale as u32) + frac)
}
```

### 下单请求的完整验证链

```
JSON request → parse symbol → parse side → parse order_type
→ parse time_in_force → parse quantity → validate qty limits
→ parse price → validate price limits → validate tick size
→ check engine alive → build Command → bridge.send_order()
→ await response → format JSON response
```

每一步失败都会返回具体的错误信息和 `x-request-id` 头，方便调试。

### 取消订单的路由问题

取消订单需要知道该订单属于哪个品种（才能路由到正确的引擎线程），但客户端只提供 order_id。解决方案是维护一个全局 `order_registry`：

```rust
// order_registry: Arc<RwLock<HashMap<u64, Symbol>>>
// 下单时注册:
order_registry.write().insert(order_id, symbol);
// 取消时查找:
let symbol = order_registry.read().get(&order_id).copied();
```

---

## 7.4 WebSocket 实时推送

WebSocket 提供实时市场数据推送，支持三种频道：`trades@SYMBOL`、`depth@SYMBOL`（也接受 `orderbook@`）、`ticker@SYMBOL`。

### 架构

```
Engine thread                   WsState                     Client
    │                              │                            │
    │  broadcast::send(Trade)      │                            │
    ├─────────────────────────────▶│                            │
    │                              │  if subscribed("trades@")  │
    │                              ├───────────────────────────▶│
    │                              │                            │
    │  broadcast::send(Depth)      │                            │
    ├─────────────────────────────▶│  if subscribed("depth@")   │
    │                              ├───────────────────────────▶│
```

使用 tokio 的 `broadcast` channel 实现一对多推送：

```rust
// crates/tachyon-gateway/src/ws.rs:17-24
pub struct WsState {
    pub event_tx: broadcast::Sender<WsServerMessage>,
    pub active_connections: Arc<AtomicU64>,
    pub max_connections: u64,
}
```

### 订阅管理

每个 WebSocket 连接维护自己的订阅集合（`HashSet<String>`），只接收订阅的频道数据：

```rust
// ws.rs:244-263
fn should_deliver(msg: &WsServerMessage, subscriptions: &HashSet<String>) -> bool {
    match msg {
        WsServerMessage::Trade { data } => {
            subscriptions.contains(&format!("trades@{}", data.symbol))
        }
        WsServerMessage::Depth { data } => {
            subscriptions.contains(&format!("depth@{}", data.symbol))
        }
        WsServerMessage::Ticker { data } => {
            subscriptions.contains(&format!("ticker@{}", data.symbol))
        }
        _ => false,
    }
}
```

支持两种客户端消息格式：
- **旧版单频道**: `{"type":"subscribe","channel":"trades@BTCUSDT"}`
- **批量格式**: `{"method":"subscribe","params":["trades@BTCUSDT","depth@BTCUSDT"]}`

### 慢消费者处理

如果客户端消费速度跟不上引擎的推送速率，`broadcast` channel 会报告 `Lagged`。Tachyon 的策略是：

```rust
// ws.rs:116-132
Err(broadcast::error::RecvError::Lagged(n)) => {
    lag_count += 1;
    // 通知客户端丢消息了
    let warning = json!({"type":"warning","message":format!("lagged: {} messages dropped", n)});
    sender.send(Message::Text(warning.to_string())).await;
    // 连续 3 次 lag 就断开
    if lag_count >= 3 {
        sender.send(Message::Close(None)).await;
        break;
    }
}
```

这比 Binance 的做法（静默丢弃）更友好——客户端知道自己落后了，可以主动降速或重连。

### 连接数限制

使用原子操作的 increment-then-check 模式避免 TOCTOU 竞态：

```rust
// ws.rs:51-56
let prev = state.active_connections.fetch_add(1, Ordering::AcqRel);
if prev >= state.max_connections {
    state.active_connections.fetch_sub(1, Ordering::AcqRel);  // 回滚
    return StatusCode::SERVICE_UNAVAILABLE.into_response();
}
```

先增后检查，超限则回退。这比 "先检查再增加" 更安全——不会因竞态导致超限。

---

## 7.5 TCP 二进制协议

除了 REST + WebSocket，Tachyon 还提供 TCP 二进制协议，面向对延迟极度敏感的量化交易客户端。

### Wire Format

```
┌──────────────────────────────────────┐
│ Frame                                 │
├──────────┬──────────┬────────────────┤
│ seq_num  │ msg_type │ payload        │
│ (u64 LE) │ (u8)     │ (variable)     │
└──────────┴──────────┴────────────────┘
```

使用 `tokio_util::codec::Framed` 封装 TCP 流，`ServerCodec` 负责帧的编解码。

### 关键优化 — 禁用 Nagle 算法

```rust
// crates/tachyon-gateway/src/tcp.rs:84
let _ = stream.set_nodelay(true);
```

Nagle 算法会缓冲小包合并发送，减少网络开销但增加延迟。对交易系统而言，每一微秒都很关键，所以必须禁用。

### 消息处理流程

TCP 服务器支持四种客户端消息：

```rust
// tcp.rs:111-233
match client_frame.message {
    ClientMessage::NewOrder(new_order) => {
        // 转换为 core::Order → bridge.send_order()
    }
    ClientMessage::CancelOrder(cancel) => {
        // 查 order_registry → bridge.send_cancel()
    }
    ClientMessage::ModifyOrder(modify) => {
        // 查 order_registry → bridge.send_order(ModifyOrder)
    }
    ClientMessage::Heartbeat => {
        // 直接返回 Heartbeat 帧
    }
}
```

注意 Cancel 和 Modify 操作也通过 `order_registry` 查找真实的 symbol，不信任客户端提供的 `symbol_id`——这是安全设计。

---

## 7.6 认证与限流

### API Key 认证

```rust
// crates/tachyon-gateway/src/auth.rs:63-94
pub async fn auth_middleware(
    State(auth): State<AuthState>,
    request: Request<Body>,
    next: Next,
) -> Response {
    if !auth.config.enabled { return next.run(request).await; }

    // 公开端点跳过认证
    if is_public_path(request.uri().path()) {
        return next.run(request).await;
    }

    // 检查 X-API-Key 头
    match request.headers().get("x-api-key") {
        Some(value) if auth.config.api_keys.contains(value.to_str().unwrap()) => {
            next.run(request).await
        }
        _ => StatusCode::UNAUTHORIZED.into_response(),
    }
}
```

公开端点列表：
- `/health` — 健康检查
- `/metrics` — Prometheus 指标
- `/api/v1/symbols` — 品种列表
- `/api/v1/orderbook/` — 订单簿深度
- `/api/v1/trades/` — 最近成交

### 令牌桶限流

使用经典的 Token Bucket 算法，per-IP 隔离：

```rust
// crates/tachyon-gateway/src/rate_limit.rs:40-72
struct TokenBucket {
    tokens: f64,
    last_refill: Instant,
}

impl TokenBucket {
    fn try_consume(&mut self, rate: u32, burst: u32) -> bool {
        self.refill(rate, burst);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }

    fn refill(&mut self, rate: u32, burst: u32) {
        let elapsed = now.duration_since(self.last_refill).as_secs_f64();
        self.tokens = (self.tokens + elapsed * rate as f64).min(burst as f64);
    }
}
```

| 参数 | 含义 | 默认值 |
|------|------|--------|
| `requests_per_second` | 稳态速率 | 100 |
| `burst_size` | 桶容量（允许的突发量） | 200 |

例如配置 100 rps + 200 burst：客户端可以瞬间发 200 个请求，之后稳定在 100/秒。超出返回 HTTP 429。

### 中间件顺序

```rust
// main.rs:451-459
let rest_app = rest_router(app_state)
    .layer(auth_middleware)       // 先认证
    .layer(rate_limit_middleware) // 再限流
```

axum 的 layer 是洋葱模型，**后加的先执行**。所以请求流经顺序是：

```
→ rate_limit_middleware（先拒绝过多请求）
  → auth_middleware（再验证身份）
    → route handler（最后处理业务）
```

限流在认证前执行，这样恶意请求不需要解析 API key 就被拦截。

---

## 7.7 共享状态管理

引擎线程（sync）和 REST handler（async）之间需要共享市场数据。Tachyon 使用 `std::sync::RwLock`（而非 `tokio::sync::RwLock`）：

```rust
// main.rs:277-281
let recent_trades: Arc<RwLock<HashMap<String, Vec<TradeResponse>>>> = ...;
let book_snapshots: Arc<RwLock<HashMap<String, OrderBookResponse>>> = ...;
let order_registry: Arc<RwLock<HashMap<u64, Symbol>>> = ...;
```

**为什么用 `std::sync::RwLock` 而非 `tokio::sync::RwLock`？**

- 引擎线程不在 tokio 运行时上，无法使用 tokio 的异步锁
- `std::sync::RwLock` 在持有时间极短的场景下性能更好（无 async 调度开销）
- 引擎线程只做写操作（更新最新数据），REST handler 只做读操作

### RwLock Poison 恢复

如果持有锁的线程 panic，`std::sync::RwLock` 会进入 "poisoned" 状态。Tachyon 选择恢复而非传播 panic：

```rust
// main.rs:689-691
ctx.order_registry
    .write()
    .unwrap_or_else(|p| p.into_inner())  // poison 恢复
    .insert(order_id.raw(), *symbol);
```

`into_inner()` 提取被 poisoned 锁保护的数据——在撮合引擎中，数据的一致性比 panic 传播更重要。

---

## 7.8 优雅关闭

Tachyon 的关闭流程遵循严格的顺序，确保不丢数据：

```
1. Ctrl+C / SIGTERM
   │
2. 停止接受新连接 (tokio::select! 退出)
   │
3. 设置 shutdown flag (AtomicBool)
   │
4. 等待引擎线程排空队列并退出 (with timeout)
   │
5. 关闭 trade history writer
   │
6. Flush WAL
   │
7. 退出
```

```rust
// main.rs:527-578
// 1. 等待 shutdown 信号
_ = tokio::signal::ctrl_c() => {
    tracing::info!("Received shutdown signal");
}

// 2. 通知引擎线程
shutdown.store(true, Ordering::Release);

// 3. 等待引擎线程，带超时
let engine_join = tokio::task::spawn_blocking(move || {
    for handle in engine_threads { handle.join(); }
});
tokio::select! {
    _ = engine_join => { /* 正常退出 */ }
    _ = tokio::time::sleep_until(shutdown_deadline) => {
        tracing::warn!("Shutdown timeout reached, forcing exit");
    }
}

// 4. 关闭 trade writer
drop(trade_tx);  // 关闭通道，writer 收到 None 后退出

// 5. Flush WAL
if let Ok(ref mut writer) = wal.lock() {
    writer.flush();
}
```

关键点：
- 引擎线程收到 `shutdown` 后会再做一次 `drain_batch`，确保队列中的命令不丢
- 使用 `spawn_blocking` 在 tokio 中等待 OS 线程 join
- WAL flush 在最后执行，确保所有已处理的命令都持久化了

---

## 7.9 完整请求流程图

以一个 REST 下单请求为例，完整的数据流：

```
Client                     Tachyon Server
  │                             │
  │  POST /api/v1/order         │
  │────────────────────────────▶│
  │                             │
  │                    ┌────────┴──────────┐
  │                    │ rate_limit_middleware│ ← Token Bucket 检查
  │                    │ auth_middleware      │ ← X-API-Key 验证
  │                    │ place_order handler  │
  │                    │   │                  │
  │                    │   ▼                  │
  │                    │ parse JSON           │
  │                    │ validate symbol      │
  │                    │ validate side        │
  │                    │ parse price (str→i64)│
  │                    │ parse qty (str→u64)  │
  │                    │ check limits         │
  │                    │   │                  │
  │                    │   ▼                  │
  │                    │ EngineBridge         │
  │                    │   │                  │
  │                    │   │ SPSC.try_push()  │
  │                    │   │                  │
  │                    └───┼──────────────────┘
  │                        │
  │             ┌──────────▼──────────────┐
  │             │ Engine Thread (OS)       │
  │             │   │                      │
  │             │   ▼                      │
  │             │ WAL.append_command()     │  ← 先写日志
  │             │   │                      │
  │             │   ▼                      │
  │             │ engine.process_command() │  ← 再撮合
  │             │   │                      │
  │             │   ▼                      │
  │             │ update book_snapshots    │
  │             │ update recent_trades     │
  │             │ ws_event_tx.send()       │  ← WebSocket 推送
  │             │   │                      │
  │             │   ▼                      │
  │             │ oneshot::tx.send(events) │  ← 返回结果
  │             └──────────┬──────────────┘
  │                        │
  │                    ┌───┼──────────────────┐
  │                    │   ▼                  │
  │                    │ rx.await             │
  │                    │ format response JSON │
  │                    └────────┬─────────────┘
  │                             │
  │  200 OK + JSON              │
  │◀────────────────────────────│
  │                             │
```

**时间分解**（典型值）：

| 阶段 | 耗时 |
|------|------|
| HTTP 解析 + 验证 | ~10us |
| SPSC push + 等待 | ~1-5us |
| WAL 写入 | ~1-10us (取决于 fsync 策略) |
| 撮合引擎处理 | **<100ns** |
| 结果返回 + JSON 序列化 | ~5us |
| **端到端 (REST)** | **~20-50us** |

注意撮合本身只需 <100ns，大部分时间花在网络 I/O 和序列化上。这就是为什么专业量化用 TCP 二进制协议而非 REST。

---

## 本章小结

| 组件 | 技术选择 | 理由 |
|------|---------|------|
| 网关 | tokio + axum | 高效处理大量并发连接 |
| 引擎 | 专用 OS 线程 | 可预测的纳秒级延迟 |
| 桥接 | SPSC + oneshot | 无锁、低延迟 |
| 实时推送 | broadcast channel | 一对多广播 |
| 二进制协议 | TCP + Framed codec | 最低延迟 |
| 认证 | axum middleware | 可插拔、声明式 |
| 限流 | Token Bucket per-IP | 允许突发、公平 |
| 共享状态 | std::sync::RwLock | 兼容同步引擎线程 |

---

> **下一章**: [第八章：持久化与恢复 — 确定性重放](./08-persistence.md)
