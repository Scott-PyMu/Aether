/**
 * 拒绝启动界面（M1-06 / A4 / 评审 #9）。
 *
 * 检测命中同步盘后只提供两个动作：「迁移到本地目录」与「退出」；
 * 不提供任何原地运行的覆盖开关（A4 / 评审 #9）。主界面在 App 层不可达，
 * 业务命令在命令层返回 `startup_blocked`（双重兜底）。
 */
import { useCallback, useState } from "react";

import {
  describeIpcError,
  exitApp,
  migrateDataDir,
  pickMigrationTarget,
  type StartupSnapshot,
} from "./startup";

interface StartupGateProps {
  snapshot: StartupSnapshot;
  onMigrated: (snapshot: StartupSnapshot) => void;
}

export function StartupGate({ snapshot, onMigrated }: StartupGateProps) {
  const [target, setTarget] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const isHardError = snapshot.phase === "blocked_error";

  const pick = useCallback(async () => {
    setError(null);
    try {
      const picked = await pickMigrationTarget();
      if (picked) {
        setTarget(picked);
      }
    } catch (pickError) {
      setError(describeIpcError(pickError));
    }
  }, []);

  const migrate = useCallback(async () => {
    setBusy(true);
    setError(null);
    try {
      const migrated = await migrateDataDir(target.trim());
      onMigrated(migrated);
    } catch (migrateError) {
      setError(describeIpcError(migrateError));
    } finally {
      setBusy(false);
    }
  }, [onMigrated, target]);

  const exit = useCallback(() => {
    void exitApp();
  }, []);

  return (
    <main className="startup-gate" data-testid="startup-gate">
      <h1 className="startup-gate-title">数据目录检测未通过</h1>
      {isHardError ? (
        <p className="startup-gate-message" data-testid="startup-message">
          {snapshot.message}
        </p>
      ) : (
        <>
          <p className="startup-gate-message">
            数据目录位于同步盘 / 云目录（OneDrive、Dropbox、iCloud、网络盘等）。
            为保证数据库完整性，应用已拒绝启动；迁移数据到本地目录后才能进入主界面。
          </p>
          <p className="startup-gate-dir" data-testid="startup-data-dir">
            当前数据目录：{snapshot.data_dir}
          </p>
          <ul className="startup-gate-reasons" data-testid="startup-reasons">
            {(snapshot.detection?.reasons ?? []).map((reason) => (
              <li key={reason}>{reason}</li>
            ))}
          </ul>
          {snapshot.detection?.note ? (
            <p className="startup-gate-note" data-testid="startup-precision-note">
              {snapshot.detection.note}
            </p>
          ) : null}
          <label className="startup-gate-field">
            <span>迁移目标目录（本地磁盘，必须为空目录）</span>
            <input
              data-testid="startup-target"
              value={target}
              onChange={(event) => setTarget(event.target.value)}
              placeholder="例如 D:\\AetherData"
              spellCheck={false}
            />
          </label>
          <div className="startup-gate-actions">
            <button
              type="button"
              data-testid="startup-pick"
              onClick={pick}
              disabled={busy}
            >
              选择目录…
            </button>
            <button
              type="button"
              data-testid="startup-migrate"
              onClick={migrate}
              disabled={busy || target.trim().length === 0}
            >
              {busy ? "正在迁移…" : "迁移到本地目录"}
            </button>
          </div>
        </>
      )}
      {error ? (
        <p className="startup-gate-error" data-testid="startup-error">
          {error}
        </p>
      ) : null}
      <button
        type="button"
        className="startup-gate-exit"
        data-testid="startup-exit"
        onClick={exit}
      >
        退出
      </button>
    </main>
  );
}
