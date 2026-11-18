# lumino-midly 性能迭代交接文档

> 生成：2026-11-18 · 上一会话工作区为 F:\lumino-rs，实际代码工作在本 fork。
> 本文档自包含：新会话无历史上下文，按 §0 顺序读完即可接手。
> 配套详细归档：`F:\lumino-rs\.ai-context\sessions\2026-11\2026-11-18-midly-p0-baseline.md`（含 PUA-CHECKPOINT）。

## 0. 新会话开工顺序

1. 加载 `pua` skill（不存在则提示用户"Pua不存在，请检查skills！"并继续）。味道路由：性能优化 → 🟡 字节（A/B + 数据驱动）。
2. 读验收合同：`F:\lumino-rs\docs\research\lumino-midly-performance-guide.md`（指导书，P0–P5 路线 + EOT §2 + 多 Agent 验收门禁）。
3. 读本文档 + 配套归档（路径见上）。
4. 建议工作区 = `F:\midly-fork`（本 fork）。注意：fork 无 codegraph 索引（lumino-rs 有）；是否索引由用户决定，不要自行 init。
5. 角色纪律（Rust 总工程师）：规划先行（读 AGENT.md/decisions/patterns/summaries 若存在）；TDD 先测后码；编译/check/test 权归主 agent，subagent 只写代码不跑验证；只测被修改 crate，不跑 workspace 全量；文件 <400 行；生产代码禁 unwrap/expect；中文注释与 commit；Conventional Commits；每轮归档到 `.ai-context/sessions/YYYY-MM/`。

## 1. 路径地图

| 路径 | 内容 |
|------|------|
| `F:\midly-fork` | 工作仓库（克隆自 github.com/PenguinBMDevs/midly-fork，gh-proxy 前缀）。工作分支 `baseline/0.6.5` |
| `F:\midly-fork\perf` | P2 测量台 crate（characterize/bench），结果在 `perf/results/*.tsv` |
| `F:\midly-fork\tests\oracle*` | P1 正确性 Oracle（入口 oracle.rs + oracle/{reference,compare,synthetic}.rs） |
| `F:\midly-fork\test-asset` | 12 个素材：10 个恢复的 fixture + 2 个 210k 音符黑乐谱（"熠星始芒：StarT audio 210k v1/v1.1fix.mid"） |
| `F:\midly-baselines\lumino-midly-{0.6.3,0.6.4,0.6.5}` | 对照版本（.crate 解压，三个 SHA256 均与 crates.io API checksum 一致） |
| `F:\lumino-rs` | 主仓（dev 分支）。指导书在 docs/research/；**当前未接入 fork（见 §3）** |
| `C:\Users\zhang\.cargo\registry\src\index.crates.io-1949cf8c6b5b557f\lumino-midly-0.6.5` | 0.6.5 发布源码（source of truth，checksum cd01adfe…） |

## 2. 已完成（全部有验证证据）

**P0 统一源码与测试资产** ✅
- fork master(093ba6a) 是 0.6.3-dirty，0.6.4/0.6.5 发布 commit 从未推送（0.6.5 发布自本地 7942019e）。
- `baseline/0.6.5` 分支：src 全部 18 文件 + README 与 crates.io 0.6.5 **逐文件 SHA256 一致**；实际内容差异仅 loader.rs/fast_midi.rs/lib.rs。
- fixture 根因：commit a2bff4b 误删全部 10 个 fixture；从初始 commit 2354d7a 无损恢复。发布 crate `include=["/src/*","/Cargo.toml"]` 不含测试资产。
- 修复改名遗留 bug：`[lib] name = "midly"`（否则 13 个 doctest 全挂，`use midly::` 无法解析）。

**P1 正确性 Oracle** ✅
- 独立参考累加器（**不复用 NoteAccumulator**，防共享 bug），消费 Smf::parse 逐轨产出期望值，与快路径 TrackAllEvents 全字段顺序敏感对拍。
- 20 合成用例（running status/非零释放力度/同键重触发/同 tick 文件序/0xD0 错位回归/EOT 边界/SysEx+Escape/文本 meta 全集/多轨不同 EOT/VLQ 大 delta/format 0/1/2）+ 11 fixture 完整对拍 + PiDamaged 畸形确定性。
- parallel/serial 双构建对拍一致。
- **Oracle 发现并修复 3 个基线 bug**：① doctest lib 名；② serial 构建 E0382 完全不可编译（两处流式函数无条件 Mutex::new(track_handler)，加 #[cfg(feature="parallel")] 门控）；③ Smf 路径 PB 编码分裂（bend.as_int() 偏差误用 as u16 → 负偏差 wrap+掩码，raw 100→8292；改 bend.0.as_int()；回归门 test_oracle_smf_api_pb_parity）。

**P2 首轮基线** ✅（矩阵未补全，见 §8）
- perf 测量台：墙钟/CPU/MB/s/Mevents/s/回调串行占比/峰值内存/每轨分布；预热 2+正式 5，median/min/max；单池复用；版本切换 via [patch]+--config。
- 素材表征 12 个（perf/results/characterize.tsv）。

**commit 表（baseline/0.6.5，master 未动）**

| SHA | 内容 |
|-----|------|
| 86ca6ae | chore(baseline): 恢复 0.6.5 发布源码 + 10 fixture |
| 1df438f | fix(packaging): [lib] name=midly 修 doctest |
| 359fc1f | test(oracle): P1 Oracle（982 行） |
| 6bdb8b6 | fix(loader): serial E0382 cfg 门控 |
| 83b3388 | fix(loader): Smf 路径 PB 原始 14-bit 对齐 |
| 1f624a3 | feat(perf): P2 测量台 + 首轮原始数据 |

**测试状态（交接时点）**：fork 全量 parallel = 77 单元 + 5 Oracle + 13 doctest(1 ignored) 绿；serial（--no-default-features --features std,alloc,memmap,strict）= 77 + 5 + 12 绿。新代码 clippy 零警告、rustfmt 干净。主仓集成验证（181 测试绿）是在 patch 接入期间跑的，**patch 现已被还原**（§3）。

## 3. 当前仓库状态（交接时核实，重要）

- **fork**：`baseline/0.6.5` = 1f624a3，工作树干净。6 个提交**未签名、未推送**。
- **lumino-rs（dev）**：上一会话加的 `[patch.crates-io] lumino-midly = { path = "../midly-fork" }` **已被还原**（用户重置工作区）。Cargo.toml/Cargo.lock 干净，lumino-midly 回到 **registry 0.6.5（发布版，不含本 fork 修复）**。另有 ~62 个他人未提交改动（dialog/message/ui-egui 等）——**不要动、不要提交 lumino-rs 任何东西**。
- 需要集成验证时重新接入（用完还原，禁止提交）：
  1. Cargo.toml [patch.crates-io] 加 `lumino-midly = { path = "../midly-fork" }`
  2. `cargo test -p lumino-midi-model -p lumino-midi-loader`（181 测试）
  3. 还原 Cargo.toml/Cargo.lock（git checkout -- 这两个文件即可，它们当前无其他改动）
- 依赖 midly 的主仓 crate（6 个）：midi-model、midi-loader、midi-io、export、project、ui。主链路：`MidiDocument::from_notes_bytes → extract_all_events_per_track_streaming_from_bytes → rayon 按轨解析 → 全局 Mutex 回调（排序/ChunkedList/文本复制/全局归并）`。

## 4. 关键数据（首轮基线，原始数据 perf/results/p2-baseline.tsv）

机器 Ryzen 9 7940H 8C/16T；release opt3+lto+codegen1；Pi.mid = 24MB/30轨/3,141,592 音符。

| 配置（Pi.mid） | wall median | 备注 |
|---|---|---|
| fork parallel t1 realistic | 45.37ms | 回调串行占比 21.9% |
| fork parallel t2 realistic | 26.02ms | 41.1% |
| fork parallel t4 realistic | 19.08ms | 66.5% |
| fork parallel t8 realistic | **16.00ms**（15.08–16.26） | **79.9%**，1504 MB/s，196 Mevents/s，peak 119.8MB |
| fork parallel t16 realistic | 17.89ms | **SMT 竞争反升** |
| fork parallel t8 minimal | 10.73ms | ≈纯解析+派发 |
| fork parallel t1 minimal | 35.85ms | |
| fork serial realistic / minimal | 46.06 / 35.04ms | ≈ parallel t1 |
| base-0.6.4 parallel t8 realistic / minimal | 17.57 / 10.24ms | |
| base-0.6.3 parallel t8 realistic / minimal | 15.86 / 8.57ms | 0.6.3 无 text_events（静默丢文本 meta） |

- **Amdahl 模型 wall ≈ S + P/N 拟合 N=1..8 误差 <1ms**（S≈12.8ms 回调串行，P≈25.8ms 可并行解析）。t1→t8 加速 2.84×，上限≈3.5×。**P3（全局 Mutex 回调）是唯一主抓手，数据强确认。**
- 每轨回调分布（t8 realistic）：p50=0.288ms p90=0.982 max=1.458ms（最慢轨#18，208k 音符）——Pi.mid 多轨均衡。
- StarT210k v1.1fix（2.5MB/40轨/21万音符）：parallel t8 realistic 2.72ms（60.9%）/ minimal 1.90ms；serial realistic 5.99ms。
- 小文件 Clementi（4KB/3轨/666音符）：parallel t8 = 70µs vs serial = 20µs → **派发开销 3.5×，serial 阈值必要（P2.5-C）**。
- **0.6.3 纯解析快 20-25% 是靠丢文本 meta 换的——指导书红线，不可接受；P4 假设：不丢数据收回该成本。**
- 计量限制：GetProcessTimes 按 15.625ms 系统 tick 量化（短跑 CPU 读 0）→ 墙钟 QPC 为准；机器波动存在（同配置复测 16.00→18.50ms，min 一致）→ 结论用 min/median 区间；冷/热页缓存未分离（待补）。

## 5. 红线与纪律

1. **不可逆操作需用户批准**：push 到 org 仓库、force-push、改 master 历史、发布 crate。本地分支迭代自主。
2. **GPG 无人值守阻塞**：全局 commit.gpgsign=true + key C3F9C959BBD67BA8，gpg-agent 无缓存 passphrase 时 pinentry 弹窗卡死（上一会话实证超时）。本地提交用 `git -c commit.gpgsign=false commit` 并在 message 注明；**推送前补签**（用户输入一次 passphrase 或授权方案）。卡死时杀 pinentry/gpg-agent 进程 + 检查 .git/index.lock。
3. **lumino-rs 不提交任何东西**（他人未提交改动 + patch 不可入库：CI/他机无 ../midly-fork 路径）。
4. **正确性优先于性能**：不得删释放力度/EOT/文本等正确性信息换速度；不得为过关放宽测试。
5. 禁止忙等/spin；CPU 利用率是诊断指标，墙钟/吞吐/内存/正确性才是验收。
6. fmt 政策：基线 src 保持上游字节（豁免 cargo fmt，避免全量重排污染 diff）；**新增/修改代码必须 rustfmt --edition 2024 干净 + clippy 零新增警告**（上游遗留警告 ~79 个已记录，不属本轮）。
7. 每 PR/commit 只含一个性能假设；性能结论附原始数据（TSV 入库）。
8. lumino-rs 规则：禁改 traits/、禁 mod.rs、生产禁 unwrap/expect（测试 expect 带上下文）。

## 6. 验证命令速查（Windows PowerShell）

```powershell
# fork 全量（parallel，默认 features 含 alloc/std/parallel）
cd F:\midly-fork; cargo test --features memmap,strict
# fork 全量（serial）
cargo test --no-default-features --features std,alloc,memmap,strict
# 仅 Oracle
cargo test --test oracle --features memmap,strict -- --nocapture
# clippy / 新文件 fmt
cargo clippy --all-targets --features memmap,strict
rustfmt --edition 2024 --check tests/oracle.rs tests/oracle/reference.rs tests/oracle/compare.rs tests/oracle/synthetic.rs

# perf 测量台（在 F:\midly-fork\perf）
cargo build --release --manifest-path F:\midly-fork\perf\Cargo.toml
$exe = "F:\midly-fork\perf\target\release\midly-perf.exe"
& $exe characterize "F:\midly-fork\test-asset\Pi.mid"
& $exe bench "F:\midly-fork\test-asset\Pi.mid" --threads 8 --callback realistic --label fork-0.6.5 --out F:\midly-fork\perf\results\p2-baseline.tsv
# serial 构建（注意 --features text-events 保留 fork 字段）
cargo build --release --no-default-features --features text-events --manifest-path F:\midly-fork\perf\Cargo.toml
# 版本对照（0.6.4；0.6.3 需 --no-default-features --features parallel 关 text-events）
cargo build --release --config "patch.crates-io.lumino-midly.path='F:/midly-baselines/lumino-midly-0.6.4'" --manifest-path F:\midly-fork\perf\Cargo.toml
# 跑完对照务必恢复 fork 构建：cargo build --release --manifest-path F:\midly-fork\perf\Cargo.toml

# 主仓集成（临时接入，用完还原，见 §3）
cd F:\lumino-rs; cargo test -p lumino-midi-model -p lumino-midi-loader
```

## 7. Oracle 锁定的基线语义（P3/EOT 开发必读）

- 配对：单 (channel,key) 一个活跃音符；重触发关旧音符 release=0；NoteOn vel=0 等价 NoteOff(release 0)；NoteOff 透传释放力度。
- 未闭合音符：finish() 在**最后事件 tick**（含 EOT delta）收尾，release=0，按 **channel×key 升序**追加。
- 音符产出顺序 = NoteOff 到达序（**非 start_tick 排序**）；下游 midi-model 自行稳定排序。控制事件 = 文件序。
- **EOT(0x2F) 完全被忽略**（`_ => {}`），TrackAllEvents 无 TrackTiming；EOT 后事件继续宽松解析（无诊断无标记）——指导书 §2 的 TrackTiming/Strict/兼容模式尚未实现。
- F0 SysEx 与 F7 Escape 均并入 sys_ex；MidiPort(0x21) 首个生效；TrackName(0x03) 不收录（下游 scan_track_names 负责）；文本 meta 宽松长度（0x58 len≥2 / 0x51 len==3 / 0x59 len≥2；Smf 参考链对 0x58 要求 len≥4 否则 Unknown——Levels.mid 有 2 字节拍号，Oracle 已建模该差异）。
- Tempo us=0 跳过；bpm=60e6/us（f32）。
- **PB 契约 = 原始 14-bit（0..16383）**；`PitchBend::as_int()` 是 [-8192,8191] 偏差语义，取原值用 `bend.0.as_int()`。
- NoteOn/NoteOff 的 key 是 **u8**（256 键黑乐谱）；Aftertouch 的 key 是 u7。
- 快路径 running status 不在 system 消息后清除（比 Smf 宽松，畸形域差异，未改，Oracle 合成用例避开）。
- 全局 Mutex 回调在**两处**：extract_all_events_per_track_streaming_from_bytes 与 extract_notes_and_control_events_per_track_streaming_from_bytes（P3 都要覆盖或明确弃用策略）。
- P4 候选：4 个近重复 parse_fast_track_* 函数；每事件双 advance(delta)（note_acc+ctrl_acc）；文本收集成本（vs 0.6.3 差 20-25%）。
- 主仓无 Smf 版提取 API 调用方（grep 证实）；应用主链路 = 快路径字节 API。

## 8. 下一步（优先级序）

1. **P3 主优化（数据已实锤，预期 wall 16ms→~5ms）**：新增 `Fn + Sync` 逐轨并行回调 API（旧 API 保留兼容）；排序/ChunkedList 构建/文本复制在锁外按轨执行；每轨写独立槽位，最后按轨序轻量归并；全局稳定排序只按 tick 时保持同 tick 文件序。验收：Oracle 全绿（回调恰好一次/索引完整断言已有）+ parallel/serial 对拍 + perf realistic A/B 附 TSV + 小文件无回退 + 峰值内存无不可接受回退。可派 rust-code-engineer subagent 写代码（注入 pua，**不给编译/test 权**）。
2. **P2 补全**：perf 加 gen 模式合成 4 类缺失素材（单轨小文件 / 单巨轨≥1M 音符 / 控制密集 / SysEx 密集大文件），固定素材入库；矩阵驱动脚本（版本×parallel/serial×线程 1/2/4/8/16×素材，逐格新进程）；冷/热页缓存分离记录。
3. **EOT TrackTiming（指导书 §2）**：先写 §2.9 十项规格测试（红）→ 实现 TrackTiming{eot_tick,physical_end_tick,content_end_tick,has_events_after_eot}（绿）；默认兼容模式+诊断（产品决策假设，用户可推翻）；Strict 模式可选；不得静默丢 EOT 后事件；未闭合音符以 EOT 收尾。
4. **P4 热循环**（每 commit 单假设）：文本收集成本收回、双 advance 统一、4 重复函数合并、active-note 稀疏索引（按 profile）。
5. **P5 单巨轨轨内并行**：轻量扫描安全边界 (offset,absolute_tick,running_status) → 并行 chunk → O(边界状态) 缝合；禁止任意字节切轨。
6. **Tier2**：lumino 主链路分阶段插桩（parse/callback/sort/ChunkedList/final merge），验证全链路收益与峰值内存。

## 9. 技术陷阱清单（上一会话踩过）

- `#[repr(C, packed)]` 字段禁入 assert_eq!（E0793 取引用）：先 `let x = *packed;` 再按值取字段到普通局部。
- 集成测试 crate root（tests/oracle.rs）的子模块**不自动**解析 tests/oracle/ 子目录：用 `#[path = "oracle/xxx.rs"] mod xxx;`。
- edition 2024：`unsafe extern "system" {}`；unsafe fn 内裸 unsafe 操作触发 unsafe_op_in_unsafe_fn 警告（上游遗留）。
- crate 名：包名 lumino-midly，fork [lib] name=midly → 依赖必须写 `midly = { package = "lumino-midly" }` 别名才能跨版本（基线默认 lib 名 lumino_midly）一致 `use midly::`。
- 0.6.3 TrackAllEvents **无 text_events 字段**（0.6.4 加入）：perf 用 text-events 特性门控；0.6.3 跑分关该特性，事件数口径差异已记录。
- 0.6.3/0.6.4 基线**无 serial E0382 修复** → 基线只跑 parallel；serial 对照仅 fork。
- GitHub 直连 connection reset → 克隆用 `https://gh-proxy.com/https://github.com/...`（fallback ghproxy.net）；crates.io/static.crates.io 直连可用。
- PowerShell 把 git/cargo 的 stderr 进度当 NativeCommandError 噪音——看实际输出与退出码，别被红字骗。
- cargo --config 覆盖 patch path 后**务必恢复 fork 构建**再跑 fork 测量（二进制会被基线覆盖）。

## 10. 环境事实

- Windows，Ryzen 9 7940H（8 物理核/16 逻辑），rustc/cargo 1.98.1。
- 电源模式/后台进程未控制（指导书要求固定——矩阵补全时记录并尽量固定）；性能结论以 min/median 区间表述。
- fork 克隆含全部历史（master 单分支，无 tag）；test-asset 两个黑乐谱文件名含中文+全角冒号，用 read_dir 遍历不要硬编码。

## 11. 自主运行合同（用户已确认）

- 本会话内连续自主循环：假设→固定基线→先补正确性测试→单点实现→parallel/serial 对拍→同机 A/B→接受或回退→记录数据。
- 无人值守边界：会话关闭即停（靠本文档+归档+分支接续）；GPG 交互是唯一硬阻塞（§5.2）；不可逆操作不做。
- 产品决策合理假设先行并记录：EOT 默认兼容模式+诊断（不静默丢数据）。
- 失败升级按 pua 契约计数（当前 L0，已确认失败 0 次——Oracle 首跑发现的基线差异属预期产出）。