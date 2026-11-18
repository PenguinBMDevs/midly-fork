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

## 首轮版本对照（2026-11-18 20:16，t8 / parallel / minimal / runs=15 / 起始负载 13%）

| 素材 | fork | 0.6.5 | 0.6.4 | 0.6.3 |
|---|---:|---:|---:|---:|
| Pi.mid（3.14M 音符） | **7.56ms** | 12.11 | 11.22 | 10.44 |
| Clementi（4KB） | 0.040 | 0.025 | 0.034 | 0.032 |
| control-dense（1.02M 事件） | 2.14 | 2.26 | ⚠ 0.65 | ⚠ 0.66 |
| sysex-dense（6.4MB） | 0.136 | 0.037 | 0.034 | 0.029 |
| text-dense（2.4M 事件） | 7.56 | 8.10 | 7.99 | ⚠ 3.78 |

## 读表纪律（重要）

1. **先对齐 `notes` / `events` 列，再比墙钟。** 本次有两类不可比：
   - 0.6.3 / 0.6.4 的 control-dense 是 `notes=0 events=48`（该版本的流式 API 没物化音符），
     0.65ms 不代表更快，只代表**做的事不一样**；
   - 0.6.3 没有 `text_events`，text-dense 的 events 只有 0.96M（fork/0.6.4/0.6.5 是 1.92M）。
2. **只有 Pi.mid 四项全可比**：fork 相对 0.6.5/0.6.4/0.6.3 分别 **-38% / -33% / -28%**
   （含 P4 容量预分配；0.6.3 没有 text-events，对 Pi 无影响）。
3. **微秒级素材（Clementi）不可用于版本比较**：0.02–0.08ms 全在计时粒度噪声内。
4. **sysex-dense 只作趋势参考**：事件数一致，但各版本对 SysEx 的采集/保留语义不同。
5. **必须看 `.meta.txt` 的起始负载**：>25% 时只能比同批相对值；正式签收用空闲窗口
   （见 `HANDOFF.md` §9）。
6. 每次矩阵都是独立进程（`bench` 口径），峰值内存可横向比较；但 CPU 时间按 15.625ms tick
   量化，亚毫秒素材的 `cpu_median_ms` 只作参考。

## 已知限制

- 基线 0.6.3–0.6.5 的 **serial 构建编译失败**（`loader.rs` E0382 `track_handler` 被移动），
  fork 已修；矩阵对基线串行模式直接跳过并告警。
- 0.6.3/0.6.4 的流式 API 在 control-dense 上 `notes=0`：属于版本行为差异，
  不是矩阵脚本的问题；对照时以 `notes/events` 列为准。
