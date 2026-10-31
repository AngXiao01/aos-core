# 交给版本 A 和版本 B

核心库到 §16.1 为止。人的界面、MCP、Agent Host 循环、ML 提供方和 iPhone app 不在这个仓库。

## 内存后端

`aos_branch::MemWorld` 已经能跑通 `aos-conformance` 里的 §16.1。测试是普通函数，版本仓库可以换上自己的 `LogStore` 和 `BlobStore` 再调用同一套检查。正确性不绑定某一种磁盘格式。

这次在本机（发布构建，开发确认器关闭）记下的性能：

| 操作 | 结果 | 规格 |
|---|---|---|
| 分叉 | < 1 ms | < 10 ms |
| 一次提交 1 万次对象写入 | 287 ms | < 2 s |
| 撤销这 1 万次写入 | 328 ms | < 2 s |
| 100 万对象的汇总变更单 | 914 ms | < 5 s |

这些数字来自 `cargo test -p aos-conformance --release --no-default-features --test perf -- --ignored`。性能试验默认不进正确性测试。

## trait

版本只实现这些 trait，不改语义：

| trait | crate | 版本要提供的东西 |
|---|---|---|
| `Clock` | `aos-types` | 当前 Unix 毫秒 |
| `Rng` | `aos-types` | 测试和标识用的随机字节 |
| `LogStore` | `aos-log` | 只追加的日志。`append` 必须拒绝序号、前哈希或条目哈希不对的写入。`hash_entry` 返回 `Result`，`Err` 也按哈希不对处理 |
| `BlobStore` | `aos-log` | 整块内容寻址。`put_chunked` 可以继续返回不支持，FastCDC 不是这一版的门 |
| `ObjectView` | `aos-types` | 查询要的读接口。内存世界的 `branch_view` 已经实现 |
| `ReplayGuard` | `aos-confirm` | 每个键只能用一次。过期直接拒绝。闸门传入的键是 `aos_egress::replay_key(nonce, 项序号)`，即 16 字节 nonce 后接 u32 大端序号，nonce 与过期时间都取自已签名的请求 |
| `EgressTransport` | `aos-egress` | **唯一**可以对外写或对外发送的实现。库里的 `RecordingTransport` 只记字节，不开网络 |

确认器验证吃 65 字节未压缩公钥和 64 字节 `r‖s` 签名。`execute_gate` 的第三个参数是配对时存下的 iPhone 公钥，它是审批和常设规则的唯一信任根；`Permit` 不带公钥，审批的通道、目标、载荷哈希、nonce、过期时间都从签名覆盖的请求字节里解析，`Permit::Allow` 要附上由意图对象算出的 `IntentFacts`（`facts_from_intent`），闸门据此重新评估策略。OS 给审批请求签名用 `aos_confirm::sign_request`。代替 iPhone 的软件签名只在 `aos-confirm` 的 `dev-confirmer` feature 里，并且 `not(debug_assertions)` 时编译失败。红色横幅由版本画，不由这个库画。

## 组装

依赖方向是：类型在底；日志、能力、策略、确认在其上；查询只读 `ObjectView`；分支在日志之上并调用查询做变更单下钻；动作不依赖分支；`aos-egress` 把动作宿主和闸门接到内存世界上。闸门不回调策略去发送，策略只返回 allow / confirm / deny。

宿主 ABI 版本是 `aos_action::ABI_VERSION = 1`，模块名 `aos`，九个导入是 `query`、`read`、`write`、`create`、`blob_read`、`blob_write`、`intent_create`、`job_submit`、`log`。参数是规范 CBOR。这个版本号从现在起给两个版本用。生产 crate 不依赖 wasmtime 或 wasmi；一致性测试在开发依赖里用 wasmi 证明外来 I/O 无法实例化。

## 字节

测试向量在 `vectors/cbor/` 和 `vectors/approval/`。审批决策的消息是规范 CBOR 请求字节后面直接接 UTF-8 的 `approve` 或 `deny`，再做一次 ECDSA P-256（SHA-256）。sys 模式在 `schemas/sys/`。没写死的选择和规格矛盾分别在 `DECISIONS.md`、`QUESTIONS.md`。

版本 A/B 的规格文件这次读不到。若内核不接受「alloc 无上限」，先改 trait 再改日志。
