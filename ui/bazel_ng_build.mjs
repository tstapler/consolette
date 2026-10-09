// Runs `ng build` inside a Bazel sandbox.
//
// Sandboxed inputs are symlinks into the real output base. The Angular compiler plugin
// realpaths source files while TypeScript keeps the tsconfig-relative sandbox path, so it
// reports `main.ts` as "missing from the TypeScript compilation". `--preserve-symlinks`
// fixes that but breaks pnpm-style node_modules resolution (which needs realpaths).
// Copying sources into a scratch dir makes them real files; node_modules stays a symlink.
import { spawnSync } from "node:child_process";
import { cpSync, mkdtempSync, symlinkSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

const root = process.cwd(); // bin/ui (js_run_binary chdir)
const out = resolve(root, process.argv[2]); // declared output dir
const work = mkdtempSync(join(tmpdir(), "ng-build-"));

for (const entry of [
  "src",
  "public",
  "angular.json",
  "package.json",
  "tsconfig.json",
  "tsconfig.app.json",
]) {
  cpSync(join(root, entry), join(work, entry), { recursive: true, dereference: true });
}
symlinkSync(join(root, "node_modules"), join(work, "node_modules"));

// Served at /dashboard (src/dashboard.rs), so assets must resolve under that base.
const result = spawnSync(
  process.execPath,
  [
    join(work, "node_modules/@angular/cli/bin/ng.js"),
    "build",
    "--base-href",
    "/dashboard/",
    "--output-path",
    out,
  ],
  { cwd: work, stdio: "inherit", env: { ...process.env, NG_CLI_ANALYTICS: "false" } },
);
process.exit(result.status ?? 1);
