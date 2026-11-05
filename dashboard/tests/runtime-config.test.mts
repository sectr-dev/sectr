import assert from "node:assert/strict";
import { test } from "node:test";
import vm from "node:vm";

import {
  cliLoginCommand,
  renderRuntimeConfig,
} from "../src/lib/runtime-config.ts";

/** Run the script as a browser would, and return what it set. */
function evaluate(script: string): unknown {
  const window: { __SECTR_CONFIG__?: unknown } = {};
  vm.runInNewContext(script, { window });
  // Objects from another vm context fail deepEqual on their prototype.
  return JSON.parse(JSON.stringify(window.__SECTR_CONFIG__));
}

test("no value gives an empty config", () => {
  assert.deepEqual(evaluate(renderRuntimeConfig(undefined)), {});
  assert.deepEqual(evaluate(renderRuntimeConfig("  ")), {});
});

test("a URL comes through without its trailing slash", () => {
  assert.deepEqual(evaluate(renderRuntimeConfig("https://sectr.example.com/")), {
    publicApiUrl: "https://sectr.example.com",
  });
});

test("no value can break out of the script", () => {
  for (const hostile of [
    '"; globalThis.pwned = true; "',
    "</script><script>globalThis.pwned = true</script>",
    "a\u2028globalThis.pwned = true\u2029b",
    "\\",
  ]) {
    const script = renderRuntimeConfig(hostile);
    assert.ok(!script.includes("</script"), script);
    const window: { __SECTR_CONFIG__?: { publicApiUrl: string } } = {};
    const sandbox: { window: typeof window; pwned?: boolean } = { window };
    vm.runInNewContext(script, sandbox);
    assert.equal(sandbox.pwned, undefined, script);
    assert.equal(window.__SECTR_CONFIG__?.publicApiUrl, hostile.trim());
  }
});

test("the login hint names the server only when it's configured", () => {
  const original = globalThis.window;
  try {
    assert.equal(cliLoginCommand(), "sectr auth login");
    (globalThis as { window?: unknown }).window = { __SECTR_CONFIG__: {} };
    assert.equal(cliLoginCommand("--with-key"), "sectr auth login --with-key");
    (globalThis as { window?: unknown }).window = {
      __SECTR_CONFIG__: { publicApiUrl: "http://localhost:8080" },
    };
    assert.equal(
      cliLoginCommand(),
      "sectr auth login --api http://localhost:8080",
    );
    assert.equal(
      cliLoginCommand("--with-key"),
      "sectr auth login --api http://localhost:8080 --with-key",
    );
  } finally {
    (globalThis as { window?: unknown }).window = original;
  }
});
