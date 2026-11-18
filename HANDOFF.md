# lumino-midly 性能迭代交接文档

> 更新：2026-11-18（第二轮会话，P3/P2 补全/EOT 完成）· 工作仓库 `F:\midly-fork`
> 本文档自包含：新会话无历史上下文，按 §0 顺序读完即可接手。
> 配套详细归档：`F:\lumino-rs\.ai-context\sessions\2026-11\`（P0 基线、P3/P4 归档）。

## 0. 新会话开工顺序

1. 加载 `pua` skill（不存在则提示用户"Pua不存在，请检查skills！"并继续）。味道路由：性能优化 → 🟡 字节（A/B + 数据驱动）。
2. 读验收合同：`F:\lumino-rs\docs\research\lumino-midly-performance-guide.md`（指导书 P0–P5 + EOT §2 + 多 Agent 验收门禁）。
3. 读本文档 + `F:\lumino-rs\.ai-context\sessions\2026-11\2026-11-18-midly-p3-eot.md`。
4. 工作区 = `F:\midly-fork`（本 fork）。fork 无 codegraph 索引（lumino-rs 有）；是否索引由用户决定，不要自行 init。
5. 角色纪律（Rust 总工程师）：规划先行；TDD 先测后码；编译/check/test 权归主 agent，subagent 只写代码不跑验证；只测被修改 crate；文件 <400 行；生产代码禁 unwrap/expect；中文注释与 commit；Conventional Commits；每轮归档。
6. **提交消息必须用 `git commit -F <UTF-8 文件>`**：PowerShell 直接传中文参数字节会被 GBK 转码破坏（`-F` 已实证安全；`git log` 显示乱码只是控制台 GBK 噪音，用 hex 校验）。

## 1. 路径地图

| 路径 | 内容 |
|------|------|
| `F:\midly-fork` | 工作仓库。工作分支 `baseline/0.6.5` |
| `F:\midly-fork\perf` | P2/P3 测量台 crate（characterize/bench/gen + Tier2 `lumino-bench`），结果在 `perf/results/*.tsv` |
| `F:\midly-fork\perf\src\bin\lumino_bench.rs` | **Tier2 真实链路测量台**（`--features lumino`，path 依赖 ../lumino-rs/crates/audio/midi-model）。`perf` 因此要求同机存在 lumino-rs 检出 |
| `F:\midly-fork\perf\tier2\` | **Tier2 参考实现**（`document_build.rs.tier2` + README：接入步骤/A-B/后续优化）。lumino 侧排期改为待办，实现不丢 |
| `F:\Soundfont Collection v1\熠星始芒：StarT 164 Million v1.1fix.mid` | 用户提供的**生产级压测素材**：1.25GB / 826 轨 / 164,206,462 音符（仓库外，勿拷贝入库） |
| `F:\midly-fork\tests\oracle*` | P1 正确性 Oracle（入口 oracle.rs + oracle/{reference,compare,synthetic}.rs） |
| `F:\midly-fork\tests\eot.rs` | EOT 规格测试（指导书 §2.9 十项） |
| `F:\midly-fork\test-asset` | 12 个 fixture + 2 个 210k 黑乐谱 |
| `F:\midly-fork\test-asset\generated` | **gitignore**：`midly-perf gen` 合成素材（确定性，重跑即得） |
| `F:\midly-baselines\lumino-midly-{0.6.3,0.6.4,0.6.5}` | 对照版本（.crate 解压，SHA256 与 crates.io 一致） |
| `F:\lumino-rs` | 主仓（dev 分支）。指导书在 docs/research/；**未接入 fork**（见 §3） |

## 2. 已完成（全部有验证证据）

**P0 统一源码与测试资产** ✅
- fork master(093ba6a) 是 0.6.3-dirty；`baseline/0.6.5` 的 src 全部 18 文件 + README 与 crates.io 0.6.5 逐文件 SHA256 一致。
- fixture 从初始 commit 2354d7a 无损恢复；修复 `[lib] name = "midly"`（13 个 doctest）。

**P1 正确性 Oracle** ✅
- 独立参考累加器 + 20 合成用例 + 11 fixture 完整对拍 + PiDamaged 畸形确定性；parallel/serial 双构建。
- Oracle 发现并修复 3 个基线 bug：doctest lib 名；serial E0382；Smf 路径 PB 编码分裂。

**P2 性能基线 + 素材矩阵** ✅
- 测量台：墙钟/CPU/MB/s/Mevents/s/回调占比/峰值内存/每轨分布；预热 2 + 正式 5；单池复用；版本切换 via [patch]+--config。
- 素材：Pi.mid（24MB/30轨/3.14M音符）、StarT210k×2、Clementi、Levels、SysExTest + **合成 4 类**（单轨小/单巨轨1M/控制密集/SysEx密集，`midly-perf gen <dir>`）。
- 决策：合成素材不入库（19MB 二进制），生成器确定性 + 测试自校验（`Smf::parse` + 快路径双口径），`test-asset/generated/` 已 gitignore。

**P3 无锁逐轨并行 API** ✅（主优化）
- 新增 `Fn + Sync` API（旧 `FnMut` 保留，parallel 构建内部委托）：
  - `extract_all_events_per_track_parallel_from_bytes`（流式回调，库内不持锁）；
  - `extract_all_events_per_track_collect_from_bytes`（并行 map + 按轨序 `Vec<T>`，零锁「每轨独立槽位 + 归并」）；
  - `extract_notes_and_control_events_per_track_parallel_from_bytes`。
- **Pi.mid t8：realistic 17.3/18.4 → realistic-par 12.0/13.3ms；串行占比 77% → 0.2%**；merge 仅 0.03ms。
- 修正交接预期：原估 16→~5ms 基于 Amdahl 线性 P/N；实测解析段 t8 只有 3.35×（内存带宽/分配受限），故 P3 真实收益 = min 16.8→11.1ms（-34%）。**天花板已变成解析热循环（minimal t8 ≈ 8.5–10ms），不再在锁。**
- t16 现在优于 t8（11.0 vs 12.0ms）：旧路径 SMT 反升，锁消除后 SMT 有收益。
- 小文件 Clementi 20 轮：0.029 vs 0.030–0.036ms（min 一致）→ 无实质回退。serial 构建 Pi 45.4 vs 47.1ms（±3% 噪声）、Clementi 0.022 vs 0.017 → 平/略优。
- 峰值内存 Pi +1.7%（同量级）；StarT/Clementi 持平或更低。

**EOT TrackTiming** ✅（指导书 §2）
- `TrackTiming { eot_tick, physical_end_tick, content_end_tick, has_events_after_eot }` + `declared_end_tick()` + `is_strict_clean()`；`TrackAllEvents.timing` 暴露。
- 默认**兼容模式**：EOT 后事件照常解析（NoteOff 完成配对、文本/SysEx 全保留），只打标记；Strict 由调用方按 `is_strict_clean()` 决策；缺 EOT → `None` + fallback 物理末尾。
- **热循环零开销**：EOT 处理全在 Meta 分支内，用 `TrackIter::remaining_len()` 判定 EOT 后是否还有字节，`content_end_tick` 循环后推导。同机 A/B：min 7.92 vs 7.96ms（无回归）。
- 测试：`tests/eot.rs` §2.9 十项（315 行）+ Oracle 参考链独立算 timing，11 fixture 逐轨一致。

**Tier2 真实链路接入（lumino-rs 本地未提交）** ✅（数据已验证）
- `perf/src/bin/lumino_bench.rs`（B 方案，零侵入测量台）：只调公开的 `MidiDocument::from_notes_bytes`，
  `digest` 子命令对文档全部公开字段做**规范摘要**（全局事件按 (tick, track, …) 排序后哈希），
  `run` 子命令测端到端墙钟/峰值内存。
- 接入改动点：`crates/audio/midi-model/src/document_build.rs` 改用
  `extract_all_events_per_track_collect_from_bytes`——闭包内完成排序 + ChunkedList 构建 + 文本复制
  （锁外并行），按轨序归并（归并前精确预分配）。
- 端到端（同机同批交替 3 轮）：Pi.mid **19.27→13.50ms**（median -30%，min 18.53→11.37 = **-39%**）；
  StarT210k 5.11→5.48ms（**控制密集 +7%**，结构性：归并串行暴露）；Clementi 持平。
- 语义对拍：5 个素材接入前后摘要逐字段一致；**0.6.5 vs fork 也逐字段一致**（证明 P3/EOT 不改产物）。
- 副作用（正面）：归并按轨序 → 修掉旧实现的**跨轨同 tick 顺序不确定性**（旧行为依赖轨到达顺序）。

**164M 生产级门禁** ✅（用户素材，1.25GB / 826 轨 / 164,206,462 音符）
- 峰值门禁通过：Tier2 前后 3404 → 3461MB（**+1.7%**），墙钟 **4358 → 2261ms（-48%）**；
  语义摘要逐字段一致（`perf/results/tier2-digests/164m-*.txt`）。
- 分阶段归因（t8）：midly 纯解析 **504ms**；midly realistic-par 1355ms；app 2261ms。
- **新发现**：`scan_track_names` 的第二次全量扫描占 **1.28s**（跳过 → 978ms，-57%）。
  根因：无 TrackName 的轨必须走完整条轨才能确认没有名字。
- fork 新增 `TrackAllEvents.track_name`（首个 FF 03 原始字节，meta 分支内带出，零热循环成本）；
  下游接入随 Tier2 一起待办（见 `perf/tier2/README.md`）。
- 原始数据：`perf/results/p4-164m.tsv`。

**commit 表（baseline/0.6.5，master 未动；已签名并推送至 github/baseline/0.6.5）**

| SHA | 内容 |
|-----|------|
| 86ca6ae | chore(baseline): 恢复 0.6.5 发布源码 + 10 fixture |
| 1df438f | fix(packaging): [lib] name=midly 修 doctest |
| 359fc1f | test(oracle): P1 Oracle（982 行） |
| 6bdb8b6 | fix(loader): serial E0382 cfg 门控 |
| 83b3388 | fix(loader): Smf 路径 PB 原始 14-bit 对齐 |
| 1f624a3 | feat(perf): P2 测量台 + 首轮原始数据 |
| 391877a | docs: HANDOFF |
| fc26f4a | feat(loader): 逐轨无锁并行回调 API（P3） |
| f151b29 | feat(perf): realistic-par A/B + P3 首轮数据 |
| cf27127 | feat(perf): 4 类合成素材 + 预分配修正 + 矩阵数据 |
| 0d84ff6 | feat(loader): EOT TrackTiming |
| d897a94 | test(perf): EOT 零开销 A/B 数据 |
| 73a3998 | docs: HANDOFF 更新 |
| 2abb6cb | feat(perf): Tier2 测量台 + 接入前后 A/B 数据 |
| 9e14e95 | docs: HANDOFF 记录 Tier2 |
| 84d9a78 | feat(loader): track_name + 164M 门禁数据 + Tier2 参考实现 |

**测试状态**：fork parallel = 77 单元 + 10 EOT + 6 Oracle + 13 doctest(1 ignored) 绿；serial（`--no-default-features --features std,alloc,memmap,strict`）= 77 + 10 + 6 + 12 绿。新代码 clippy 零新增、rustfmt 干净。

## 3. 当前仓库状态（重要）

- **fork**：`baseline/0.6.5` = 9ade3f7，工作树干净。**19 个提交全部 GPG 签名（G）并已推送**。
  - 远端布局：`origin` = `ssh://git@ssh.github.com:443/...`（本机无 SSH 公钥，不能用）；
    **`github` = `https://github.com/PenguinBMDevs/midly-fork.git`（推送用这个）**。
  - 推送通道：本机 Clash 代理 `127.0.0.1:7890`，仓库级只配 github.com：
    `git config --local "http.https://github.com.proxy" "http://127.0.0.1:7890"`
    （gh-proxy 只读、直连 HTTPS 被限速、SSH 22 被 reset；GCM 已存 `git:https://github.com` 凭据）。
  - 推送命令：`git push github baseline/0.6.5`；备份 bundle：
    `baseline-0.6.5-signed-2026-11-18.bundle`（gitignore）。
  - **旧 SHA 已作废**：补签是对 `master..baseline/0.6.5` 逐提交 `--amend -S` 重写，§2 表里的
    SHA 不再是当前值，以 `git log --oneline` 为准。
- **lumino-rs（dev）：工作区已被用户重置并提交自身工作，与 fork 无关联**。
  - 用户决策：**Tier2 合并等 midly 全部改动完成后再做**（依赖改造视为待办）。
  - **2026-11-18 晚：Tier2 + track_name 已本地临时接入验证并完整还原**
    （181 绿：56+1+124，5 ignored；Pi/164M digest 逐字段一致；
    Pi 16.74→8.25ms -51%、164M 2818→783ms -74%）。工作树仍干净（`cargo check` 通过）。
  - 参考实现与接入步骤、A/B 数据：`perf/tier2/`（`README.md` + `document_build.rs.tier2`）。
  - **不要在 lumino-rs 留任何未提交改动**（用户会重置）；需要实测时按 `perf/tier2/README.md` 临时接入，
    测完 `git checkout --` 还原。
- 依赖 midly 的主仓 crate（6 个）：midi-model、midi-loader、midi-io、export、project、ui。

## 4. 关键数据（原始数据在 perf/results/）

机器 Ryzen 9 7940H 8C/16T；release opt3+lto+codegen1；Pi.mid = 24MB/30轨/3,141,592 音符。

| 配置（Pi.mid，t8） | wall median | 备注 |
|---|---|---|
| P2 首轮 realistic | 16.00ms（15.08–16.26） | 串行占比 79.9% |
| **P3 realistic（同批）** | **17.30 / 18.44**（min 16.84 / 17.34） | 机器漂移，A/B 只认同批 |
| **P3 realistic-par** | **12.04 / 13.30**（min 11.06 / 11.35） | 串行占比 **0.21%**，merge 0.03ms |
| P3 minimal（天花板） | 9.36–10.4（min 7.96–8.6） | 纯解析+派发，内存带宽受限 |
| P3 realistic-par t16 | 10.99（min 10.58） | SMT 现在有收益（旧路径反升） |
| StarT210k realistic → par | 3.11 → 2.80ms | merge 0.60ms = 21%（控制事件 218k 密集） |
| Clementi（4KB/3轨）20 轮 | 0.029 → 0.030–0.036（min 一致） | 无实质回退 |
| gen-control-dense（1.02M 事件） | 4.90/5.43 → 4.56/4.66ms | **merge 2.3–2.4ms ≈ 50%**，新串行热点 |
| gen-sysex-dense（6.3MB SysEx） | 2.03 → 1.22ms | 预分配后 -40%（此前假回退已证伪） |
| gen-single-giant（1 轨 1M 音符） | 13.36 → 13.73ms | **单轨无并行度，P3 无收益 → P5 支点** |
| **真实链路 Pi.mid（Tier2 前→后）** | **19.27 → 13.50ms（min 18.53→11.37）** | median -30% / min -39%；peak 79.9→85.4MB（+7%） |
| 真实链路 StarT210k（控制密集） | 5.11 → 5.48ms | **+7%**：归并串行暴露（结构性），P4 k-way merge 可收回 |
| 真实链路 Clementi | 0.041 → 0.042ms | 持平 |
| **164M（1.25GB/826轨）真实链路 前→后** | **4358 → 2261ms（-48%）** | peak 3404→3461MB（+1.7%）；跳过 `scan_track_names` 再降至 978ms |

**P4 上限测量（A1，2026-11-18；原始数据 `perf/results/p4-a1-upper-bounds.tsv`）**

删除式 A/B（同机同批交替、min 口径、25 次/进程 × 4 进程）：

| 假设 | 素材 | 基线 → 删除后 | 结论 |
|---|---|---|---|
| 文本收集成本 | gen-text-dense（文本类占 60%） | 5.30 → **1.82ms（-66%）** | 约 2.4ns/文本事件 |
| 文本收集成本 | Pi.mid（真实素材） | 8.03 → 7.88ms（-1.9%） | **噪声内，不立项** |
| 双 `advance(delta)` 合并 | Pi / 单巨轨 | 7.89→7.40 / 10.05→9.78ms | **噪声内（median 持平），不立项** |
| **音符容量预估** | Pi.mid | 8.24 → **5.28ms（-36%）** | **立项** |
| **音符容量预估** | gen-single-giant | 10.09 → **8.41ms（-16.6%）** | **立项** |
| **音符容量预估** | 164M | 396.03 → **303.04ms（-23.5%）** | **立项**（安静窗口） |

落地结果（A2，`e2904fe`；**安静窗口 load=3% 复测**）：Pi **7.65→4.97ms（-35.0%）**、
单巨轨 **9.78→8.28ms（-15.3%）**、164M **431.13→282.51ms（-34.5%）**，四次运行区间不重叠。
（负载 22–45% 时同批曾测 512.73→350.10ms = -31.7%，比值一致。）
**峰值不升反降**（`retain-all` 口径）：Pi 116.2→114.9MB、164M 5844.3→5689.1MB（-2.7%）。
4 个近重复 `parse_fast_track_*` 合并**不属于性能项**（各自单态化、同时只跑一个），
如需合并只为减少维护面，不带来墙钟收益。

**A3：P5 上限量化（minimal t8，A2 容量修复后）**

| 素材 | 轨数 / 音符 | wall min | 吞吐 |
|---|---|---|---|
| 164M 最大轨抽成 format 0 | 1 / 8,795,520 | **85.10ms** | 101 Mevents/s |
| gen-single-giant | 1 / 1,000,000 | 9.03ms | 106 Mevents/s |
| Pi.mid | 30 / 3,141,592 | 5.93ms | **445 Mevents/s** |

结论：单轨吞吐只有多轨的 **1/4.4**（并行度缺失）。P5 理论上限 ≈ 4.3×
（8.8M 单轨 85.1ms → ~20ms；8 线程绝对下限 10.6ms，受内存带宽限制）。
但 164M 文件最大轨只占 5.4% → 对它 P5 最多省 ~4%；**是否立项仍取决于
「format 0 / 单巨轨占主导」的真实素材分布**。

**P5 立项门槛扫描（同日）**：`F:\Soundfont Collection v1\` 共 52GB / 8972 文件，
其中 **只有 2 个 MIDI**（164M、StarT210k），**没有真实 format 0 素材**。
→ 无法证伪「P5 无用」，也无法证实「P5 有用」；按数据驱动原则**暂不立项**。




- 计量限制：`GetProcessTimes` 按 15.625ms tick 量化（CPU 仅粗口径）；机器波动 ±2ms（同配置复测）；结论一律用 min/median 区间 + 同批交替 A/B。

**A4：Tier2 之后剩余成本分解（164M，`lumino-bench phases`，2026-11-18 晚）**

| 阶段（164M，接入后 765ms） | 墙钟 | 占比 |
|---|---|---|
| 0.15→0.50 并行逐轨（解析 + 排序 + ChunkedList） | **765.1ms** | **99.9%** |
| 0.50→0.70 按轨序归并 | 1.0ms | 0.1% |
| 0.70→0.72 控制/文本排序 | 1.1ms | 0.1% |
| 其余（头部 / TrackManager / 构造） | ~0.1ms | — |

逐轨内部（各线程累计 CPU，826 轨；`LUMINO_PHASE_STATS=1` 探针）：

- 排序 `sort_by_key(start_tick)` **1915.8ms（62%）**
- `ChunkedList::from_sorted_iter` **1002.2ms（32%）**
- 其余（分配 / 文本复制）177ms
- 解析本身 ≈ **2256ms**（由 midly minimal 282ms × 8 估算）
- 总 CPU ≈ 5351ms → 理想墙钟 669ms；实测 765ms（并行效率 87%）

**尾部（单轨 handler 耗时，前 5）**：766 **450.0ms**、769 363.7ms、767 329.9ms、
775 212.0ms、774 159.4ms。前 5 轨占全部逐轨工作 **48.9%**，**最大单轨 = 端到端墙钟的 59%**。

结论（决定下一杠杆）：

- 「轨内并行解析」（P5）不是最大杠杆——即使解析并行，巨轨的**排序 + ChunkedList**
  仍是单线程临界路径；P5 的天花板被这两项吃掉。
- 排序是**内存受限**的：3 条 800 万音符级轨（各 ~123MB 音符数组）单独构成 450ms 临界路径；
  按全文件平均 18.9ns/音符估算会低估 2.4×（大轨 44.7ns/音符）。
- 控制事件 k-way merge 已无价值：控制/文本排序合计 **1.1ms**。

- 冷/热页缓存：当前 harness 用 `fs::read` 整份入内存，解析测量与页缓存无关（冷热只影响未计时的读盘）；mmap 冷缺页属 Tier2 口径，未测 —— 已记录为限制，不要假装测过。

## 5. 红线与纪律

1. **不可逆操作需用户批准**：push、force-push、改 master、发布 crate。本地分支迭代自主。
2. **GPG 无人值守阻塞**：`commit.gpgsign=true` + key C3F9C959BBD67BA8，无缓存 passphrase 时 pinentry 卡死。用 `git -c commit.gpgsign=false commit -F <file>` 并在 message 注明；推送前补签。
3. **lumino-rs 不提交任何东西**（他人改动 + path patch 不可入库）。
4. **正确性优先于性能**：不得删释放力度/EOT/文本换速度；不得放宽测试。
5. 禁止忙等/spin；墙钟/吞吐/内存/正确性才是验收。
6. fmt：基线 src 已实测 `rustfmt --check` 干净（保持）；新增/修改代码必须 fmt-clean + clippy 零新增。
7. 每 commit 只含一个假设；性能结论附原始数据（TSV 入库）。
8. Rust 2024：**`gen` 是保留字**（模块名用 `materials`）。

## 6. 验证命令速查（Windows PowerShell）

```powershell
cd F:\midly-fork
cargo test --features memmap,strict                                        # 78+10+6+13
cargo test --no-default-features --features std,alloc,memmap,strict        # 78+10+6+12
cargo test --test eot --features memmap,strict -- --nocapture
cargo clippy --all-targets --features memmap,strict
rustfmt --edition 2024 --check src/loader.rs src/fast_midi.rs tests/eot.rs tests/oracle.rs tests/oracle/*.rs

# perf 测量台
cd F:\midly-fork\perf
cargo build --release
cargo test                                                                 # 生成器自校验 3 项
$exe = "F:\midly-fork\perf\target\release\midly-perf.exe"
& $exe gen "F:\midly-fork\test-asset\generated"
& $exe characterize "F:\midly-fork\test-asset\Pi.mid"
& $exe bench "F:\midly-fork\test-asset\Pi.mid" --threads 8 --callback realistic-par --label fork-0.6.5 --out F:\midly-fork\perf\results\x.tsv
# callback: minimal（纯解析）| retain-all（保留全量产物，峰值口径）| realistic | realistic-par
# 素材：gen-text-dense.mid 是「文本风暴」上限素材（文本类占 60%），不要当真实分布引用
# serial 构建：cargo build --release --no-default-features --features text-events
# 版本对照：cargo build --release --config "patch.crates-io.lumino-midly.path='F:/midly-baselines/lumino-midly-0.6.4'"
#   ⚠ 跑完对照务必恢复 fork 构建（二进制会被基线覆盖）

# 抽单轨为 format 0（逐字节保真；P5 素材）
& F:\midly-fork\perf\target\release\midly-perf.exe to-format0 <src.mid> <dst.mid> [--track N]   # 默认选最大轨
# 已生成（gitignore，可重跑）：test-asset/generated/star164-largest-track-format0.mid
#   源 F:\Soundfont Collection v1\…StarT 164 Million v1.1fix.mid 的 track 769
#   1 轨 / 8,795,520 音符 / 67MB / division 1920

# Tier2 真实链路测量台（要求同机存在 ../lumino-rs；编译 midi-model 及其依赖）
cargo build --release --features lumino --bin lumino-bench     # 默认 patch=fork
& F:\midly-fork\perf\target\release\lumino-bench.exe digest F:\midly-fork\test-asset\Pi.mid
& F:\midly-fork\perf\target\release\lumino-bench.exe phases <file.mid>     # 进度回调 → 分阶段墙钟
& F:\midly-fork\perf\target\release\lumino-bench.exe run F:\midly-fork\test-asset\Pi.mid --warmup 2 --runs 5 --label tier2 --out F:\midly-fork\perf\results\tier2.tsv
# 对照基线（0.6.5）：
cargo build --release --features lumino --bin lumino-bench --config "patch.crates-io.lumino-midly.path='F:/midly-baselines/lumino-midly-0.6.5'"

# 主仓集成（临时接入，用完还原）
cd F:\lumino-rs; cargo test -p lumino-midi-model -p lumino-midi-loader
```

## 7. 快路径锁定语义（Oracle + tests/eot.rs 锁死）

- 配对：单 (channel,key) 一个活跃音符；重触发关旧音符 release=0；NoteOn vel=0 等价 NoteOff；NoteOff 透传释放力度。
- 未闭合音符：`finish()` 在**最后事件 tick** 收尾，release=0，按 **channel×key 升序**追加。
- 音符产出顺序 = NoteOff 到达序（非 start_tick 排序）；控制事件 = 文件序。
- **EOT**：首个 `FF 2F`（任意长度）→ `eot_tick`；EOT 不进入事件流；EOT 后事件保留并置 `has_events_after_eot`；`content_end_tick` = 最后非 EOT 事件 tick；重复 EOT 计入 `has_events_after_eot`。
- F0 SysEx 与 F7 Escape 均并入 sys_ex；MidiPort(0x21) 首个生效；**TrackName(0x03) 由快路径带出首个值（`track_name`，原始字节）——下游不得再做第二次全量扫描**；文本 meta 宽松长度（0x58 len≥2 / 0x51 len==3 / 0x59 len≥2；Smf 参考链对 0x58 要求 len≥4）。
- Tempo us=0 跳过；bpm=60e6/us（f32）。
- **PB 契约 = 原始 14-bit（0..16383）**；`PitchBend::as_int()` 是偏差语义，取原值用 `bend.0.as_int()`。
- NoteOn/NoteOff 的 key 是 **u8**（256 键黑乐谱）；Aftertouch 的 key 是 u7。
- 快路径 running status 不在 system 消息后清除（比 Smf 宽松，畸形域差异，未改）。
- 全局 Mutex 回调仅在旧 API 保留（parallel 构建 = Mutex 包 `FnMut` + 委托无锁 API）。
- **跨轨同 tick 事件的顺序**：旧并行回调下 = 轨到达顺序（随调度变化，0.6.5 亦然）；Tier2 的
  collect + 按轨序归并把它变成确定顺序。`lumino-bench digest` 用规范摘要（(tick,track) 排序）
  屏蔽该噪声，做语义对拍。

## 8. 下一步（优先级序）

0. **推送已完成** ✅：19 提交已签名并推到 `github/baseline/0.6.5`（详见 §3）。后续提交：
   `git push github baseline/0.6.5`（仓库级代理配置已就绪）。
1. **P4 解析热循环** ✅ **已完成唯一有效项**（其余为噪声内，见 §4）：
   - `e2904fe`：音符容量按 `track_bytes / 6` 预估（不变式：每 NoteOn/NoteOff
     ≥3 字节），单轨上限 16M 音符。Pi -30%、164M -23.5%、峰值 -2.7%。
   - 文本收集（2.4ns/事件）只在文本风暴素材上可测；`advance` 合并无效；
     `parse_fast_track_*` 合并是维护项不是性能项。**没有别的 P4 假设待做**——
     新的热循环假设必须先有删除式上限数据再立项。
   - 新增测量能力：`--callback retain-all`（峰值口径）、`gen-text-dense.mid`。
2. **P5 单巨轨轨内并行**：gen-single-giant 13.36ms 且 P3 无收益。
   **素材已就位**：从 164M 真实文件抽轨 769 → 单轨 format 0（8,795,520 音符，67MB，
   逐字节保真；`midly-perf to-format0` 可对任意源再生成）。注意：826 轨里最大轨占 5.4%，
   对**该文件** P5 不是关键路径。
   **立项门槛已扫描（2026-11-18）：`F:\Soundfont Collection v1\` 全目录 52GB 只有 2 个 MIDI**
   （164M 与 StarT210k），**没有任何真实 format 0 素材** → 无法给出「单轨占主导」的分布证据。
   A3 上限：单轨 101 Mevents/s vs 多轨 445 Mevents/s（4.3×），但 8.8M 单轨绝对时间仅 85ms。
   **结论：P5 暂不立项**，除非后续出现真实的 format 0/单巨轨素材或用户侧统计。
3. **Tier2 落地 ✅ 已开 PR**：`lumino-rs` 分支 `feat/midi-tier2`（`f8b47981`，已推送）→ PR #302（base `dev`）。
   本地验证：181 绿 + digest 一致 + Pi -51% / 164M -74%；依赖为 fork `rev = e8edb9a`。
   **后续优化（A4 数据重排）**：巨轨排序 + ChunkedList（450ms 临界路径）> P5 轨内解析；
   控制事件 k-way merge 已无价值（1.1ms）。
4. **P2 矩阵补全（剩余项）**：矩阵驱动脚本（版本 × parallel/serial × 线程 × 素材）；
   mmap 冷/热页缓存分离；电源模式固定并记录。
5. EOT 后续（可选）：Strict 解析模式 API；下游用 `timing` 做悬挂音符/Hold 收尾与 Trim 策略。

## 9. 技术陷阱清单

- `#[repr(C, packed)]` 字段禁入 `assert_eq!`（E0793）：先 `let x = *packed;` 再按值取字段。
- 集成测试 crate root 的子模块**不自动**解析 `tests/oracle/`：用 `#[path = "oracle/xxx.rs"] mod xxx;`。
- Rust 2024：`gen` 保留字；`unsafe extern "system" {}`；unsafe fn 内裸 unsafe 触发 unsafe_op_in_unsafe_fn。
- 包名 `lumino-midly` + `[lib] name=midly` → 依赖必须 `midly = { package = "lumino-midly" }` 别名。
- 0.6.3 `TrackAllEvents` 无 `text_events`；perf 用 `text-events` 特性门控。
- 0.6.3/0.6.4 无 serial E0382 修复 → 基线只跑 parallel。
- GitHub 直连 reset → 用 `https://gh-proxy.com/https://github.com/...`；crates.io 直连可用。
- **PowerShell 是 GBK 控制台**：git/cargo 的 UTF-8 输出显示为乱码属噪音；提交消息用 `-F <UTF-8 文件>`，需要核对时用 `cmd /c "git log ... > file"` + hex。
- 性能测量：同批交替 A/B、min 口径、≥15 次正式运行；`cargo --config` 覆盖 patch 后务必恢复 fork 构建。
- 性能 A/B 前先质疑测量实现：SysEx 密集「回退」实为 arena 反复倍增重分配，预分配后 -40%。
- **`Copy-Item` 保留源 mtime → cargo 判定未变更而不重编**：还原被改源文件后必须
  `(Get-Item f).LastWriteTime = Get-Date` 再 build，否则拿到旧二进制（本会话实测踩中）。
- `perf` 现在 path 依赖 `../../lumino-rs/crates/audio/midi-model`：**同机必须有 lumino-rs 检出**，
  否则 `cargo build`/`cargo test` 在清单解析阶段即失败（与 `lumino` feature 无关）。
- `lumino-bench digest` 的规范摘要按 (tick, track, …) 排序：不要用未排序摘要做门禁（跨轨同 tick
  顺序随调度变化，会假报不一致）。
- **GPG 签名 ≠ 推送认证**：GPG 只签提交内容；推送需要 HTTPS PAT（GCM 已存 `git:https://github.com`）
  或 SSH 公钥。本机 `origin` 是 `ssh://git@ssh.github.com:443` 但无公钥；**推送走 `github` 远端**
  （HTTPS + 仓库级代理 `http.https://github.com.proxy`）。gh-proxy 只读，不能推。
- **测量前必须确认机器空闲**：本会话在 CPU 负载 100%（Zed/播放器/Edge）时测得 Pi minimal
  22–25ms（正常 8.5–10ms），墙钟直接翻倍；用 `Get-CimInstance Win32_Processor` 看 LoadPercentage。
- **lumino-rs 工作区可能被用户重置/提交**：任何未提交改动都会消失。Tier2 类改动只在临时窗口做，
  测完立刻 `git checkout --`，实现留档在 `perf/tier2/`。

## 10. 环境事实

- Windows，Ryzen 9 7940H（8 物理核/16 逻辑），rustc/cargo 1.98.1。
- 电源模式/后台进程未控制（指导书要求固定——矩阵补全时记录并尽量固定）。
- fork 克隆含全部历史；test-asset 两个黑乐谱文件名含中文+全角冒号，用 read_dir 遍历不要硬编码。

## 11. 自主运行合同（用户已确认）

- 连续自主循环：假设→固定基线→先补正确性测试→单点实现→parallel/serial 对拍→同机 A/B→接受或回退→记录数据。
- 无人值守边界：会话关闭即停（靠本文档+归档+分支接续）；GPG 交互是唯一硬阻塞；不可逆操作不做。
- 产品决策合理假设先行并记录：EOT 默认兼容模式 + 诊断（不静默丢数据）。
- 失败升级按 pua 契约计数（当前 L0，已确认失败 0 次：P3 假回退/EOT 假回归均通过 A/B 归因并修复，属正常迭代）。
