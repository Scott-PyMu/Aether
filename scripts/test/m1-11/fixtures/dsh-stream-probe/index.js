/**
 * M1-11 适配器增强可行性探针（fixture，不入生产）。
 *
 * 用途：验证「仅通过 --patch 注入 Cordis 插件即可拿到 DSH 原始 provider delta」。
 * 不拷贝任何上游/第三方代码；事件契约来自 DSH 公开包运行时可观察行为。
 *
 * 用法（隔离 DSH_HOME）：
 *   1. 把本目录复制到 <DSH_HOME>/profiles/acp/node_modules/dsh-stream-probe（必须放 profile 目录，
 *      且 package.json 必须无 BOM）；
 *   2. 用 deepseek-harness-acp-spike.mjs 并设置 M1_11_DSH_ACP_PATCH 指向 patch.yml；
 *   3. 运行后从记录 stderr 观察 SPIKE_FRAME 行（chunkType=text-delta / reasoning-delta / tool-call-delta）。
 */
export const name = 'spike-stream';

export function apply(ctx) {
  process.stderr.write('SPIKE_STREAM_PLUGIN_LOADED\n');
  ctx.on('agent/assistant-stream', ({ frame }) => {
    const chunk = frame?.chunk;
    process.stderr.write(
      `SPIKE_FRAME type=${frame?.type} attempt=${frame?.attemptId} chunkType=${chunk?.type || ''} len=${chunk?.text?.length ?? ''}\n`,
    );
  });
}
