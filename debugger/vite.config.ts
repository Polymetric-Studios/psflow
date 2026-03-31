import { defineConfig } from "vite";
import wasm from "vite-plugin-wasm";
import { execFileSync } from "child_process";
import { writeFileSync, readFileSync, mkdtempSync, unlinkSync } from "fs";
import { join, dirname } from "path";
import { fileURLToPath } from "url";
import { tmpdir } from "os";
import type { Plugin } from "vite";

const __dirname = dirname(fileURLToPath(import.meta.url));

function psflowRunner(): Plugin {
  return {
    name: "psflow-runner",
    configureServer(server) {
      server.middlewares.use("/api/run", (req, res) => {
        if (req.method !== "POST") {
          res.writeHead(405);
          res.end("Method not allowed");
          return;
        }

        let body = "";
        req.on("data", (chunk: Buffer) => { body += chunk.toString(); });
        req.on("end", () => {
          const tmp = mkdtempSync(join(tmpdir(), "psflow-"));
          const mmdPath = join(tmp, "graph.mmd");
          const tracePath = join(tmp, "trace.json");

          try {
            writeFileSync(mmdPath, body);

            // Find the psflow binary
            const bin = join(__dirname, "..", "target", "debug", "psflow");

            const stderr = execFileSync(bin, [mmdPath, "--trace", tracePath], {
              encoding: "utf-8",
              timeout: 30000,
              stdio: ["pipe", "pipe", "pipe"],
            });

            const trace = readFileSync(tracePath, "utf-8");

            res.writeHead(200, { "Content-Type": "application/json" });
            res.end(JSON.stringify({ trace: JSON.parse(trace), log: stderr }));
          } catch (err: any) {
            res.writeHead(200, { "Content-Type": "application/json" });
            res.end(JSON.stringify({
              error: err.stderr?.toString() || err.message,
              log: err.stderr?.toString() || "",
            }));
          } finally {
            try { unlinkSync(mmdPath); } catch {}
            try { unlinkSync(tracePath); } catch {}
          }
        });
      });
    },
  };
}

export default defineConfig({
  plugins: [wasm(), psflowRunner()],
  build: {
    target: "esnext",
  },
});
