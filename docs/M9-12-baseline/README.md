# M9-12 基线：前端覆盖率与 CI 证据口径

> 录制于 M9-12 开工前（HEAD `a2b3f72`），用于执行「前端覆盖率不下降」门禁。
> M9-12 不修改 `docs/M9-01-baseline/` 的冻结契约；本目录只承载 M9-12 新增的
> 覆盖率基线与删除清单口径。

## 文件

| 文件 | 用途 |
|---|---|
| `frontend-coverage.json` | `pnpm --filter @aether/desktop coverage` 的总计（lines/statements/branches/functions）与测试数（59 文件/364 测试） |
| （生成）`scripts/test/.tmp/m9-12/coverage/tauri-deletion-list.json` | G8 待删除面（`crates/aether-tauri/**` + `packages/protocol/src/bindings.ts`）；M9-12 运行时生成，不入库 |
| （生成）`scripts/test/.tmp/m9-12/coverage/incremental.json` | CI `cargo llvm-cov report` 解析出的 `aether-app`/`aether-rpc`/`aether-daemon` 增量覆盖 |

## 校验

```bash
pnpm verify:m9-12
```

- 若 `apps/desktop/coverage/coverage-summary.json` 存在，逐项比较并拒绝低于基线；
- CI coverage job 在 `pnpm -r coverage` 后执行 `pnpm verify:m9-12`，随后生成增量覆盖。
- 基线只在 M9-12 录制；后续任务不得为了让门禁通过而下调基线（AGENTS §3）。

## 与 ADR-029 §8.2（P2-7）的关系

M9-02～G8 期间 Rust 覆盖率门禁排除 `aether-tauri`，并以 `aether-app`/`aether-rpc`/
`aether-daemon` 的增量数字 + Tauri 删除文件清单替代；G8 恢复全量并给出前后对比表
（M9-13/M9-14）。本目录不改变门禁口径。
