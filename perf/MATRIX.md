# P2 性能矩阵

`perf/matrix.ps1`：版本 × parallel/serial × 线程 × 素材，一键复现 + 记录环境。

```powershell
# 全矩阵（fork 支持 serial；基线自动跳过 serial，见下）
.\perf\matrix.ps1 -Versions fork,0.6.5,0.6.4,0.6.3 -Modes parallel,serial `
    -Threads 1,8,16 -Materials pi,clementi,control,sysex,text -Runs 15 -Warmup 3

# 复用已缓存二进制
.\perf\matrix.ps1 -NoBuild -Materials pi,text
```

- 二进制缓存：`perf/target/matrix/<version>-<mode>.exe`
- 输出：`perf/results/matrix-<ts>.tsv`（列见 `bench::tsv_header`）+ `.meta.txt`
  （CPU / 逻辑核 / 起始负载 / commit / rustc / 构建参数）
- **交错执行**：同一 (素材, 线程) 下各版本背靠背跑。非交错时机器漂移会压过版本差异
  （实测同一批数据：顺序执行时 fork t8=9.9ms、0.6.3=9.7ms；交错后 fork=7.1、0.6.3=10.0）。
- 基线：`F:\midly-baselines\lumino-midly-<ver>/`。脚本把基线复制到
  `perf/target/matrix/base-<ver>/` 并把 `version` 改成 **0.6.99**：
  否则 crates.io 上更高的 0.6.5 会被解析器选中，`[patch]` 会被静默忽略
  （cargo 提示 `patch ... was not used`）。
- 特性裁剪（`--no-default-features --features ...`）：

  | 版本 | parallel 模式 | serial 模式 |
  |---|---|---|
  | fork | `parallel,text-events,collect-api,sort-proto` | 空（fork 已修 E0382） |
  | 0.6.5 / 0.6.4 | `parallel,text-events` | **跳过**（基线 serial 编译失败） |
  | 0.6.3 | `parallel`（无 text-events） | **跳过** |

- 构建会改写 `perf/Cargo.lock`（patch 源），脚本在构建后自动还原。
- serial 构建只跑一次/素材（`threads` 列固定 0，表示无并行）。

## 全矩阵结果（2026-11-18 20:19，interleaved，runs=15，起始负载 20%）

**Pi.mid（3,141,592 音符 / 24MB）median ms：**

| 版本 | t1 | t8 | t16 | peak@t8 |
|---|---:|---:|---:|---:|
| **fork** | 34.58 | **7.11** | **6.16** | **43.9MB** |
| 0.6.5 | 39.50 | 10.85 | 10.55 | 48.7MB |
| 0.6.4 | 36.30 | 9.46 | 10.05 | 49.1MB |
| 0.6.3 | 34.75 | 10.02 | 9.69 | 46.8MB |
| fork-serial（t=0） | 31.75 | — | — | 32.6MB |

→ fork 在 t8 **-25% ~ -34%**、t16 **-36% ~ -41%**；峰值也更低（容量预估消除了倍增过冲）。

**t8 其余素材（median ms）：**

| 素材 | fork | 0.6.5 | 0.6.4 | 0.6.3 | 备注 |
|---|---:|---:|---:|---:|---|
| Clementi（4KB） | 0.037 | 0.028 | 0.030 | 0.029 | 微秒级，噪声内 |
| control-dense（1.02M 事件） | 2.41 | 2.13 | ⚠ 0.85 | ⚠ 0.64 | 0.6.3/0.6.4 `notes=0`，不可比 |
| sysex-dense（6.4MB） | 0.151 | 0.050 | 0.045 | 0.040 | 采集语义不同，只作趋势 |
| text-dense（2.4M 事件） | 6.10 | 6.54 | 6.72 | ⚠ 3.33 | 0.6.3 无 text-events（events 减半） |

- **control-dense 上 fork 比 0.6.5 慢 ~13%**：这是 P4 容量预估 `track_bytes / 6` 在
  控制密集轨上的过估（音符 128k、事件 1.02M）——绝对差 0.3ms，属于已知权衡；
  音符主导的素材全部为正收益。
- **sysex-dense 不可比**：各版本对 SysEx 的跳过/保留语义不同（事件数相同不代表做同样的事）。
- **0.6.3/0.6.4 的 control-dense `notes=0`**：其流式 API 未物化音符，0.6–0.8ms 不代表更快。

## 读表纪律（重要）

1. **先对齐 `notes` / `events` 列，再比墙钟**（上表 ⚠ 行都不可比）。
2. **只信交错执行的结果**；非交错的长矩阵会被机器漂移主导。
3. **微秒级素材（Clementi）不可用于版本比较**：全在计时粒度噪声内。
4. **必须看 `.meta.txt` 的起始负载**：本次 20%（Zed/播放器在跑），相对比较仍有效，
   绝对值需要空闲窗口签收（见 `HANDOFF.md` §9）。
5. 每次矩阵都是独立进程（`bench` 口径），峰值内存可横向比较；CPU 时间按 15.625ms tick
   量化，亚毫秒素材的 `cpu_median_ms` 只作参考。

## 已知限制

- 基线 0.6.3–0.6.5 的 **serial 构建编译失败**（`loader.rs` E0382 `track_handler` 被移动），
  fork 已修；矩阵对基线串行模式直接跳过并告警。
- 0.6.3/0.6.4 的流式 API 在 control-dense 上 `notes=0`：属于版本行为差异，
  不是矩阵脚本的问题；对照时以 `notes/events` 列为准。
- 1M 以下音符的轨在 fork 上走标准库排序（PR #303），矩阵用 `minimal` 回调不覆盖下游；
  下游差异见 `perf/results/p5-sort-proto.tsv`。
