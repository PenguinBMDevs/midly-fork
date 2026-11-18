# Tier2 主链路接入（待办资产）

> 状态：**已完成实现与验证，但因 lumino-rs 侧排期（等 midly 全部改动完成后再合并）暂未落地**。
> 本目录保存参考实现与全部实测数据，供后续接入时直接取用。

## 文件

- `document_build.rs.tier2`：`crates/audio/midi-model/src/document_build.rs` 的完整 Tier2 版本
  （逐轨 `_collect_` 并行 + 按轨序归并 + 归并前精确预分配）。**这是源文件内容，不是补丁**：
  接入时直接覆盖目标文件，并给 `F:\lumino-rs\Cargo.toml` 的 `[patch.crates-io]` 加：
  `lumino-midly = { path = "../midly-fork" }`（仅本地/临时；CI 无此路径，正式落地需改用 git rev 或上游合并）。

## 接入步骤（本地验证用）

1. 复制 `document_build.rs.tier2` → `F:\lumino-rs\crates\audio\midi-model\src\document_build.rs`
2. `F:\lumino-rs\Cargo.toml` → `[patch.crates-io]` 增加 `lumino-midly = { path = "../midly-fork" }`
3. `cd F:\lumino-rs; cargo test -p lumino-midi-model -p lumino-midi-loader`
   （期望 56 + 1 + 124 绿；5 ignored 为性能门控）
4. 测量：`cd F:\midly-fork\perf; cargo build --release --features lumino --bin lumino-bench`
5. 还原：`git checkout -- Cargo.toml Cargo.lock crates/audio/midi-model/src/document_build.rs`

## 实测数据（同机、同批交替）

| 素材 | 接入前 median / min | 接入后 median / min | 峰值变化 |
|---|---|---|---|
| Pi.mid（24MB / 30 轨 / 3.14M 音符） | 19.27 / 18.53ms | **13.50 / 11.37ms**（-30% / -39%） | 79.9 → 85.4MB（+7%） |
| **StarT 164M（1.25GB / 826 轨 / 164,206,462 音符）** | **4358 / 3901ms** | **2261 / 2218ms（-48%）** | **3404 → 3461MB（+1.7%）** |
| StarT210k（217k 控制事件） | 5.11 / 4.60ms | 5.48 / 5.12ms（+7%，归并串行暴露） | +3.5% |
| Clementi（4KB） | 0.041 / 0.032ms | 0.042 / 0.032ms（持平） | 小幅 |

语义对拍（全部逐字段一致）：接入前后 5 素材 + 164M；0.6.5 vs fork 5 素材。
摘要留档：`perf/results/tier2-digests/`；原始数据：`perf/results/tier2-real-pipeline.tsv`。

## 后续优化（已量化，尚未接入）

**TrackName 免二次扫描**：`document_build.rs` 当前调用 `scan::scan_track_names(file_bytes)`，
它对**没有 TrackName 的轨**必须走完整条轨。164M 素材实测该扫描占 **1.28s**（2261ms → 978ms，
-57%）。fork 已新增 `TrackAllEvents.track_name: Option<&[u8]>`（首个 FF 03，原始字节），
接入时把

```rust
let track_names = scan::scan_track_names(file_bytes);
```

替换为在 collect 闭包里收集（`TrackBuild.name = events.track_name.map(scan::decode_midi_text).filter(|n| !n.is_empty())`），
归并时按轨序 push 即可。预计 164M 端到端 **2261 → ~1000ms**。

**控制事件 k-way merge**：`control_events` 先按轨归并、再 `sort_by_key(tick)` 全排序；
每轨控制事件本就按 tick 有序，改 k 路归并可收回 StarT 的 +7%。

## 注意

- `[patch] path` 与 `document_build.rs` 改动均为**本地临时**，禁止提交到 lumino-rs。
- 性能测量前确认机器空闲（CPU 负载 100% 时墙钟可翻倍，本会话已实测踩中）。
