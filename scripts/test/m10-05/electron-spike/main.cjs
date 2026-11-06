/**
 * Q3 解码 spike：Electron 主进程（仅测试用最小壳）。
 *
 * 用法：AetherDecodeSpike.exe --spike --spike-out=<path>
 * 通过 utilityProcess.fork 启动 worker，验证打包后 N-API 从 asarUnpack 加载。
 */
const { app, utilityProcess } = require("electron");
const { writeFileSync } = require("node:fs");
const path = require("node:path");

const spikeIndex = process.argv.indexOf("--spike");
if (spikeIndex >= 0) {
  app.disableHardwareAcceleration();
  app.whenReady().then(() => {
    const outArg = process.argv.find((arg) => arg.startsWith("--spike-out="));
    const outFile = outArg ? outArg.slice("--spike-out=".length) : path.join(app.getPath("temp"), "aether-m10-05-spike.json");
    const workerPath = path.join(__dirname, "worker.cjs");
    const child = utilityProcess.fork(workerPath, [], {
      serviceName: "aether-m10-05-decode-spike",
      stdio: "pipe",
      env: {
        ...process.env,
        AETHER_SPIKE_APP_ROOT: app.getAppPath(),
        AETHER_SPIKE_RESOURCES: process.resourcesPath,
        AETHER_SPIKE_IS_PACKAGED: app.isPackaged ? "1" : "0",
      },
    });
    let stdout = "";
    let stderr = "";
    let settled = false;
    const settle = (payload) => {
      if (settled) return;
      settled = true;
      writeFileSync(outFile, `${JSON.stringify(payload, null, 2)}\n`, "utf8");
      try {
        child.kill();
      } catch {
        /* already gone */
      }
      app.quit();
    };
    child.stdout?.on("data", (chunk) => {
      stdout += String(chunk);
    });
    child.stderr?.on("data", (chunk) => {
      stderr += String(chunk);
    });
    child.on("message", (message) => {
      settle({ ok: true, worker: message, stdout, stderr });
    });
    child.on("exit", (code) => {
      if (!settled) settle({ ok: false, error: `worker exited ${code}`, stdout, stderr });
    });
    child.postMessage({ type: "run", appRoot: app.getAppPath(), resources: process.resourcesPath });
    setTimeout(() => {
      if (!settled) settle({ ok: false, error: "worker timeout", stdout, stderr });
    }, 60_000).unref();
  });
}
