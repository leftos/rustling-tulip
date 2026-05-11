/**
 * Verify the local machine has everything the harness needs.
 *
 * Run with `pnpm doctor`. Exits non-zero on any failure so it can gate CI.
 */
import { spawn } from "node:child_process";
import { existsSync } from "node:fs";
import { join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

type Check = {
  name: string;
  run: () => Promise<string>;
};

const here = fileURLToPath(new URL(".", import.meta.url));
const repoRoot = resolve(here, "..", "..", "..");

const checks: Check[] = [
  {
    name: "tauri-driver on PATH",
    run: () =>
      whichOrErr(
        "tauri-driver",
        "Install: cargo install tauri-driver --locked",
      ),
  },
  {
    name: "msedgedriver on PATH",
    run: () =>
      whichOrErr(
        "msedgedriver",
        "Install msedgedriver-tool, run it once to download the driver, " +
          "then add the binary's directory to PATH.\n" +
          "  cargo install --git https://github.com/chippers/msedgedriver-tool\n" +
          "  & \"$HOME/.cargo/bin/msedgedriver-tool.exe\"",
      ),
  },
  {
    name: "msedgedriver-tool on PATH",
    run: () =>
      whichOrErr(
        "msedgedriver-tool",
        "Install: cargo install --git https://github.com/chippers/msedgedriver-tool",
      ),
  },
  {
    name: "Tauri debug binary exists",
    run: async () => {
      const candidates = [
        join(repoRoot, "target", "debug", "rustling-tulip-app.exe"),
        join(repoRoot, "target", "debug", "rustling-tulip-app"),
      ];
      const hit = candidates.find((p) => existsSync(p));
      if (hit) return hit;
      throw new Error(
        "Not built yet. Run `pnpm host:build` from tools/e2e (or " +
          "`pnpm tauri build --debug --no-bundle` from apps/tauri-app).",
      );
    },
  },
  {
    name: "daemon binary exists",
    run: async () => {
      const candidates = [
        join(repoRoot, "target", "debug", "rustling-tulipd.exe"),
        join(repoRoot, "target", "debug", "rustling-tulipd"),
      ];
      const hit = candidates.find((p) => existsSync(p));
      if (hit) return hit;
      throw new Error(
        "Not built yet. Run `cargo build -p daemon` from the workspace root.",
      );
    },
  },
];

function whichOrErr(name: string, hint: string): Promise<string> {
  return new Promise((resolveFn, rejectFn) => {
    const isWin = process.platform === "win32";
    const tool = isWin ? "where" : "which";
    const child = spawn(tool, [name], { stdio: ["ignore", "pipe", "ignore"] });
    let stdout = "";
    child.stdout?.on("data", (d: Buffer) => {
      stdout += d.toString("utf8");
    });
    child.on("exit", (code) => {
      if (code === 0 && stdout.trim().length > 0) {
        resolveFn(stdout.trim().split(/\r?\n/)[0] ?? name);
      } else {
        rejectFn(new Error(`${name} not on PATH.\n  ${hint}`));
      }
    });
    child.on("error", () => {
      rejectFn(new Error(`could not run ${tool} to locate ${name}`));
    });
  });
}

async function main(): Promise<void> {
  // Output is the whole point of this script. console.log is fine in a script.
  // eslint-disable-next-line no-console
  console.log("e2e doctor — checking prerequisites:\n");
  let failures = 0;
  for (const c of checks) {
    try {
      const info = await c.run();
      // eslint-disable-next-line no-console
      console.log(`  [ok] ${c.name}\n       ${info}`);
    } catch (err) {
      failures += 1;
      // eslint-disable-next-line no-console
      console.log(
        `  [!!] ${c.name}\n       ${err instanceof Error ? err.message : String(err)}`,
      );
    }
  }
  // eslint-disable-next-line no-console
  console.log("");
  if (failures > 0) {
    // eslint-disable-next-line no-console
    console.error(`${failures} check(s) failed.`);
    process.exit(1);
  }
  // eslint-disable-next-line no-console
  console.log("All checks passed. You can `pnpm host` or `pnpm test`.");
}

void main();
