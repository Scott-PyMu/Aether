/**
 * 适配器基类（D6）：hello 握手、方法分发、事件信封上报、无效帧计数。
 *
 * 使用方式（Mock 适配器即基于本类）：
 * ```ts
 * const adapter = new Adapter({ runtime: {...}, lines, writeLine });
 * adapter.handle("initialize", () => ({ capabilities: [...] }));
 * await adapter.run();
 * ```
 */

import { buildEnvelope, SessionSequencer, type EnvelopeContext } from "./envelope";
import {
  ARTIFACT_REF_METHOD,
  ARTIFACT_REF_TYPE,
  INVALID_FRAME_UNHEALTHY_THRESHOLD,
  NOTIFICATION_EVENT,
  NOTIFICATION_HELLO,
  NOTIFICATION_PERMISSION_REQUEST,
  PROTOCOL_VERSION,
  type ArtifactRefParams,
} from "./protocol";
import { JsonRpcPeer, RpcError } from "./rpc";

export interface AdapterRuntimeInfo {
  name: string;
  version: string;
  capabilities?: string[];
}

export interface AdapterOptions {
  runtime: AdapterRuntimeInfo;
  /** 入站帧（核心 → 适配器），通常为 stdin 读取器。 */
  lines: AsyncIterable<string>;
  /** 出站帧写入口（一行一次），通常为 stdout 写入器。 */
  writeLine: (line: string) => Promise<void>;
  /** 诊断日志（必须走 stderr，禁止混入 stdout，D6 失败场景表）。 */
  stderr?: (line: string) => void;
  /** 是否发送 hello（默认发送；测试可关闭以验证握手超时）。 */
  sendHello?: boolean;
  /** 连续无效帧阈值（默认 20，D6 硬阈值）。 */
  invalidFrameThreshold?: number;
}

export type MethodHandler = (params: unknown) => unknown | Promise<unknown>;

export class Adapter {
  readonly peer: JsonRpcPeer;

  private readonly handlers = new Map<string, MethodHandler>();
  private readonly sequencers = new Map<string, SessionSequencer>();
  private readonly runtime: AdapterRuntimeInfo;
  private readonly lines: AsyncIterable<string>;
  private readonly stderr: (line: string) => void;
  private readonly sendHelloEnabled: boolean;
  private readonly invalidFrameThreshold: number;

  private invalidFrameStreak = 0;
  private invalidFramesTotal = 0;
  private stopped = false;

  constructor(options: AdapterOptions) {
    this.runtime = options.runtime;
    this.lines = options.lines;
    this.stderr = options.stderr ?? (() => {});
    this.sendHelloEnabled = options.sendHello ?? true;
    this.invalidFrameThreshold = options.invalidFrameThreshold ?? INVALID_FRAME_UNHEALTHY_THRESHOLD;
    this.peer = new JsonRpcPeer(options.writeLine, {
      onRequest: (method, params) => this.dispatch(method, params),
      onNotification: async () => {
        // MVP：核心 → 适配器无通知；未知通知忽略（前向兼容）。
      },
    });
  }

  /** 注册方法处理器；未注册的方法回 `-32601`（不断连）。 */
  handle(method: string, handler: MethodHandler): this {
    this.handlers.set(method, handler);
    return this;
  }

  /** 会话 sequencer（信封 `seq` 单会话单调唯一）。 */
  sequencer(sessionId: string): SessionSequencer {
    let sequencer = this.sequencers.get(sessionId);
    if (!sequencer) {
      sequencer = new SessionSequencer(sessionId);
      this.sequencers.set(sessionId, sequencer);
    }
    return sequencer;
  }

  /** 发送 `event` 通知（附录 B 类型；payload 必须与 Rust 侧结构严格一致）。 */
  async emitEvent(
    context: EnvelopeContext,
    type: string,
    payload: Record<string, unknown>,
  ): Promise<void> {
    const seq = this.sequencer(context.sessionId).next();
    const envelope = buildEnvelope(context, type, payload, seq);
    await this.peer.notify(NOTIFICATION_EVENT, envelope);
  }

  /** 发送 `permission.request` 通知（D6/D9 回环的适配器侧入口）。 */
  async emitPermissionRequest(payload: Record<string, unknown>): Promise<void> {
    await this.peer.notify(NOTIFICATION_PERMISSION_REQUEST, payload);
  }

  /** 发送 `log` 通知。 */
  async emitLog(params: Record<string, unknown>): Promise<void> {
    await this.peer.notify("log", params);
  }

  /**
   * 发送 `artifact_ref` 引用帧（M2-09/D6）：附件内容存 artifacts 文件（经
   * `AETHER_ARTIFACTS_DIR` 注入），线协议只携带路径 + 元数据；引用帧必须 <1MiB。
   */
  async emitArtifactRef(params: ArtifactRefParams): Promise<void> {
    await this.peer.notifyFrame(ARTIFACT_REF_METHOD, { type: ARTIFACT_REF_TYPE }, params);
  }

  /** 诊断输出（stderr）。 */
  log(level: string, message: string): void {
    this.stderr(`[${level}] ${message}`);
  }

  /** 运行读循环：先发 hello，再逐帧处理；连续无效帧达阈值即停止。 */
  async run(): Promise<void> {
    if (this.sendHelloEnabled) {
      await this.sendHello();
    }
    for await (const line of this.lines) {
      const valid = await this.peer.handleLine(line);
      if (valid) {
        this.invalidFrameStreak = 0;
      } else {
        this.invalidFramesTotal += 1;
        this.invalidFrameStreak += 1;
        this.log("warn", `无效帧（连续 ${this.invalidFrameStreak}/${this.invalidFrameThreshold}）`);
        if (this.invalidFrameStreak >= this.invalidFrameThreshold) {
          this.log("error", "连续无效帧达到阈值，停止处理（D6）");
          break;
        }
      }
      if (this.stopped) break;
    }
  }

  stop(): void {
    this.stopped = true;
  }

  get invalidFrameCount(): { total: number; streak: number } {
    return { total: this.invalidFramesTotal, streak: this.invalidFrameStreak };
  }

  private async sendHello(): Promise<void> {
    const runtime: Record<string, unknown> = {
      name: this.runtime.name,
      version: this.runtime.version,
      capabilities: this.runtime.capabilities ?? [],
    };
    await this.peer.notify(NOTIFICATION_HELLO, {
      protocol: PROTOCOL_VERSION,
      runtime,
    });
  }

  private async dispatch(method: string, params: unknown): Promise<unknown> {
    const handler = this.handlers.get(method);
    if (!handler) {
      throw new RpcError(-32601, `Method not found: ${method}`);
    }
    return handler(params);
  }
}
