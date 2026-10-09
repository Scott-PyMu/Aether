/**
 * M4 演练共享库（M4-01/02/03 复用）。
 *
 * 职责：
 *   - 统一执行演练步骤（cargo/pnpm/node 命令，捕获 stdout+stderr）；
 *   - 按「触发 → 自动应对 → 恢复」三段提取证据行并归档；
 *   - 输出每场景 JSON（含三段证据）+ 原始日志 + summary.json。
 *
 * 说明：M4 演练与任务级 DoD 验证为分层关系（实施计划 §5 M4-01 分层说明）：
 *   M2/M3 验证单点行为正确（单测/集成），M4 验证端到端脚本化执行稳定 +
 *   三段证据归档。本库不重复实现断言，仅编排既有测试并提取其输出证据。
 */
import { spawnSync } from "node:child_process";
import { existsSync, mkdirSync, writeFileSync } from "node:fs";
import path from "node:path";
import process from "node:process";

import { repoRoot } from "../../lib/exec.mjs";

const PHASES = ["trigger", "response", "recovery"];

/** 归一化时间戳为文件系统安全的字符串。 */
export function stampNow() {
  return new Date().toISOString().replace(/[:.]/g, "-");
}

/**
 * 创建演练运行器。
 *
 * @param {object} options
 * @param {string} options.task      任务 ID（如 "M4-01"）
 * @param {string} options.title     标题
 * @param {string} [options.evidenceBase] 证据根目录（默认 scripts/test/.tmp/<task>/）
 */
export function createDrill(options) {
  const { task, title } = options;
  const stamp = stampNow();
  const evidenceDir = path.join(
    repoRoot,
    "scripts",
    "test",
    ".tmp",
    task.toLowerCase(),
    `evidence-${stamp}`,
  );
  const scenarios = [];
  const scenarioResults = [];

  function capture(command, args, { env, cwd, timeoutMs } = {}) {
    const started = Date.now();
    const result = spawnSync(command, args, {
      cwd: cwd ?? repoRoot,
      env: { ...process.env, ...(env ?? {}) },
      encoding: "utf8",
      maxBuffer: 64 * 1024 * 1024,
      timeout: timeoutMs ?? 30 * 60 * 1000,
    });
    const elapsedMs = Date.now() - started;
    const out = `${result.stdout ?? ""}${result.stderr ?? ""}`;
    return { status: result.status ?? 1, out, elapsedMs, error: result.error };
  }

  /** 在一段输出中查找以 prefix 开头的整行（trim 后）。 */
  function findLines(out, prefix) {
    return out
      .split(/\r?\n/)
      .map((line) => line.replace(/^\uFEFF/, "").trimEnd())
      .filter((line) => line.trim().startsWith(prefix));
  }

  /**
   * 注册场景定义（按注册顺序执行）。
   *
   * definition = {
   *   id, name, source,
   *   trigger: string, response: string, recovery: string,   // 三段说明
   *   steps: [{ name, command, args, env?, cwd?, timeoutMs?,
   *             markers?: [{ phase, prefix }] }],
   * }
   */
  function define(definition) {
    scenarios.push(definition);
  }

  function executeScenario(definition) {
    const logParts = [];
    const stepResults = [];
    const phaseEvidence = { trigger: [], response: [], recovery: [] };
    let pass = true;
    let failureReason = "";

    for (const step of definition.steps) {
      console.log(`\n$ ${step.command} ${step.args.join(" ")}   # ${definition.id} / ${step.name}`);
      const result = capture(step.command, step.args, {
        env: step.env,
        cwd: step.cwd,
        timeoutMs: step.timeoutMs,
      });
      process.stdout.write(result.out);
      if (result.error) {
        console.error(`[drill] 无法执行：${result.error.message}`);
      }
      console.log(`[drill] ${definition.id} / ${step.name} exit=${result.status} elapsed=${result.elapsedMs}ms`);

      const markers = [];
      for (const marker of step.markers ?? []) {
        const lines = marker.contains
          ? result.out
              .split(/\r?\n/)
              .map((line) => line.replace(/^\uFEFF/, "").trimEnd())
              .filter((line) => line.includes(marker.prefix))
          : findLines(result.out, marker.prefix);
        for (const line of lines) {
          phaseEvidence[marker.phase].push(line.trim());
        }
        let markerOk = lines.length > 0;
        // 可选 JSON 字段断言：对以 prefix 起始的行解析 JSON 并断言（如 T11 单 JSON
        // 承载「触发/应对/恢复」三阶段时，按字段分别提取证据）。
        if (markerOk && marker.assert) {
          markerOk = lines.some((line) => {
            const index = line.indexOf(marker.prefix);
            const payload = line.slice(index + marker.prefix.length).trim();
            try {
              return marker.assert(JSON.parse(payload));
            } catch {
              return false;
            }
          });
        }
        markers.push({
          phase: marker.phase,
          prefix: marker.prefix,
          found: lines.length,
          asserted: marker.assert ? markerOk : undefined,
        });
        if (!markerOk && marker.required !== false) {
          pass = false;
          failureReason ||= `步骤「${step.name}」缺少 ${marker.phase} 证据行：${marker.prefix}`;
        }
      }
      if (result.status !== 0) {
        pass = false;
        failureReason ||= `步骤「${step.name}」退出码 ${result.status}`;
      }

      stepResults.push({
        name: step.name,
        command: `${step.command} ${step.args.join(" ")}`,
        exit: result.status,
        elapsed_ms: result.elapsedMs,
        markers,
      });
      logParts.push(`===== ${step.name} =====\n$ ${step.command} ${step.args.join(" ")}\n${result.out}`);
    }

    return {
      result: {
        task,
        scenario: definition.id,
        name: definition.name,
        source: definition.source,
        phases: {
          trigger: {
            description: definition.trigger,
            evidence: phaseEvidence.trigger,
          },
          response: {
            description: definition.response,
            evidence: phaseEvidence.response,
          },
          recovery: {
            description: definition.recovery,
            evidence: phaseEvidence.recovery,
          },
        },
        steps: stepResults,
        pass,
        ...(pass ? {} : { failure_reason: failureReason }),
      },
      log: logParts.join("\n"),
    };
  }

  async function runAll({ only } = {}) {
    mkdirSync(evidenceDir, { recursive: true });
    const selected = only
      ? scenarios.filter((scenario) => only.includes(scenario.id))
      : scenarios;
    if (only && selected.length === 0) {
      console.error(`[drill] --only 未匹配任何场景：${only.join(", ")}`);
      process.exit(2);
    }

    for (const definition of selected) {
      console.log(`\n########## ${task} 场景：${definition.id}（${definition.name}） ##########`);
      const { result, log } = executeScenario(definition);
      scenarioResults.push(result);

      writeFileSync(
        path.join(evidenceDir, `${String(scenarioResults.length).padStart(2, "0")}-${definition.id}.log`),
        `${log}\n`,
        "utf8",
      );
      writeFileSync(
        path.join(evidenceDir, `${String(scenarioResults.length).padStart(2, "0")}-${definition.id}.json`),
        `${JSON.stringify(result, null, 2)}\n`,
        "utf8",
      );
      console.log(
        `[drill] 场景 ${definition.id}：${result.pass ? "PASS" : `FAIL（${result.failure_reason}）`}`,
      );
    }

    return scenarioResults;
  }

  function summarize() {
    const failed = scenarioResults.filter((result) => !result.pass);
    const summary = {
      task,
      title,
      stamp,
      total: scenarioResults.length,
      passed: scenarioResults.length - failed.length,
      failed: failed.length,
      scenarios: scenarioResults.map((result) => ({
        scenario: result.scenario,
        name: result.name,
        pass: result.pass,
        ...(result.failure_reason ? { failure_reason: result.failure_reason } : {}),
      })),
    };
    writeFileSync(
      path.join(evidenceDir, "summary.json"),
      `${JSON.stringify(summary, null, 2)}\n`,
      "utf8",
    );

    console.log(`\n===== ${task} ${title} =====`);
    for (const result of scenarioResults) {
      console.log(
        `${result.pass ? "PASS" : "FAIL"}  ${String(scenarioResults.indexOf(result) + 1).padStart(2, "0")}. ` +
          `${result.scenario}（${result.name}）`
          + (result.failure_reason ? ` — ${result.failure_reason}` : ""),
      );
    }
    console.log(`----- ${failed.length === 0 ? `全部通过（${scenarioResults.length}/${scenarios.length} 场景）` : `${failed.length} 场景失败`} -----`);
    console.log(`[drill] 证据目录：${evidenceDir}`);
    return failed.length === 0 ? 0 : 1;
  }

  return { define, runAll, summarize, evidenceDir, stamp, capture, findLines };
}

/** 解析 --only a,b,c 参数。 */
export function parseOnly(argv = process.argv) {
  const index = argv.indexOf("--only");
  if (index < 0 || !argv[index + 1]) return null;
  return argv[index + 1]
    .split(",")
    .map((value) => value.trim())
    .filter(Boolean);
}

/** 确认路径存在（构建前置检查用）。 */
export function requirePath(label, target) {
  if (existsSync(target)) return true;
  console.error(`[drill] 缺少${label}：${target}`);
  return false;
}

export { PHASES };
