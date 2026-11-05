import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import plugin from "../plugins/lms-responses-compat/index.js";

const config = JSON.parse(readFileSync(new URL("../opencode.jsonc", import.meta.url)));
const allowed = JSON.parse(readFileSync(new URL("../plugins/lms-responses-compat/models.json", import.meta.url)));

async function loadPlugin(providers = config.providers) {
  const hooks = new Map();
  let transform;
  await plugin.setup({
    model: { transform: async (callback) => { transform = callback; } },
    provider: {
      get: async ({ providerID }) => ({ location: {}, data: providers[providerID] }),
    },
    session: { hook: async (name, callback) => { hooks.set(name, callback); } },
  });
  return { hook: hooks.get("http.request"), transform };
}

function requestEvent(providerID, body, overrides = {}) {
  return {
    sessionID: "session-test", agent: "build", model: { providerID, id: "test" }, kind: "primary",
    request: new Request(`${providers[providerID].settings.baseURL}/${providerID === "palantir-openai" ? "responses" : "messages"}`, {
      method: "POST", headers: { "Content-Type": "application/json", "X-Test": "kept", "Authorization": "Bearer stack-token" }, body: JSON.stringify(body),
    }),
    ...overrides,
  };
}

const providers = Object.fromEntries(Object.entries(config.providers).map(([id, value]) => [id, {
  ...value, settings: { ...value.settings, baseURL: `https://foundry.example/${id}` },
}]));

test("native config preserves proxy model IDs, title model, permissions, and disabled MCP", () => {
  assert.equal(config.model, "palantir-openai/gpt-6-sol");
  assert.equal(config.agents.title.model, "palantir-openai/gpt-6-luna");
  assert.equal(config.providers["palantir-openai"].models["gpt-5.6-sol"].modelID, "ri.language-model-service..language-model.gpt-5-6-sol");
  assert.equal(config.providers["palantir-anthropic"].models["claude-opus-5-5"].modelID, "ri.language-model-service..language-model.anthropic-claude-5-5-opus");
  if (config.mcp?.servers?.palantir) assert.equal(config.mcp.servers.palantir.disabled, true);
  assert.ok(config.permissions.some(rule => rule.action === "external_directory" && rule.effect === "ask"));
  for (const id of Object.keys(providers)) {
    assert.deepEqual(Object.keys(config.providers[id].models), allowed[id]);
    assert.equal(config.providers[id].settings.transport, "http");
    for (const model of Object.values(config.providers[id].models)) assert.ok(model.modelID);
  }
});

test("v2 model transform retains the v1 allowlists without filtering unrelated providers", async () => {
  const { transform } = await loadPlugin(providers);
  const removed = [];
  transform({
    list: (id) => [{ id: allowed[id][0] }, { id: "unsupported-model" }],
    remove: (id, model) => removed.push([id, model]),
  });
  assert.deepEqual(removed, [["palantir-openai", "unsupported-model"], ["palantir-anthropic", "unsupported-model"]]);
});

test("proxy provider IDs cannot select saved vendor OAuth accounts", () => {
  assert.deepEqual(Object.keys(config.providers), ["palantir-openai", "palantir-anthropic"]);
  assert.equal(config.providers["palantir-openai"].canonical, "openai");
  assert.equal(config.providers["palantir-anthropic"].canonical, "anthropic");
  assert.ok(config.policies.some(rule => rule.action === "integration.use" && rule.resource === "plugin:context-mode" && rule.effect === "deny"));
  assert.ok(config.plugins.includes("-context-mode"));
});

test("v2 HTTP hook normalizes every OpenAI request kind and preserves request metadata", async () => {
  const { hook } = await loadPlugin(providers);
  const oldToken = process.env.OPENCODE_API_KEY;
  process.env.OPENCODE_API_KEY = "Bearer synthetic-test-token";
  try {
    for (const kind of ["primary", "compaction", "title", "generate"]) {
      const event = requestEvent("palantir-openai", {
        model: config.providers["palantir-openai"].models["gpt-6-sol"].modelID,
        tools: [{ type: "function", name: "shell" }, { type: "function", name: "strict-tool", strict: true, parameters: { type: "object", properties: { a: { type: "string" } } } }],
        reasoning: { effort: "high", summary: "auto" }, text: { verbosity: "low", format: { type: "text" } },
        reasoningSummary: "auto", textVerbosity: "low", input: [{ role: "user", content: [{ type: "input_text", text: "Hello" }] }],
      }, { kind });
      const signal = event.request.signal;
      await hook(event);
      const body = await event.request.json();
      assert.equal(body.model, config.providers["palantir-openai"].models["gpt-6-sol"].modelID);
      assert.deepEqual(body.reasoning, { effort: "high" });
      assert.deepEqual(body.text, { format: { type: "text" } });
      assert.equal(body.reasoningSummary, undefined);
      assert.equal(body.textVerbosity, undefined);
      assert.equal(body.tools[0].strict, false);
      assert.deepEqual(body.tools[0].parameters, { type: "object", properties: {} });
      assert.equal(body.tools[1].strict, true);
      assert.ok(body.tools[1].parameters.properties.a);
      assert.equal(event.request.headers.get("Authorization"), "Bearer stack-token");
      assert.equal(event.request.headers.get("X-Test"), "kept");
      assert.equal(event.request.signal.aborted, signal.aborted);
    }
  } finally {
    if (oldToken === undefined) delete process.env.OPENCODE_API_KEY;
    else process.env.OPENCODE_API_KEY = oldToken;
  }
});

test("Anthropic request cleanup preserves nested tool schemas, thinking, and PDFs", async () => {
  const { hook } = await loadPlugin(providers);
  const event = requestEvent("palantir-anthropic", {
    model: "claude-sonnet-4-6", thinking: { type: "enabled", budget_tokens: 16000 },
    tools: [{ name: "shell", eager_input_streaming: true, input_schema: { type: "object", properties: { command: { type: "string" } } } }],
    messages: [{ role: "user", content: [{ type: "document", display: "inline", source: { type: "base64", media_type: "application/pdf", data: "test" } }] }],
  });
  await hook(event);
  const body = await event.request.json();
  assert.equal(body.tools[0].eager_input_streaming, undefined);
  assert.equal(body.messages[0].content[0].display, undefined);
  assert.equal(body.messages[0].content[0].source.media_type, "application/pdf");
  assert.ok(body.tools[0].input_schema.properties.command);
  assert.equal(body.thinking.budget_tokens, 16000);
});

test("Anthropic HTTP requests drop proxy-incompatible cache markers for every request kind", async () => {
  const { hook } = await loadPlugin(providers);
  for (const kind of ["primary", "compaction", "title", "generate"]) {
    const event = requestEvent("palantir-anthropic", {
      cache_control: { type: "ephemeral" },
      system: [{ type: "text", text: "instructions", cache_control: { type: "ephemeral" } }],
      tools: [{ name: "lookup", cache_control: { type: "ephemeral" }, input_schema: { type: "object", properties: { cache_control: { type: "string" } } } }],
      messages: [{ role: "assistant", content: [{ type: "tool_use", name: "lookup", id: "call-1", input: { cache_control: "keep" }, cache_control: { type: "ephemeral" } }] }],
    }, { kind });
    const signal = event.request.signal;
    await hook(event);
    const body = await event.request.json();
    assert.equal(body.cache_control, undefined);
    assert.equal(body.system[0].cache_control, undefined);
    assert.equal(body.tools[0].cache_control, undefined);
    assert.equal(body.messages[0].content[0].cache_control, undefined);
    assert.deepEqual(body.tools[0].input_schema.properties.cache_control, { type: "string" });
    assert.equal(body.messages[0].content[0].input.cache_control, "keep");
    assert.equal(event.request.headers.get("Authorization"), "Bearer stack-token");
    assert.equal(event.request.signal.aborted, signal.aborted);
  }
});

test("OpenAI file attachments fail before sending, while images remain supported", async () => {
  const { hook } = await loadPlugin(providers);
  await assert.rejects(hook(requestEvent("palantir-openai", {
    input: [{ role: "user", content: [{ type: "input_file", file_data: "test" }] }],
  })), /does not support OpenAI input_file/);
  const image = requestEvent("palantir-openai", { input: [{ role: "user", content: [{ type: "input_image", image_url: "https://example.com/image.png" }] }] });
  await hook(image);
  assert.equal((await image.request.json()).input[0].content[0].type, "input_image");
});

test("HTTP hook leaves unrelated requests and malformed JSON readable", async () => {
  const { hook } = await loadPlugin(providers);
  for (const url of ["https://elsewhere.example/responses", "https://foundry.example/unrelated", `${providers["palantir-openai"].settings.baseURL}/chat/completions`]) {
    const event = requestEvent("palantir-openai", {}, { request: new Request(url, { method: "POST", body: "unchanged" }) });
    const original = event.request;
    await hook(event);
    assert.equal(event.request, original);
    assert.equal(await event.request.text(), "unchanged");
  }
  const event = requestEvent("palantir-openai", {});
  event.request = new Request(event.request.url, { method: "POST", body: "not-json" });
  await hook(event);
  assert.equal(await event.request.text(), "not-json");
});

test("HTTP hook preserves cancellation and replaces stale content length", async () => {
  const { hook } = await loadPlugin(providers);
  const controller = new AbortController();
  const event = requestEvent("palantir-openai", {});
  event.request = new Request(event.request.url, {
    method: "POST", headers: { "content-length": "999" },
    body: JSON.stringify({ textVerbosity: "low" }), signal: controller.signal,
  });
  await hook(event);
  assert.equal(event.request.headers.get("content-length"), null);
  controller.abort();
  assert.equal(event.request.signal.aborted, true);
});

test("installed Ponytail package registers v2 commands, skills, and persistent mode hooks", async () => {
  const { mkdtempSync, rmSync } = await import("node:fs");
  const { tmpdir } = await import("node:os");
  const { join } = await import("node:path");
  const directory = mkdtempSync(join(tmpdir(), "opencode-ponytail-test-"));
  const originalConfigHome = process.env.XDG_CONFIG_HOME;
  process.env.XDG_CONFIG_HOME = directory;
  try {
    const { default: ponytail } = await import("@dietrichgebert/ponytail");
    const commands = new Map();
    const skills = [];
    const hooks = new Map();
    const prompts = [];
    await ponytail.setup({
      command: { transform: async fn => fn({ add: command => commands.set(command.name, command) }) },
      skill: { transform: async fn => fn({ add: skill => skills.push(skill) }) },
      session: { hook: async (name, callback) => hooks.set(name, callback), prompt: async prompt => prompts.push(prompt) },
    });
    assert.equal(ponytail.id, "ponytail");
    for (const name of ["ponytail", "ponytail-help", "ponytail-audit", "ponytail-debt", "ponytail-review"]) assert.ok(commands.has(name));
    assert.ok(skills.some(skill => skill.id === "ponytail" && skill.content));
    for (const mode of ["off", "ultra"]) {
      await commands.get("ponytail").execute({ sessionID: "test", prompt: { text: mode }, delivery: "queue" });
      assert.equal(readFileSync(join(directory, "opencode/.ponytail-active"), "utf8"), mode);
      for (const name of ["context", "compaction"]) {
        const event = { system: [] };
        hooks.get(name)(event);
        assert.equal(event.system.length, mode === "off" ? 0 : 1);
        if (mode !== "off") assert.ok(event.system[0].text);
      }
    }
    assert.equal(prompts[1].sessionID, "test");
    assert.equal(prompts[1].delivery, "queue");
    assert.ok(prompts[1].text.includes("ultra"));
  } finally {
    if (originalConfigHome === undefined) delete process.env.XDG_CONFIG_HOME;
    else process.env.XDG_CONFIG_HOME = originalConfigHome;
    rmSync(directory, { recursive: true, force: true });
  }
});
