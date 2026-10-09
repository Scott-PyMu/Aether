# M4 Gate 4 验收报告（MVP Go/No-Go）

| 项 | 内容 |
|---|---|
| 门禁 | Gate 4（《实施计划与验收标准》v1.24 §5「出口门禁 Gate 4」） |
| 验收对象 | M4 硬化与验收：M4-01…M4-06（含 30 场景演练、T1–T15、命令面完整性、打包发布、文档与安全收尾） |
| 基线 | 需求 v0.12、设计 v1.15；另含 ADR-016（未并入设计，缺陷修复登记）、实施计划 v1.24、`docs/P1-实施计划与验收标准.md` v0.5 |
| 被测提交 | 代码提交 `37c94a7`（含 M4 批次 + ADR-016 修复 + Rust 1.99 兼容修复；ci #80 22/22 全绿）；M4 归档提交 `b157849`（m4-nightly #2 全绿） |
| 执行环境 | 本机（Windows 10：Rust / Node v24.14.1 / Bun 1.4.2 / pnpm 10.34.5；真实 WebView2；Rust 1.99.0 复核）+ CI（ci #80 22/22；m4-nightly #2 三作业全绿） |
| 执行日期 | 2026-10-08 首验 + 2026-10-09 复验与 CI 归档 |
| **结论** | **Go（正式）**：全部验收项闭环——30/30 失败场景演练在案（m4-nightly #2 drills）、T1–T15 全绿（覆盖率 Rust 88.23% / TS 83–95%）、命令面 36/36 与错误码 20/20 核验、发布形态 MSI（三官方注册、无 Mock、哈希一致、首启自检）与测试构建安装产物会话闭环+内联权限回环通过（m4-nightly #2 acceptance + installer smoke）、文档/安全收尾完成；ADR-016 经四轮评审结案（v0.5）；**安装产物冒烟 CI 夜跑归档完成**；无回退动作触发。 |

---

## 1. M4 任务执行结果

| 任务 | 结果 | 证据 |
|---|---|---|
| M4-01 进程类演练（11 场景） | **11/11 通过** | `docs/M4-01-证据.md`；`scripts/test/.tmp/m4-01/evidence-2026-10-08T13-36-03-036Z/` |
| M4-02 存储类演练（8 场景） | **8/8 通过** | `docs/M4-02-证据.md`；`scripts/test/.tmp/m4-02/evidence-2026-10-08T14-33-22-903Z/` |
| M4-03 协议/并发类演练（11 场景） | **11/11 通过** | `docs/M4-03-证据.md`；`scripts/test/.tmp/m4-03/evidence-2026-10-08T13-21-52-110Z/` |
| M4-04 验收清单与性能基线 | **20/20 项通过**（T1–T14 + DoD4；T15 覆盖率独立门禁通过） | `docs/M4-04-验收报告.md`；`scripts/test/.tmp/m4-04/evidence-2026-10-08T14-55-47-909Z/acceptance.json` |
| M4-05 打包与发布 | **全部通过**：静态/注册包/真实 WebView 内联回环 E2E（含监听/解除监听）+ 发布形态 MSI（三官方注册、无 Mock、哈希一致、首启自检）+ 测试构建安装产物会话闭环（合并验证 4/4 + 冒烟 8/8） | `docs/M4-05-证据.md`；`scripts/test/.tmp/m4-05/evidence-2026-10-09T02-11-18-310Z/`；`dist-out/SHA256SUMS.txt` |
| M4-06 文档与安全收尾 | **全部通过**（文档 5/5、30/30 手册、新成员演练 1589ms、SBOM 归档、36/36 清单、脱敏 0 命中） | `docs/M4-06-证据.md`；`scripts/test/.tmp/m4-06/evidence-2026-10-08T15-05-22-355Z/` |

## 2. 30/30 失败场景演练在案

| 组 | 场景数 | 结果 | 三段证据（触发 → 自动应对 → 恢复） |
|---|---|---|---|
| D2/D5 进程类（M4-01） | 11 | 11/11 | 每场景 `NN-<scenario>.json/.log`（含核心 OOM 与输出洪水的真实 RSS 全链路必过项） |
| D3/D4 存储类（M4-02） | 8 | 8/8 | 同上（含 T8 磁盘满、T12 双开写库） |
| D6/D8 协议/并发类（M4-03） | 11 | 11/11 | 同上（含取消风暴、广播满、控制投递积压） |
| **合计** | **30** | **30/30** | 夜跑接线：`.github/workflows/m4-nightly.yml`（schedule + dispatch，Windows） |

## 3. T1–T15（摘要；详见 M4-04 报告）

| # | 实测要点 | 结论 |
|---|---|---|
| T1 | P50=62ms / P95=64ms（上限 500ms/2s） | ✅ |
| T2 | 单 run P95=34ms（上限 150ms） | ✅ |
| T3 | 控制事件 0 丢失；gap 可补读 | ✅ |
| T4 | kill -9 ×20：loss=[] / false_completions=[] / unreconciled=[] | ✅ |
| T5a / T5b | Ready 5.96s / 31.4s（上限 30s / 120s） | ✅ |
| T6 / T7 | 100 并发 ask 无丢失；路径逃逸 100% deny + 审计（TOCTOU 明示） | ✅ |
| T8 | 只读降级 + 外部路径备份空间护栏（M4-02） | ✅ |
| T9 | 备份→清库→恢复指纹一致 + 审计 | ✅ |
| T10 | >2MiB 断连记错；1–2MiB 正常解析；内存受控 | ✅ |
| T11 | 退出 5.28s；`TerminateJobObject` 优先；无残留 | ✅ |
| T12 / T13 | 单实例聚焦；同步盘拒绝启动（真实 WebView2 E2E） | ✅ |
| T14 | `AETHER_BINDINGS_CHECK PASS` | ✅ |
| T15 | Rust 88.23% / TS 83.09–94.70%（均 ≥70%；负夹具阻断） | ✅ |

## 4. Gate 4 通过条件对照（v1.24 §5）

| 条件 | 判定 | 依据 |
|---|---|---|
| M4 全部 DoD 通过 | **通过**（本机；含安装产物冒烟） | §1；M4-05 证据 |
| T1–T15 全绿 | **通过** | §3；`verify-coverage-gate` 通过 |
| 30/30 失败场景演练在案 | **通过** | §2 |
| 覆盖率 >70% | **通过** | Rust 88.23% / TS ≥83.09% |
| mock-only 路径标记 | 不适用（real-adapter 路径；Gate 1 结论 real-adapter） | M1-Gate1 报告 |

**正式 Go 收口（ADR-016 评审意见 §5-3 次序，两项均已闭环）**：① ADR-016 修订落文并经
**四轮评审结案**（首轮「有条件照准」→ 第二轮「附条件已闭环」→ 第三轮归档复核「通过（闭环）」
→ 第四轮结案；**最终 v0.5「已批准」**）；② **安装产物冒烟 CI 夜跑归档完成**——m4-nightly #2
（id 37892026541，提交 `b157849`）三作业全绿：drills（30 场景）/ acceptance + installer smoke
（MSI 构建 + 解包 + 首启自检 + 三官方注册 + 测试构建闭环 + 内联权限回环）/ docs & security；
代码侧最终提交 `37c94a7` 由 ci #80（22/22 全绿）验证（含 Rust 1.99 兼容修复，见 §7-6）。

## 5. 回退动作对照（分级）

| Gate 4 回退触发 | 状态 |
|---|---|
| 性能类（T1/T2/T3）No-Go | 未触发（T1 62ms、T2 34ms、T3 零丢失） |
| 稳定性类（T4/T5a/T5b/T11）No-Go | 未触发（20 次 kill -9 零丢失、自愈达标） |
| 安全类（T7 及信任边界）No-Go | 未触发（100% deny + 审计；信任边界已在文档明示） |
| 同一项两次 No-Go | 未触发 |
| 安装产物冒烟 | **已通过**（发布形态 MSI + 测试构建闭环 + 首启自检 + 三官方注册；WiX 预置说明见 M4-05 §1 注） |

## 6. 安全基线变更登记（本批次）

- **ADR-016**（`docs/adr/ADR-016-event-listen-minimal-capability.md`）：M4-05 真实 WebView E2E
  发现 D7 `aether://event` 通道因 capabilities 权限集为空而不可用；按流程登记 ADR 并补齐
  最小监听对（`core:event:allow-listen` / `allow-unlisten`），未放宽 CSP/导航/命令校验，
  未引入新插件；安全基线单测（`security_baseline.rs`）与 M1-08 E2E 回归通过；
- **评审与修订**：`docs/ADR-016-评审意见.md`（2026-10-09 首轮「有条件照准」：1 P1 + 5 P2）；
  v0.2 修订已落文（关联行 D1→D7、权限范围事实登记、T2 口径与证据落点、评审记录节、
  评审意见 §6-B1 首选实施——E2E 增 `event-unlisten-probe` 运行时解除监听断言、评审意见 §6-B2
  Gate 4 基线行修正）；第二轮复审**通过（附条件已闭环）**（`docs/ADR-016-复审意见.md`）：
  首轮 1 P1 + 5 P2 全部关闭，独立复跑 security_baseline 6/6、E2E 10/10；第三轮归档复核
  **通过（闭环）**（`docs/ADR-016-第三轮复审意见.md`）；第四轮结案复核：M1/M2 全部关闭、
  评审链结案（`docs/ADR-016-第四轮复审意见.md`）；**ADR-016 最终状态 v0.5「已批准
  （评审链结案）」**——无未闭环评审项；
- **ADR 批准后复核（2026-10-09）**：security_baseline 6/6、E2E 10/10、安装冒烟 8/8；
  产品代码未因 ADR 修订变化（修订为文本/登记级；唯一实现项 `event-unlisten-probe` 已在
  第二轮随批落地并复核），既有 M4 验收证据维持有效，无需重复全量验收；
- 影响面：仅事件监听对；`withGlobalTauri:false`、导航拦截、IPC 校验框架不变。

## 7. 风险与待办（均非阻塞）

| # | 级别 | 事项 | 承接 |
|---|---|---|---|
| 1 | 记录 | 真实干净虚拟机（无 Node/开发工具、无 WebView2）人工复做 + 无 WebView2 引导安装实机验证 | 发布检查清单（`docs/打包与发布说明.md` §4）；本机以 `msiexec /a` 管理式解包近似（已通过）；CI 安装冒烟已归档（m4-nightly #2） |
| 2 | 记录 | **CI 归档已完成**：ci #80（`37c94a7`，22/22 全绿）；m4-nightly #2（`b157849`，drills/acceptance+installer smoke/docs 三作业全绿） | 证据链接见 §8 |
| 3 | 记录 | 人工签核：IPC 复查清单与 M4-05 干净机冒烟记录 | `docs/M4-06-IPC参数校验复查清单.md` 末表 |
| 4 | 记录 | macOS DMG 由 CI macos-14 作业产出（本机为 Windows） | `release-desktop-installers.yml` / `m4-nightly`（ci #80 macOS 构建+冒烟 success） |
| 5 | 记录 | ADR-016 评审链结案（四轮，v0.5「已批准」）**已完成** | `docs/ADR-016-第四轮复审意见.md`；本报告 §6 |
| 6 | 记录 | **Rust 1.99 兼容修复**（ci 首轮失败的根因）：`dec_usize` 弃用 API 改用 CAS 饱和递减（提交 `37c94a7`；语义不变、保持 MSRV 1.80；ci #80 复验全绿） | 本报告 §4；`crates/aether-control/src/pipeline.rs` |

## 8. 证据索引

| 证据 | 路径 | 状态 |
|---|---|---|
| **CI `ci` run #80**（`37c94a7`，22/22 全绿） | https://github.com/Scott-PyMu/Aether/actions/runs/37894745455 | 归档 |
| **CI `m4-nightly` run #2**（`b157849`，三作业全绿：30 场景演练 + acceptance/installer smoke + docs） | https://github.com/Scott-PyMu/Aether/actions/runs/37892026541 | 归档（artifact `m4-drills-evidence` / `m4-acceptance-evidence` / `m4-docs-evidence`） |
| M4-01/02/03 演练证据（30 场景 JSON+日志） | `scripts/test/.tmp/m4-0{1,2,3}/evidence-*/`；CI artifact 同步归档 | 本机 / CI |
| M4-01/02/03 证据文档 | `docs/M4-01-证据.md`…`M4-03-证据.md` | 已提交 |
| M4-04 验收报告 + acceptance.json | `docs/M4-04-验收报告.md`；`.tmp/m4-04/evidence-2026-10-08T14-55-47-909Z/` | 已提交 / 本机 |
| M4-05 证据 + 发布形态 MSI/哈希/安装冒烟 | `docs/M4-05-证据.md`；`.tmp/m4-05/evidence-2026-10-09T02-11-18-310Z/`；`dist-out/Aether_0.1.0_x64_en-US.msi` + `SHA256SUMS.txt` | 已提交 / 本机产物 / CI artifact |
| M4-06 证据 + 依赖清单 | `docs/M4-06-证据.md`；`docs/evidence/m4-06/dependency-inventory.json` | 已提交 |
| ADR-016（四轮评审链） | `docs/adr/ADR-016-*.md` + `docs/ADR-016-{评审,复审,第三轮复审,第四轮复审}意见.md` | 已提交 |
| 夜跑工作流 | `.github/workflows/m4-nightly.yml` | 已提交并实跑（#2 全绿） |
| 覆盖率日志 | `scripts/test/.tmp/m4-run/coverage.log`；CI coverage job（#80 success） | 本机 / CI |

## 9. 判定结论

- **Go（正式）**：P0 技术验收全部闭环——30/30 场景（CI 演练并归档）、T1–T15、命令面/错误码、
  覆盖率、打包发布（发布形态 MSI + 安装产物冒烟 CI 归档）、文档/安全收尾均通过；无回退触发；
- 收口次序（ADR-016 评审意见 §5-3）：① ADR-016 四轮评审链结案、v0.5「已批准」（**已完成**）；
  ② 安装产物冒烟 CI 夜跑归档（**已完成**，m4-nightly #2）；代码侧最终提交 `37c94a7`
  由 ci #80（22/22）验证；
- P0 门禁（ADR-013）= 本 Gate + M1–M4 任务 DoD，**判定通过**；beta 使用期墙钟指标为发布后跟踪 KPI；
- 本报告为 Gate 4 评审记录；CI 归档链接见 §8。
