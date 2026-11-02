/*
  Generates the CLI reference blocks from the built `sectr` binary:
  `--help` for the top-level command and every subcommand → one plain-text
  file per command under src/generated/cli/. The docs pages import them with
  Vite `?raw` and render through Astro's <Code> — the binary's clap doc
  comments stay the single source of truth, nothing is hand-copied.

  The generated files are COMMITTED (not gitignored): the docs build must not
  need a Rust toolchain (Cloudflare Workers Builds runs `npm ci && npm run
  build`), and the committed files are what makes `check:cli` a real drift
  check — regenerate + `git diff --exit-code`, exactly the dashboard
  `check:api` pattern. A help-text change that forgets to regenerate fails
  CI; a PR touching the CLI shows the regenerated blocks
  in review.

  Requires the sectr binary: builds it via cargo if missing (override with
  SECTR_BIN, e.g. a release binary from the install path).
*/
import {
    mkdirSync,
    existsSync,
    writeFileSync,
    rmSync,
    readdirSync,
} from "node:fs";
import { execFileSync } from "node:child_process";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";
import { exit } from "node:process";

const ROOT = join(dirname(fileURLToPath(import.meta.url)), "..");
const OUT = join(ROOT, "src/generated/cli");

// The documented command surface: [fileStem, argv...]. Keep in sync with
// crates/sectr-cli — check:cli fails on drift the moment help output moves.
const COMMANDS = [
    ["top", []],
    ["auth", ["auth"]],
    ["auth-login", ["auth", "login"]],
    ["auth-whoami", ["auth", "whoami"]],
    ["auth-logout", ["auth", "logout"]],
    ["init", ["init"]],
    ["dev", ["dev"]],
    ["send", ["send"]],
    ["tail", ["tail"]],
    ["deploy", ["deploy"]],
    ["secrets", ["secrets"]],
    ["secrets-set", ["secrets", "set"]],
    ["secrets-list", ["secrets", "list"]],
    ["secrets-rm", ["secrets", "rm"]],
];

execFileSync("cargo", ["build", "-p", "sectr-cli"], {
    cwd: join(ROOT, ".."),
    stdio: "inherit",
});
const bin =
    process.env.SECTR_BIN ?? join(ROOT, "..", "target", "debug", "sectr");
if (!existsSync(bin)) {
    console.error(`sectr binary not found at ${bin}!`);
    exit(1);
}

// Get the command's help text. clap prints help even on a TTY-less stdout
// for `--help` (stdout not a terminal → normal Help output, no pager).
function help(argv) {
    return (
        execFileSync(bin, [...argv, "--help"], { encoding: "utf8" }).trimEnd() +
        "\n"
    );
}

rmSync(OUT, { recursive: true, force: true });
mkdirSync(OUT, { recursive: true });
for (const [stem, argv] of COMMANDS) {
    writeFileSync(join(OUT, `${stem}.txt`), help(argv));
    console.log(`wrote src/generated/cli/${stem}.txt`);
}
