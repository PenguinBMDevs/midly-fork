# midly fork 的发布路径（决策稿）

> 现状：`lumino-rs` 通过
> `[patch.crates-io] lumino-midly = { git = "https://github.com/PenguinBMDevs/midly-fork", rev = "e8edb9a" }`
> 使用本 fork。**git rev 是临时方案**：CI 依赖外网可达、`cargo update` 不会漂移（已用 rev 固定），
> 但版本语义（semver / 变更日志 / 发布产物）缺失。

## 三条路

| | A. 保持 git rev | B. 发布 crates.io | C. 上游合并 |
|---|---|---|---|
| 成本 | 0 | 中（版本策略 + 发布流程） | 高（PR 评审 + 上游节奏） |
| 可复现性 | 好（rev 固定） | 最好（版本号 + checksum） | 最好 |
| 外网依赖 | 构建时必须能拉 GitHub | 仅 crates.io 镜像 | 仅 crates.io |
| 版本语义 | 无 | 有 | 有 |
| 风险 | 仓库被删/改名即失联；无审计锚点 | 发布不可撤回（可 yank） | 周期不可控 |

## A 的最低维护要求（现在就适用）

1. **rev 必须指向已推送、已签名的提交**（当前 `e8edb9a` 在 `baseline/0.6.5`，26 提交全签名）。
2. fork 的 `HANDOFF.md` + `perf/results/` 是唯一变更说明；改动必须配数据。
3. 升级 rev 时：`cargo update -p lumino-midly` 后跑
   `cargo test -p lumino-midi-model -p lumino-midi-loader` + `lumino-bench digest`（Pi + 164M）。

## B 的落地步骤（建议中期）

1. 版本号：fork 当前 `0.6.5`；API 有增量（`TrackAllEvents.track_name`、`_collect_` 系列、
   `TrackTiming`）→ 发 **0.6.6**（新增字段/函数，向后兼容；`TrackAllEvents` 增加字段对
   结构体字面量构造者是破坏性的，但该结构体只由解析器产出，实际无外部构造）。
2. 清单核对：`Cargo.toml` 的 `description/repository/license`、`README`、`CHANGELOG`。
3. 流程：`cargo package` 干跑 → `cargo publish --dry-run` → 打 tag → `cargo publish`。
   **发布不可逆**（只能 yank），需要负责人确认。
4. 发布后把 lumino 的 patch 改成 `lumino-midly = "0.6.6"`（删除 git patch），CI 不再需要外网。

## C 的判断标准

只有在「fork 的改动对上游也有价值」且「上游愿意接收」时才走 C：
- 本 fork 的改动集中在**并行加载 / 文本事件 / EOT 诊断**，属于通用能力；
- 但上游 midly 的维护节奏与 API 稳定性策略未知，且本 fork 已绑定 lumino 的语义
  （如「归并按轨序」），上游未必接受。
- 结论：**C 不作为短期路径**；以 B 为正式发布，A 为过渡。

## 建议顺序

```
现在：A（rev = e8edb9a，已合并 #302）
#303 合并后：A（rev 升级到含基数排序的提交）
下次发版窗口：B（0.6.6），同时删除 git patch
长期：B 为主；C 仅在出现上游合作意愿时评估
```
