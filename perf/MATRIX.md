# P2 性能矩阵

`perf/matrix.ps1`：版本 × parallel/serial × 线程 × 素材，一键复现 + 记录环境。

```powershell
# 全矩阵（4 版本 × 2 模式 × 3 线程 × 5 素材）
.\perf\matrix.ps1 -Versions fork,0.6.5,0.6.4,0.6.3 -Modes parallel,serial `
    -Threads 1,8,16 -Materials pi,clementi,control,sysex,text -Runs 15 -Warmup 3

# 复用已缓存二进制
.\perf\matrix.ps1 -NoBuild -Materials pi,text
```

- 二进制缓存：`perf/target/matrix/<version>-<mode>.exe`
- 输出：`perf/results/matrix-<ts>.tsv`（列见 `bench::tsv_header`）+ `.meta.txt`
  （CPU / 逻辑核 / 起始负载 / commit / rustc / 构建参数）
- 基线：`F:\midly-baselines\lumino-midly-<ver>/`；脚本自动按版本裁剪特性：
  - `0.6.3`：无 `text_events`、无 `_collect_` → `parallel`（serial 模式则空）
  - `0.6.4/0.6.5`：无 `_collect_` → `parallel,text-events`
  - `fork`：`parallel,text-events,collect-api`
- 构建会改写 `perf/Cargo.lock`（patch 源），脚本在构建后自动还原。

## 首次运行结果（2026-11-18 20:11，t8 / parallel / minimal / runs=15 / 起始负载 11%）

| 素材 | fork | 0.6.5 | 0.6.4 | 0.6.3 |
|---|---:|---:|---:|---:|
| Pi.mid（3.14M 音符） | **7.66ms** | 13.79 | 11.43 | 11.47 |
| Clementi（4KB） | 0.040 | 0.083 | 0.028 | 0.028 |
| control-dense（1.02M 事件） | **2.49** | 3.76 | ⚠ 0.65 | 2.40 |
| sysex-dense（6.4MB） | 0.181 | 0.115 | 0.051 | 0.030 |
| text-dense（2.4M 事件） | **7.82** | 9.43 | 8.23 | 8.79 |

## 读表纪律（重要）

1. **先对齐 `notes` / `events` 列，再比墙钟。**
   首次运行里 0.6.4 的 control-dense 是 `notes=0 events=48`（该版本的 streaming API
   语义与 fork 不同），0.65ms 不代表更快，只代表**做的事不一样**。
   sysex-dense 也属于「事件数一致、语义未必一致」，只能作趋势参考。
2. **微秒级素材（Clementi）不可用于版本比较**：0.02–0.08ms 全在计时粒度噪声内。
3. **必须看 `.meta.txt` 的起始负载**：>25% 时只能比同批相对值；
   正式签收用空闲窗口（见 `HANDOFF.md` §9）。
4. 每次矩阵都是独立进程（`bench` 口径），峰值内存可直接横向比较；但 **CPU 时间按
   15.625ms tick 量化**，亚毫秒素材的 `cpu_median_ms` 只作参考。
