// Runs test/*.test.mjs with the built-in node:test runner.
// `node --test <dir>` means different things on Node 20 (a directory) and 22+ (a glob), so the
// files are listed explicitly; extra arguments (for example --test-name-pattern) are passed on.
import { spawnSync } from "node:child_process";
import { readdirSync } from "node:fs";
import { fileURLToPath } from "node:url";

const dir = fileURLToPath(new URL("../test/", import.meta.url));
const files = readdirSync(dir)
  .filter((name) => name.endsWith(".test.mjs"))
  .sort()
  .map((name) => dir + name);
if (files.length === 0) {
  console.error("no tests in sdk/web/test");
  process.exit(1);
}
// --test-timeout (a hung test fails after a minute instead of hanging the gate) exists from 20.11.
const [major = 0, minor = 0] = process.versions.node.split(".").map(Number);
const timeout = major > 20 || (major === 20 && minor >= 11) ? ["--test-timeout=60000"] : [];
const result = spawnSync(process.execPath, ["--test", ...timeout, ...process.argv.slice(2), ...files], {
  stdio: "inherit",
});
process.exit(result.status ?? 1);
