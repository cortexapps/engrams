import assert from "node:assert/strict";
import { mkdtempSync, readFileSync, writeFileSync, rmSync } from "node:fs";
import { resolve } from "node:path";
import { test } from "node:test";
import { openEffects } from "./durable.mts";

const base = resolve("var/mcp-stack-fixture");
test("every write is committed, including duplicate IDs, and survives reopen", () => {
  const root = mkdtempSync(`${base}/durable-`);
  try {
    const effects = openEffects(root);
    assert.equal(effects.commit("same", 2), 2);
    assert.equal(effects.commit("same", 2), 4);
    const reopened = openEffects(root);
    assert.equal(reopened.snapshot().executions, 2);
    assert.equal(reopened.commit("same", 2), 6);
    assert.equal(openEffects(root).snapshot().executions, 3);
    const text = readFileSync(`${root}/effects.json`, "utf8");
    assert.equal(text.includes('"same"'), false);
    assert.equal(text.includes('"delta"'), false);
    assert.equal(openEffects(root).snapshot().events.length, 3);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("truncated, corrupt, and incomplete durable state fails closed", () => {
  const root = mkdtempSync(`${base}/corrupt-`);
  try {
    const state = openEffects(root);
    state.commit("call", 1);
    const valid = readFileSync(`${root}/effects.json`, "utf8");
    for (const bad of [
      "",
      valid.slice(0, -2),
      valid.replace('"executions":1', '"executions":0'),
    ]) {
      writeFileSync(`${root}/effects.json`, bad);
      assert.throws(() => openEffects(root), /Invalid durable state/);
    }
    writeFileSync(`${root}/effects.json`, valid);
    writeFileSync(`${root}/effects.next`, "torn");
    assert.throws(() => openEffects(root), /Incomplete durable state/);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});
