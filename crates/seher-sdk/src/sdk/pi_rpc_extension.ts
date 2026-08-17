import { createConnection } from "node:net";
import { readFileSync } from "node:fs";
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

type ToolSpec = { name: string; description: string; parameters: unknown };
type BridgeResponse = { ok: boolean; result?: string; error?: string };
type BridgeConfig = { specPath?: string; host?: string; port: number; token?: string };

// Pi may re-evaluate this module when a session switch changes its extension cache.
// The global slot is initialized before the environment namespace is scrubbed, so
// re-evaluation keeps the original bridge while descendants never inherit secrets.
const bridgeConfigKey = Symbol.for("seher.pi.bridge.config");
const bridgeGlobal = globalThis as typeof globalThis & { [key: symbol]: BridgeConfig | undefined };
const bridgeConfig = bridgeGlobal[bridgeConfigKey] ?? (process.env.SEHER_PI_TOOL_SPEC ? {
  specPath: process.env.SEHER_PI_TOOL_SPEC,
  host: process.env.SEHER_PI_BRIDGE_HOST,
  port: Number(process.env.SEHER_PI_BRIDGE_PORT),
  token: process.env.SEHER_PI_BRIDGE_TOKEN,
} : undefined);
if (bridgeConfig) bridgeGlobal[bridgeConfigKey] = bridgeConfig;

const { specPath, host, port, token } = bridgeConfig ?? { port: Number.NaN };
for (const key of Object.keys(process.env)) {
  if (key.startsWith("SEHER_PI_")) delete process.env[key];
}

function request(tool: string, input: unknown): Promise<BridgeResponse> {
  const { promise, resolve, reject } = Promise.withResolvers<BridgeResponse>();
  if (!host || !port || !token) {
    reject(new Error("Seher tool bridge is not configured"));
    return promise;
  }
  const socket = createConnection({ host, port });
  let buffer = "";
  let settled = false;
  const fail = (error: unknown) => {
    if (settled) return;
    settled = true;
    reject(error instanceof Error ? error : new Error(String(error)));
    socket.destroy();
  };
  socket.setEncoding("utf8");
  socket.on("connect", () => {
    try {
      socket.write(JSON.stringify({ token, tool, input }) + "\n", (error) => {
        if (error) fail(error);
      });
    } catch (error) {
      fail(error);
    }
  });
  socket.on("data", (chunk: string) => {
    buffer += chunk;
    if (buffer.length > 1024 * 1024) {
      fail(new Error("Seher tool bridge response is too large"));
      return;
    }
    const newline = buffer.indexOf("\n");
    if (newline < 0 || settled) return;
    const line = buffer.slice(0, newline).replace(/\r$/, "");
    try {
      const response = JSON.parse(line) as BridgeResponse;
      if (typeof response.ok !== "boolean") throw new Error("invalid Seher tool bridge response");
      settled = true;
      resolve(response);
      socket.destroy();
    } catch (error) {
      fail(error);
    }
  });
  socket.on("error", fail);
  socket.on("end", () => fail(new Error("Seher tool bridge closed without a response")));
  return promise;
}

export default function (pi: ExtensionAPI) {
  if (!specPath) return;
  const specs = JSON.parse(readFileSync(specPath, "utf8")) as { tools: ToolSpec[] };
  for (const spec of specs.tools) {
    pi.registerTool({
      name: spec.name,
      label: spec.name,
      description: spec.description,
      parameters: spec.parameters,
      async execute(_toolCallId, params) {
        const response = await request(spec.name, params);
        if (!response.ok) throw new Error(response.error ?? "Seher tool failed");
        return { content: [{ type: "text", text: response.result ?? "" }], details: {} };
      },
    });
  }
}
