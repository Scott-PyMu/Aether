# M10-09 Windows `CURRENT` 原子替换说明（受限项）

> 关联：ADR-039 v1.0 D-039-8 §1｜M10-09｜2026-11-15

## 1. 要求

ADR-039 D-039-8 §1 要求 `CURRENT` 原子替换：

- 先写 `CURRENT.tmp` 并 `fsync`/`FlushFileBuffers`；
- 同卷替换：POSIX `rename(2)` + 目录 `fsync`；Windows 已有 `CURRENT` 用
  `ReplaceFileW`（或 `MoveFileExW(MOVEFILE_REPLACE_EXISTING|MOVEFILE_WRITE_THROUGH)`）；
- 首次创建用不带 replace 的 `MoveFileExW`；禁止“删除旧文件再 rename”。

## 2. 实现现状

`crates/aether-store/src/layout.rs::replace_file_atomic`：

- 临时文件先 `sync_all()`（数据落盘）；
- 使用 `std::fs::rename(tmp, dest)`。在 Windows 上，标准库映射到
  `MoveFileExW(MOVEFILE_REPLACE_EXISTING)`（同卷命名空间原子替换）；
- 不删除旧文件后再 rename；不跨卷。

**偏差**：未直接调用 `ReplaceFileW`/`MOVEFILE_WRITE_THROUGH`；目录 `fsync` 在
Windows 不受支持。

## 3. 原因

workspace 根 `Cargo.toml` 设置：

```toml
[workspace.lints.rust]
unsafe_code = "forbid"
```

`ReplaceFileW`/`MoveFileExW` 在 `windows` crate 中均为 `unsafe` 调用；`forbid`
无法在 crate/模块级用 `#[allow(unsafe_code)]` 覆盖。为遵守 AGENTS §2/§6 的
workspace 硬约束，本任务不引入 unsafe FFI。

## 4. 风险与缓解

- `std::fs::rename` 的 `MoveFileExW(MOVEFILE_REPLACE_EXISTING)` 保证命名空间替换原子性；
  `CURRENT.tmp` 已 `sync_all`，崩溃场景下最多保留旧 `CURRENT` 或新 `CURRENT`，不会出现
  “删除旧文件后崩溃导致无 `CURRENT`”。
- 未使用 `MOVEFILE_WRITE_THROUGH`：写入顺序/掉电持久性略弱于 ADR 字面要求；已通过
  启动校验（`CURRENT` 可读 + `shards_sha256` 匹配 + generation 完整）fail-closed 兜底。
- 未执行 Windows API 失败注入（`ReplaceFileW`/`MoveFileExW` 失败）。

## 5. 后续

- 本偏差已登记为 M10-09 证据 §6/`docs/M10-09-metrics.json` 的未验证项。
- 若架构/安全要求严格按 ADR 字面实现，需在 M10-10/G13 前另行决策：
  1) 调整 workspace `unsafe_code` 策略并在独立小 crate 内封装 Windows API；或
  2) 另立 ADR 接受 `std::fs::rename` 的等价原子替换语义。
- 在 G13 签核前不得把该项标记为“已按 ADR 完整实现”。
