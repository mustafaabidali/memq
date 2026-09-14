// OpenCode 1.18.30 plugin API. Copy to .opencode/plugins/memq.ts.
import { execFile } from "node:child_process";
import { promisify } from "node:util";
import { setTimeout as wait } from "node:timers/promises";
const exec = promisify(execFile);

export const MemqPlugin = async ({ directory }: { directory: string }) => {
  let pending: Promise<string> | undefined;
  const loadBrief = async () => {
    const deadline = Date.now() + 30000;
    for (;;) {
      try {
        const result = await exec(process.env.MEMQ_BIN || "memq",
          ["--repo", directory, "brief", "--compact", "--budget", "2000"],
          { timeout: Math.max(1, deadline - Date.now()), maxBuffer: 1024 * 1024 });
        return JSON.stringify(JSON.parse(result.stdout));
      } catch (error) {
        let busy = false;
        try {
          busy = JSON.parse((error as { stdout?: string }).stdout || "{}")
            .error?.code === "in_progress";
        } catch { /* Other errors use the recovery message below. */ }
        if (!busy || Date.now() >= deadline) throw error;
        // MCP startup may still hold the lock. A retry never takes it away.
        await wait(100);
      }
    }
  };
  return {
    // Called for provider requests, including the first request after compaction.
    "experimental.chat.system.transform": async (
      _input: unknown, output: { system: string[] }
    ) => {
      try {
        // Share one read when several model requests start together.
        pending ??= loadBrief().finally(() => { pending = undefined; });
        output.system.push(await pending);
      } catch {
        output.system.push(JSON.stringify({ memq: { operation: "brief" },
          error: { code: "startup_brief_unavailable" },
          recovery: "Run memq brief --compact from the project directory." }));
      }
    },
  };
};
