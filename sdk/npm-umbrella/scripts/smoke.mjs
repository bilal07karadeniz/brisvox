// Checks of the `brisvox` umbrella package (it has no code of its own, so "the tests" are these).
//
//   node scripts/smoke.mjs
//       Static checks, no install: the dependency pins equal the package's own version, every
//       file the `exports` map and `files` name exists, and each entry re-exports exactly the
//       package it claims to.
//
//   node scripts/smoke.mjs --install <brisvox.tgz> <agent.tgz> <web.tgz>
//       Also installs the three packed tarballs into a fresh project (the registry is only
//       consulted for the `ws` dependency of the agent SDK) and imports `brisvox` and
//       `brisvox/web` the way a user does: the names must be the very objects of the scoped
//       packages, and the type declarations must resolve.
import { spawnSync } from "node:child_process";
import { existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = fileURLToPath(new URL("../", import.meta.url));
const pkg = JSON.parse(readFileSync(join(root, "package.json"), "utf8"));
const problems = [];
const check = (ok, message) => {
  if (!ok) problems.push(message);
};

// 1. Versions: the umbrella and the two packages behind it are one release.
check(pkg.name === "brisvox", `name is ${pkg.name}, expected brisvox`);
for (const dep of ["@brisvox/agent", "@brisvox/web"]) {
  check(
    pkg.dependencies?.[dep] === pkg.version,
    `${dep} must be pinned to exactly ${pkg.version}, found ${pkg.dependencies?.[dep]}`,
  );
}
check(Object.keys(pkg.dependencies ?? {}).length === 2, "dependencies are exactly the two scoped packages");

// 2. Every file the package promises is there, and `files` ships them.
const exported = [];
for (const entry of Object.values(pkg.exports)) {
  if (typeof entry === "string") exported.push(entry);
  else exported.push(...Object.values(entry));
}
for (const rel of exported) {
  check(existsSync(join(root, rel)), `exports names ${rel}, which does not exist`);
  const bare = rel.replace(/^\.\//, "");
  check(bare === "package.json" || pkg.files.includes(bare), `${bare} is exported but not in "files"`);
}
for (const rel of pkg.files) check(existsSync(join(root, rel)), `files names ${rel}, which does not exist`);

// 3. Each entry is a plain re-export of the package it stands for.
const reexports = { "index.js": "@brisvox/agent", "index.d.ts": "@brisvox/agent", "web.js": "@brisvox/web", "web.d.ts": "@brisvox/web" };
for (const [file, target] of Object.entries(reexports)) {
  const code = readFileSync(join(root, file), "utf8")
    .split("\n")
    .filter((line) => line.trim() !== "" && !line.trim().startsWith("//"))
    .join("\n");
  check(code === `export * from "${target}";`, `${file} must be exactly: export * from "${target}";`);
}

if (problems.length === 0) console.log("brisvox umbrella: static checks ok");

const at = process.argv.indexOf("--install");
if (at !== -1 && problems.length === 0) {
  const tarballs = process.argv.slice(at + 1, at + 4).map((p) => resolve(p));
  check(tarballs.length === 3 && tarballs.every(existsSync), "--install needs three existing tarballs: brisvox agent web");
  if (problems.length === 0) {
    const dir = mkdtempSync(join(tmpdir(), "brisvox-umbrella-"));
    try {
      const run = (cmd, args, opts = {}) => {
        const r = spawnSync(cmd, args, { cwd: dir, encoding: "utf8", shell: process.platform === "win32", ...opts });
        if (r.status !== 0) throw new Error(`${cmd} ${args.join(" ")} failed:\n${r.stdout}\n${r.stderr}`);
        return r.stdout;
      };
      writeFileSync(join(dir, "package.json"), JSON.stringify({ name: "umbrella-smoke", private: true, type: "module" }));
      run("npm", ["install", "--no-audit", "--no-fund", ...tarballs.map((t) => (process.platform === "win32" ? `"${t}"` : t))]);
      // The scoped packages must be installed once, as the umbrella's own dependencies.
      const tree = JSON.parse(run("npm", ["ls", "--all", "--json", "--depth=1"]));
      check(tree.dependencies?.brisvox?.version === pkg.version, "brisvox is not installed at its own version");
      const script = `
        import * as umbrella from "brisvox";
        import * as agent from "@brisvox/agent";
        import * as web from "brisvox/web";
        import * as scopedWeb from "@brisvox/web";
        const same = (a, b) => Object.keys(b).length > 0 && Object.keys(b).every((k) => a[k] === b[k]);
        if (!same(umbrella, agent)) throw new Error("brisvox does not re-export @brisvox/agent");
        if (!same(web, scopedWeb)) throw new Error("brisvox/web does not re-export @brisvox/web");
        if (typeof umbrella.Agent !== "function") throw new Error("brisvox.Agent missing");
        if (typeof web.connect !== "function") throw new Error("brisvox/web connect missing");
        console.log("imports ok:", Object.keys(umbrella).length, "agent names,", Object.keys(web).length, "web names");
      `;
      writeFileSync(join(dir, "check.mjs"), script);
      process.stdout.write(run(process.execPath, ["check.mjs"], { shell: false }));
      // Types: both entries resolve and keep their names (skipped when no compiler is given).
      const tsc = process.env.BRISVOX_TSC;
      if (tsc && existsSync(tsc)) {
        writeFileSync(
          join(dir, "types.ts"),
          `import { Agent, s } from "brisvox";\nimport { connect, type BrisvoxCallOptions } from "brisvox/web";\n` +
            `const a: Agent = new Agent({ name: "x" });\nconst o: BrisvoxCallOptions = { serverUrl: "https://x" };\nvoid [a, s, connect, o];\n`,
        );
        run(process.execPath, [tsc, "--noEmit", "--strict", "--module", "nodenext", "--moduleResolution", "nodenext",
          "--target", "es2022", "--lib", "es2022,dom", "--skipLibCheck", "types.ts"], { shell: false });
        console.log("types ok");
      }
    } catch (error) {
      problems.push(String(error));
    } finally {
      rmSync(dir, { recursive: true, force: true });
    }
  }
}

if (problems.length > 0) {
  console.error("brisvox umbrella: FAILED");
  for (const p of problems) console.error(`  - ${p}`);
  process.exit(1);
}
