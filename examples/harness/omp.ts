// OMP 18.1.18 extension API. Copy to the project's .omp/extensions/memq.ts.
// MEMQ_BIN may select a binary outside PATH; no developer paths are embedded.
import { execFile } from "node:child_process";
import { promisify } from "node:util";
const exec = promisify(execFile);

type BriefMessage = { message: { customType: string; content: string; display: boolean } };
type ExtensionApi = {
  on(event: "before_agent_start",
    listener: (event: unknown, context: { cwd: string }) => Promise<BriefMessage>): void;
};

export default function memq(api: ExtensionApi) {
  // Runs before every user turn, including the first turn after compaction.
  api.on("before_agent_start", async (_event: unknown, ctx: { cwd: string }) => {
    let content: string;
    try {
      const result = await exec(process.env.MEMQ_BIN || "memq",
        ["--repo", ctx.cwd, "brief", "--compact", "--budget", "2000"],
        { timeout: 30000, maxBuffer: 1024 * 1024 });
      const envelope = JSON.parse(result.stdout);
      content = JSON.stringify(envelope);
    } catch {
      content = JSON.stringify({ memq: { operation: "brief" },
        error: { code: "startup_brief_unavailable" },
        recovery: "Run memq brief --compact from the project directory." });
    }
    return { message: { customType: "memq-brief", content, display: true } };
  });
}
