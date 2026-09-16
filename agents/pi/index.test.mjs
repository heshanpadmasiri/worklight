#!/usr/bin/env node
// Dependency-free lifecycle harness. Node 22+ runs the TypeScript extension via
// built-in type stripping: `node agents/pi/index.test.mjs`.

import assert from "node:assert/strict";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { pathToFileURL } from "node:url";

const SOURCE = resolve("agents/pi/index.ts");
const TEST_BINARY = resolve("/tmp/worklight lifecycle test/worklight");
const temp = await mkdtemp(join(tmpdir(), "worklight-pi-extension-"));
const renderedExtension = join(temp, "index.ts");
const source = await readFile(SOURCE, "utf8");
await writeFile(
  renderedExtension,
  source.replace('"__WORKLIGHT_BINARY__"', JSON.stringify(TEST_BINARY)),
);
const { default: extension } = await import(pathToFileURL(renderedExtension).href);

function success(stdout = "") {
  return { stdout, stderr: "", code: 0, killed: false };
}

function deferred() {
  let resolvePromise;
  const promise = new Promise((resolve) => {
    resolvePromise = resolve;
  });
  return { promise, resolve: resolvePromise };
}

function fakeApi(exec = async (_command, args) => success(args[1] === "register" ? "1\n" : "")) {
  const handlers = new Map();
  const calls = [];
  const api = {
    on(event, handler) {
      const existing = handlers.get(event) ?? [];
      existing.push(handler);
      handlers.set(event, existing);
    },
    async exec(command, args, options) {
      calls.push({ command, args: [...args], options: { ...options } });
      return await exec(command, args, options, calls.length - 1);
    },
  };
  extension(api);
  return {
    calls,
    async emit(event, ctx = { isIdle: () => true }) {
      for (const handler of handlers.get(event) ?? []) {
        await handler({}, ctx);
      }
    },
  };
}

async function waitFor(predicate, message) {
  const deadline = Date.now() + 2_000;
  while (!predicate()) {
    if (Date.now() >= deadline) throw new Error(`timed out: ${message}`);
    await new Promise((resolvePromise) => setTimeout(resolvePromise, 0));
  }
}

function commandArgs(runtime) {
  return runtime.calls.map((call) => call.args);
}

async function testSubagentChildrenAreIgnored() {
  process.env.PI_MULTIPLEXER_SUBAGENT_CHILD = "1";
  try {
    const runtime = fakeApi();
    await runtime.emit("session_start");
    await runtime.emit("before_agent_start");
    await runtime.emit("session_shutdown");
    assert.deepEqual(runtime.calls, []);
  } finally {
    delete process.env.PI_MULTIPLEXER_SUBAGENT_CHILD;
  }
}

async function testLifecycleOrder() {
  const runtime = fakeApi();
  await runtime.emit("session_start");
  await runtime.emit("before_agent_start");
  await runtime.emit("ui_prompt_start", { isIdle: () => false });
  await runtime.emit("ui_prompt_end");
  await runtime.emit("agent_settled");
  await runtime.emit("session_shutdown");

  assert.deepEqual(commandArgs(runtime), [
    ["agent", "register", "pi"],
    ["agent", "status", "1", "working"],
    ["agent", "status", "1", "waiting"],
    ["agent", "status", "1", "working"],
    ["agent", "status", "1", "working"],
    ["agent", "status", "1", "done"],
    ["agent", "status", "1", "killed"],
  ]);
  for (const call of runtime.calls) {
    assert.equal(call.command, TEST_BINARY);
    assert.equal(call.options.timeout, 10_000);
  }
}

async function testDoneAgentReactivatesForAnotherRun() {
  const runtime = fakeApi();
  await runtime.emit("session_start");
  await runtime.emit("before_agent_start");
  await runtime.emit("agent_settled");
  await runtime.emit("before_agent_start");
  await runtime.emit("agent_settled");
  await runtime.emit("session_shutdown");

  assert.deepEqual(commandArgs(runtime).map((args) => args[3]).filter(Boolean), [
    "working",
    "working",
    "done",
    "working",
    "working",
    "done",
    "killed",
  ]);
}

async function testPromptOrderingDoesNotWaitForExec() {
  const waiting = deferred();
  const runtime = fakeApi(async (_command, args) => {
    if (args[1] === "register") return success("7");
    if (args[3] === "waiting") return await waiting.promise;
    return success();
  });
  await runtime.emit("session_start");
  await runtime.emit("before_agent_start");
  await runtime.emit("ui_prompt_start", { isIdle: () => false });
  await runtime.emit("ui_prompt_end");
  await waitFor(
    () => commandArgs(runtime).some((args) => args[3] === "waiting"),
    "waiting command",
  );
  waiting.resolve(success());
  await runtime.emit("session_shutdown");

  assert.deepEqual(commandArgs(runtime).map((args) => args[3]).filter(Boolean), [
    "working",
    "waiting",
    "working",
    "killed",
  ]);
}

async function testSettlementWinsOverLatePromptEnd() {
  const runtime = fakeApi();
  await runtime.emit("session_start");
  await runtime.emit("before_agent_start");
  await runtime.emit("ui_prompt_start", { isIdle: () => false });
  await runtime.emit("agent_settled");
  await runtime.emit("ui_prompt_end");
  await runtime.emit("session_shutdown");

  assert.deepEqual(commandArgs(runtime).map((args) => args[3]).filter(Boolean), [
    "working",
    "waiting",
    "working",
    "done",
    "killed",
  ]);
}

async function testPromptClassificationAndShutdownGuard() {
  const runtime = fakeApi();
  await runtime.emit("session_start");
  await runtime.emit("ui_prompt_start", { isIdle: () => true });
  await runtime.emit("ui_prompt_end");
  await runtime.emit("ui_prompt_start", { isIdle: () => false });
  await runtime.emit("session_shutdown");
  await runtime.emit("before_agent_start");
  await runtime.emit("ui_prompt_end");
  await runtime.emit("agent_settled");

  assert.deepEqual(commandArgs(runtime).map((args) => args[3]).filter(Boolean), [
    "working",
    "waiting",
    "killed",
  ]);
}

async function testExactStringIds() {
  for (const id of ["1", "9223372036854775807"]) {
    const runtime = fakeApi(async (_command, args) =>
      success(args[1] === "register" ? `  ${id}\n` : ""),
    );
    await runtime.emit("session_start");
    await runtime.emit("before_agent_start");
    await runtime.emit("session_shutdown");
    assert.equal(runtime.calls[1].args[2], id);
    assert.equal(typeof runtime.calls[1].args[2], "string");
  }

  const invalid = [
    "",
    "0",
    "-1",
    "+1",
    "01",
    "1.0",
    "1\n2",
    "9223372036854775808",
  ];
  for (const output of invalid) {
    const runtime = fakeApi(async () => success(output));
    await runtime.emit("session_start");
    await runtime.emit("before_agent_start");
    await runtime.emit("session_shutdown");
    assert.deepEqual(commandArgs(runtime), [["agent", "register", "pi"]], output);
  }
}

async function testFailuresAreIsolatedAndWorkingRepairsSettlement() {
  let firstWorking = true;
  const runtime = fakeApi(async (_command, args) => {
    if (args[1] === "register") return success("3");
    if (args[3] === "working" && firstWorking) {
      firstWorking = false;
      throw new Error("synthetic exec failure");
    }
    if (args[3] === "waiting") {
      return { stdout: "", stderr: "timeout", code: 1, killed: true };
    }
    return success();
  });
  await runtime.emit("session_start");
  await runtime.emit("before_agent_start");
  await runtime.emit("ui_prompt_start", { isIdle: () => false });
  await runtime.emit("agent_settled");
  await runtime.emit("session_shutdown");

  assert.deepEqual(commandArgs(runtime).map((args) => args[3]).filter(Boolean), [
    "working",
    "waiting",
    "working",
    "done",
    "killed",
  ]);
}

async function testRegistrationFailureSuppressesStatuses() {
  for (const result of [
    { stdout: "2", stderr: "failed", code: 2, killed: false },
    { stdout: "2", stderr: "timeout", code: 1, killed: true },
  ]) {
    const runtime = fakeApi(async () => result);
    await runtime.emit("session_start");
    await runtime.emit("before_agent_start");
    await runtime.emit("session_shutdown");
    assert.deepEqual(commandArgs(runtime), [["agent", "register", "pi"]]);
  }

  const runtime = fakeApi(async () => {
    throw new Error("cannot spawn");
  });
  await runtime.emit("session_start");
  await runtime.emit("before_agent_start");
  await runtime.emit("session_shutdown");
  assert.deepEqual(commandArgs(runtime), [["agent", "register", "pi"]]);
}

async function testShutdownHasOneGlobalDeadline() {
  const never = new Promise(() => {});
  const runtime = fakeApi(async () => await never);
  await runtime.emit("session_start");
  await waitFor(() => runtime.calls.length === 1, "pending registration");

  const realSetTimeout = globalThis.setTimeout;
  const realClearTimeout = globalThis.clearTimeout;
  let sawDeadline = false;
  globalThis.setTimeout = (callback, milliseconds) => {
    assert.equal(milliseconds, 12_000);
    sawDeadline = true;
    queueMicrotask(callback);
    return 1;
  };
  globalThis.clearTimeout = () => {};
  try {
    await runtime.emit("session_shutdown");
  } finally {
    globalThis.setTimeout = realSetTimeout;
    globalThis.clearTimeout = realClearTimeout;
  }

  assert.equal(sawDeadline, true);
  assert.deepEqual(commandArgs(runtime), [["agent", "register", "pi"]]);
}

async function testRuntimeReplacementOrdering() {
  const sharedCalls = [];
  const makeRuntime = () =>
    fakeApi(async (_command, args) => {
      sharedCalls.push([...args]);
      return success(args[1] === "register" ? String(sharedCalls.length) : "");
    });
  const oldRuntime = makeRuntime();
  await oldRuntime.emit("session_start");
  await oldRuntime.emit("session_shutdown");
  const newRuntime = makeRuntime();
  await newRuntime.emit("session_start");
  await newRuntime.emit("session_shutdown");

  assert.equal(sharedCalls[1][3], "killed");
  assert.deepEqual(sharedCalls[2], ["agent", "register", "pi"]);
}

const originalConsoleError = console.error;
console.error = () => {};
try {
  await testSubagentChildrenAreIgnored();
  await testLifecycleOrder();
  await testDoneAgentReactivatesForAnotherRun();
  await testPromptOrderingDoesNotWaitForExec();
  await testSettlementWinsOverLatePromptEnd();
  await testPromptClassificationAndShutdownGuard();
  await testExactStringIds();
  await testFailuresAreIsolatedAndWorkingRepairsSettlement();
  await testRegistrationFailureSuppressesStatuses();
  await testShutdownHasOneGlobalDeadline();
  await testRuntimeReplacementOrdering();
  console.log("Pi extension lifecycle tests passed");
} finally {
  console.error = originalConsoleError;
  await rm(temp, { recursive: true, force: true });
}
