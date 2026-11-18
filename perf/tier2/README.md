# Tier2 主链路接入（待办资产）

> 状态：**实现、语义对拍、lumino 侧 181 项测试全部通过；已在本地临时接入验证并还原**
> （2026-11-18 晚）。是否在 lumino-rs 正式落地由用户排期决定。
> 本目录保存参考实现与全部实测数据，接入时直接取用。

## 文件

- `document_build.rs.tier2`：`crates/audio/midi-model/src/document_build.rs` 的完整 Tier2 版本
  （逐轨 `_collect_` 并行 + 按轨序归并 + 归并前精确预分配）。**这是源文件内容，不是补丁**：
  接入时直接覆盖目标文件；另需给 `F:\lumino-rs\Cargo.toml` 的 `[patch.crates-io]`
  （已存在，含 xsynth 三行）**追加**一行：
  `lumino-midly = { path = "../midly-fork" }`（仅本地/临时；CI 无此路径，正式落地需改用 git rev 或上游合并）。
  - ⚠ `[patch.crates-io]` 已存在，不要再新建同名表（cargo 报 `duplicate key`）。

## 接入步骤（本地验证用，2026-11-18 已完整跑通）

1. 复制 `document_build.rs.tier2` → `F:\lumino-rs\crates\audio\midi-model\src\document_build.rs`
   （**复制后 touch mtime**，否则 Copy-Item 保留旧时间、cargo 不重编）。
2. 应用 **TrackName 免二次扫描**（见下；fork `TrackAllEvents.track_name` 已就绪）。
3. `F:\lumino-rs\Cargo.toml` → 在已存在的 `[patch.crates-io]` 追加 `lumino-midly` 一行。
4. `cd F:\lumino-rs; cargo test -p lumino-midi-model -p lumino-midi-loader`
   → 实测 **56 + 1 + 124 绿，5 ignored**（性能门控）。
5. 测量：`cd F:\midly-fork\perf; cargo build --release --features lumino --bin lumino-bench`
   （perf 自身 manifest 已有 patch，无需再动 lumino 的 Cargo.toml）。
6. 还原：`git checkout -- Cargo.toml Cargo.lock crates/audio/midi-model/src/document_build.rs`
   → 再 `cargo check -p lumino-midi-model` 确认干净可编译。

## TrackName 免二次扫描（已随本次验证一起接入）

`document_build.rs` 原调用 `scan::scan_track_names(file_bytes)`——对**没有 TrackName 的轨**
必须走完整条轨。164M 实测该扫描占 **1.28s**。接入方式（4 处改动）：

1. `struct TrackBuild` 增加字段
   `name: Option<String>`（首个 FF 03，已解码；空名称归一为 None，与旧 `scan_track_names` 一致）。
2. collect 闭包里：
   ```rust
   name: events.track_name.map(scan::decode_midi_text).filter(|n| !n.is_empty()),
   ```
3. 删除 `let track_names = scan::scan_track_names(file_bytes);`。
4. 归并循环里 `let mut track_names: Vec<Option<String>> = Vec::with_capacity(track_builds.len());`
   并 `track_names.push(track.name);`。

语义由 `lumino-bench digest` 验证：摘要包含 `doc.track_names`，Pi 与 164M **逐字段一致**。

## 实测数据（同机、同批交替；2026-11-18 晚，fork `c0dbb31`）

| 素材 | 接入前 median / min | 接入后 median / min | 峰值变化 |
|---|---|---|---|
| Pi.mid（24MB / 30 轨 / 3.14M 音符） | 16.74 / 15.98ms | **8.25 / 7.37ms（-51% / -54%）** | 78.9 → 80.7MB（+2.3%） |
| **StarT 164M（1.25GB / 826 轨 / 164,206,462 音符）** | **2818 / 2807ms** | **783 / 736ms（-72% / -74%）** | 3306 → 3424MB（+3.5%） |
| StarT210k（217k 控制事件） | 5.11 / 4.60ms | 5.48 / 5.12ms（+7%，归并串行暴露） | +3.5% |
| Clementi（4KB） | 0.041 / 0.032ms | 0.042 / 0.032ms（持平） | 小幅 |

- 早期数据（无 track_name、无容量修复）：Pi 19.27→13.50ms、164M 4358→2261ms。
  本轮数字同时包含 **Tier2 + track_name + P4 容量预估（fork）** 三项收益。
- 164M 测量时机器负载 56%→17% 波动，绝对值偏高；但前后差距 3.8×，远超噪声。
  峰值 +3.5% 与早期 +1.7% 的差异大概率是分配器/工作集波动，落地前建议在空闲窗口复测一次。

语义对拍（全部逐字段一致）：接入前后 Pi + 164M（摘要含 track_names）；
摘要留档：`perf/results/tier2-digests/164m-final-*.txt`；原始数据：`perf/results/p4-a1-upper-bounds.tsv`。

## 后续优化（已量化，尚未接入）

**（按 2026-11-18 的 A4 分阶段数据重排优先级）**

1. **巨轨的排序 + ChunkedList 构建（最大杠杆）**：164M 的 765ms 里，逐轨 handler 占 3095ms CPU
   （排序 62% + ChunkedList 32%），且 3 条 800 万音符级轨单独构成 **450ms 临界路径**
   （= 端到端墙钟 59%）。排序是内存受限的（14 字节 packed 记录、~123MB/轨）。
   可行方向：分块排序 + k 路归并、键值分离（8 字节 key）排序、或把 track 内并行与排序合并设计。
   注意：**P5（轨内并行解析）单独做收益有限**——解析只占 CPU 42%，排序/构建仍是单线程。
2. **控制事件 k-way merge：已无价值**（实测控制/文本排序合计 1.1ms），从待办移除。
3. **ChunkedList 零拷贝**：若 midly 能直接写入 chunk 存储（sink/visitor API），可省 1002ms CPU；
   属 API 级改动，需单独立项。

## 注意

- `[patch] path` 与 `document_build.rs` 改动均为**本地临时**，禁止提交到 lumino-rs（用户会重置工作区）。
- 性能测量前确认机器空闲（CPU 负载 100% 时墙钟可翻倍，已多次踩中）。
- `lumino-bench` 的 `[lumino]` 行输出到 stderr；PowerShell 管道偶尔丢行（脚本会打印 PARSE FAIL），
  单次失败不代表进程失败，手动复跑确认即可。
