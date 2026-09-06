/**
 * Verify the local machine has everything the harness needs.
 *
 * Run with `pnpm run doctor` — bare `pnpm doctor` hits pnpm's own builtin
 * doctor command and never reaches this script. Exits non-zero on any failure
 * so it can gate CI.
 */
import { spawn } from "node:child_process";
import { existsSync } from "node:fs";
import { join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

type Check = {
  name: string;
  run: () => Promise<string>;
};

/** Thrown by a check that does not apply to this machine. Not a failure. */
class Skip extends Error {
  constructor(reason: string) {
    super(reason);
    this.name = "Skip";
  }
}

const here = fileURLToPath(new URL(".", import.meta.url));
const repoRoot = resolve(here, "..", "..", "..");

/** EdgeUpdate client id of the WebView2 Evergreen runtime the Tauri window renders in. */
const WEBVIEW2_RUNTIME_GUID = "{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}";
/** EdgeUpdate client id of Edge stable, which ships the same major as the runtime. */
const EDGE_STABLE_GUID = "{56EB18F8-B008-4CBD-B6D2-8C97FE7E9062}";

const BROWSER_VERSION_KEYS = [
  `HKLM\\SOFTWARE\\WOW6432Node\\Microsoft\\EdgeUpdate\\Clients\\${WEBVIEW2_RUNTIME_GUID}`,
  `HKCU\\SOFTWARE\\Microsoft\\EdgeUpdate\\Clients\\${WEBVIEW2_RUNTIME_GUID}`,
  `HKLM\\SOFTWARE\\WOW6432Node\\Microsoft\\EdgeUpdate\\Clients\\${EDGE_STABLE_GUID}`,
  `HKCU\\SOFTWARE\\Microsoft\\EdgeUpdate\\Clients\\${EDGE_STABLE_GUID}`,
];

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
        "Install msedgedriver-tool, then run it from a scratch directory " +
          "(it extracts msedgedriver.exe into its cwd) and move the driver " +
          "onto PATH.\n" +
          "  cargo install --git https://github.com/chippers/msedgedriver-tool\n" +
          "  cd $env:TEMP; & \"$HOME/.cargo/bin/msedgedriver-tool.exe\"; " +
          "Move-Item -Force msedgedriver.exe \"$HOME/.cargo/bin/msedgedriver.exe\"",
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
    name: "msedgedriver matches the installed WebView2",
    run: async () => {
      if (process.platform !== "win32") {
        throw new Skip(
          "the harness drives WebView2 through tauri-driver, which is Windows-only today.",
        );
      }
      const driver = await readDriverVersion();
      const browser = await readWebView2Version();
      if (majorOf(driver) !== majorOf(browser.version)) {
        throw new Error(mismatchMessage(driver, browser.version));
      }
      return `driver ${driver} / WebView2 ${browser.version}\n       ${browser.key}`;
    },
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

function runCommand(
  command: string,
  args: string[],
): Promise<{ code: number | null; stdout: string }> {
  return new Promise((resolveFn, rejectFn) => {
    const child = spawn(command, args, { stdio: ["ignore", "pipe", "ignore"] });
    let stdout = "";
    child.stdout?.on("data", (d: Buffer) => {
      stdout += d.toString("utf8");
    });
    child.on("close", (code) => {
      resolveFn({ code, stdout });
    });
    child.on("error", (err: Error) => {
      rejectFn(new Error(`could not run ${command}: ${err.message}`));
    });
  });
}

/** First dotted quad in `text`, e.g. `152.0.4191.66`. */
function parseVersion(text: string): string | null {
  return /\d+\.\d+\.\d+\.\d+/.exec(text)?.[0] ?? null;
}

function majorOf(version: string): string {
  return version.split(".")[0] ?? version;
}

async function readDriverVersion(): Promise<string> {
  const { code, stdout } = await runCommand("msedgedriver", ["--version"]);
  const version = parseVersion(stdout);
  if (version === null) {
    throw new Error(
      "could not read a version from `msedgedriver --version` " +
        `(exit ${String(code)}): ${stdout.trim() || "(no output)"}`,
    );
  }
  return version;
}

async function readWebView2Version(): Promise<{
  version: string;
  key: string;
}> {
  for (const key of BROWSER_VERSION_KEYS) {
    const { code, stdout } = await runCommand("reg", ["query", key, "/v", "pv"]);
    if (code !== 0) continue;
    const pv = /^\s*pv\s+REG_SZ\s+(.+)$/im.exec(stdout)?.[1] ?? "";
    const version = parseVersion(pv);
    if (version !== null) return { version, key };
  }
  throw new Error(
    "no `pv` value under any EdgeUpdate client key, so the driver cannot be " +
      "checked against the browser. Install the WebView2 Evergreen runtime. " +
      "Keys tried:\n" +
      BROWSER_VERSION_KEYS.map((k) => `         ${k}`).join("\n"),
  );
}

function mismatchMessage(driver: string, browser: string): string {
  return (
    `Driver ${majorOf(driver)} does not match WebView2 ${majorOf(browser)}. ` +
    "Refresh the driver from a scratch directory\n" +
    "(msedgedriver-tool extracts msedgedriver.exe into its cwd), then move it onto PATH:\n" +
    '  cd $env:TEMP; & "$HOME/.cargo/bin/msedgedriver-tool.exe"; ' +
    'Move-Item -Force msedgedriver.exe "$HOME/.cargo/bin/msedgedriver.exe"\n' +
    `  (driver ${driver}, WebView2 ${browser})`
  );
}

async function main(): Promise<void> {
  // Output is the whole point of this script. console.log is fine in a script.
  // eslint-disable-next-line no-console
  console.log("e2e doctor — checking prerequisites:\n");
  let passed = 0;
  let skipped = 0;
  let failures = 0;
  for (const c of checks) {
    try {
      const info = await c.run();
      passed += 1;
      // eslint-disable-next-line no-console
      console.log(`  [ok] ${c.name}\n       ${info}`);
    } catch (err) {
      if (err instanceof Skip) {
        skipped += 1;
        // eslint-disable-next-line no-console
        console.log(`  [skip] ${c.name}\n       ${err.message}`);
        continue;
      }
      failures += 1;
      // eslint-disable-next-line no-console
      console.log(
        `  [!!] ${c.name}\n       ${err instanceof Error ? err.message : String(err)}`,
      );
    }
  }
  const summary = `${passed} ok, ${skipped} skipped, ${failures} failed.`;
  // eslint-disable-next-line no-console
  console.log("");
  if (failures > 0) {
    // eslint-disable-next-line no-console
    console.error(summary);
    process.exit(1);
  }
  // eslint-disable-next-line no-console
  console.log(`${summary} You can \`pnpm host\` or \`pnpm test\`.`);
}

void main();
