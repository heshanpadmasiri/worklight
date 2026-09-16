import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

const WORKLIGHT_BINARY = "__WORKLIGHT_BINARY__";
const COMMAND_TIMEOUT_MS = 10_000;
const SHUTDOWN_TIMEOUT_MS = 12_000;
const MAX_I64 = 9_223_372_036_854_775_807n;

type AgentStatus = "idle" | "working" | "waiting" | "done" | "killed";

type ExecResult = {
  stdout: string;
  stderr: string;
  code: number;
  killed: boolean;
};

function failureDetail(result: ExecResult): string {
  const stderr = result.stderr.trim();
  if (stderr) return stderr;
  if (result.killed) return "process was killed or timed out";
  return `process exited with status ${result.code}`;
}

function parseAgentId(stdout: string): string | undefined {
  const value = stdout.trim();
  if (!/^[1-9][0-9]*$/.test(value)) return undefined;

  try {
    const parsed = BigInt(value);
    if (parsed > MAX_I64 || parsed.toString() !== value) return undefined;
  } catch {
    return undefined;
  }

  return value;
}

export default function worklightAgentTracking(pi: ExtensionAPI): void {
  if (process.env.PI_MULTIPLEXER_SUBAGENT_CHILD === "1") return;

  let agentId: string | undefined;
  let expectedStatus: AgentStatus | undefined;
  let runActive = false;
  let shuttingDown = false;
  let operations: Promise<void> = Promise.resolve();

  function logFailure(action: string, error: unknown): void {
    const detail = error instanceof Error ? error.message : String(error);
    console.error(`worklight: could not ${action}: ${detail}`);
  }

  function enqueue(action: string, operation: () => Promise<void>): void {
    operations = operations.then(async () => {
      try {
        await operation();
      } catch (error) {
        logFailure(action, error);
      }
    });
  }

  async function execute(args: string[]): Promise<ExecResult> {
    return await pi.exec(WORKLIGHT_BINARY, args, {
      timeout: COMMAND_TIMEOUT_MS,
    });
  }

  function enqueueRegistration(): void {
    enqueue("register Pi agent", async () => {
      const result = await execute(["agent", "register", "pi"]);
      if (result.code !== 0 || result.killed) {
        logFailure("register Pi agent", failureDetail(result));
        return;
      }

      const id = parseAgentId(result.stdout);
      if (id === undefined) {
        logFailure("register Pi agent", "invalid agent id in command output");
        return;
      }
      agentId = id;
    });
  }

  function enqueueStatus(status: AgentStatus): void {
    expectedStatus = status;
    enqueue(`report Pi agent status ${status}`, async () => {
      if (agentId === undefined) return;
      const result = await execute(["agent", "status", agentId, status]);
      if (result.code !== 0 || result.killed) {
        logFailure(`report Pi agent status ${status}`, failureDetail(result));
      }
    });
  }

  pi.on("session_start", () => {
    if (shuttingDown) return;
    agentId = undefined;
    expectedStatus = "idle";
    runActive = false;
    shuttingDown = false;
    operations = Promise.resolve();
    enqueueRegistration();
  });

  pi.on("before_agent_start", () => {
    if (shuttingDown) return;
    runActive = true;
    enqueueStatus("working");
  });

  pi.on("ui_prompt_start", (_event, ctx) => {
    if (shuttingDown || (!runActive && ctx.isIdle())) return;
    runActive = true;
    if (expectedStatus !== "working") enqueueStatus("working");
    enqueueStatus("waiting");
  });

  pi.on("ui_prompt_end", () => {
    if (shuttingDown || expectedStatus !== "waiting") return;
    enqueueStatus("working");
  });

  pi.on("agent_settled", () => {
    if (shuttingDown || !runActive) return;
    enqueueStatus("working");
    enqueueStatus("done");
    runActive = false;
  });

  pi.on("session_shutdown", async () => {
    if (shuttingDown) return;
    shuttingDown = true;
    enqueueStatus("killed");

    let timer: ReturnType<typeof setTimeout> | undefined;
    try {
      await Promise.race([
        operations,
        new Promise<void>((resolve) => {
          timer = setTimeout(resolve, SHUTDOWN_TIMEOUT_MS);
        }),
      ]);
    } finally {
      if (timer !== undefined) clearTimeout(timer);
    }
  });
}
