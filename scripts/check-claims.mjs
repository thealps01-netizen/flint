// Fails when a public text (README.md, CHANGELOG.md) claims Flint is
// faster than Everything without linking the measurement in the same
// paragraph: no comparative speed claim without a measurement, and the
// benchmark backs every speed claim.
//
//   node scripts/check-claims.mjs [file ...]   (default: README.md CHANGELOG.md)
//
// Exit 0: no unbacked claim. Exit 1: each offending paragraph is printed.
import { readFileSync, existsSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const files = process.argv.slice(2).length ? process.argv.slice(2) : ["README.md", "CHANGELOG.md"].map((f) => join(root, f));
// \s+: the texts are hard-wrapped, a claim can span lines
const claim =
  /(faster|quicker|speedier)\s+than\s+everything|(outperforms|beats|outpaces)\s+everything|everything['’]?(ten|den)\s+(daha\s+)?(hızlı|çabuk)/i;
const backed = /docs\/benchmarks\/[\w.-]+\.md/;

let bad = 0;
for (const f of files) {
  if (!existsSync(f)) continue;
  const paragraphs = readFileSync(f, "utf8").split(/\r?\n\s*\r?\n/);
  for (const p of paragraphs) {
    if (claim.test(p) && !backed.test(p)) {
      bad++;
      console.log(`${f}: a speed claim against Everything without a link to docs/benchmarks/:\n  ${p.trim().replace(/\s+/g, " ").slice(0, 200)}`);
    }
  }
}
if (bad) process.exit(1);
console.log("check-claims: no unbacked claim");
