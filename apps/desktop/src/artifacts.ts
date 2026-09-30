/**
 * 文件引用面板 IPC 契约（M3-09；ADR-010 决策 1/4；UI-UX S-11/§7.3）。
 *
 * 只读引用语义：仅展示与增删引用，**不列目录、不展开文件树、不预览/编辑内容、
 * 不读文件字节**；引用不预授权——Agent 读取仍走 `fs.read` 权限门、写入仍走
 * `fs.write` 审批（D9 不变）。
 *
 * 命令与生成绑定一致（`ref_pick` / `artifacts_list` / `artifact_add` /
 * `artifact_remove`；T14 生成物 `packages/protocol/src/bindings.ts`）。
 */
import { invoke } from "@tauri-apps/api/core";

/** 系统选择器 kind（`ref_pick`；`null` 返回 = 用户取消）。 */
export type RefKind = "file" | "directory";

/** 引用 kind（`artifacts` 表；目录不递归）。 */
export type ArtifactKind = "file" | "directory";

/** `artifacts_list` / `artifact_add` 元素（ADR-010 附录 B.1 形状）。 */
export interface ArtifactEntry {
  id: string;
  path: string;
  kind: ArtifactKind;
  /** 文件字节数；目录为 `null`。 */
  size_bytes: number | null;
  created_at: number;
}

export interface ArtifactsIpc {
  /** 系统选择器（路径原样返回，不 canonicalize；校验在 `artifact_add`）。 */
  pick(kind: RefKind): Promise<string | null>;
  /** 会话引用清单（按 `created_at` 升序）。 */
  list(sessionId: string): Promise<ArtifactEntry[]>;
  /** 登记引用（canonicalize + 可访问性检查；同会话同路径幂等）。 */
  add(sessionId: string, path: string): Promise<ArtifactEntry>;
  /** 删除引用（不存在返回 `false`，幂等）。 */
  remove(sessionId: string, artifactId: string): Promise<boolean>;
}

/** 生产实现（Tauri IPC；命令入参统一 `payload` 包装，与生成绑定一致）。 */
export const artifactsIpc: ArtifactsIpc = {
  async pick(kind) {
    const result = await invoke<{ path: string | null }>("ref_pick", {
      payload: { kind },
    });
    return result.path ?? null;
  },
  async list(sessionId) {
    const result = await invoke<{ artifacts: ArtifactEntry[] }>("artifacts_list", {
      payload: { session_id: sessionId },
    });
    return result.artifacts;
  },
  async add(sessionId, path) {
    return invoke<ArtifactEntry>("artifact_add", {
      payload: { session_id: sessionId, path },
    });
  },
  async remove(sessionId, artifactId) {
    const result = await invoke<{ removed: boolean }>("artifact_remove", {
      payload: { session_id: sessionId, artifact_id: artifactId },
    });
    return result.removed;
  },
};
