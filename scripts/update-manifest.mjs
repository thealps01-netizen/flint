// Writes the update manifest (latest.json) Flint's updater reads: the
// version, the installer's address and its signature (the .sig file the
// Tauri CLI writes next to the installer when it signs it).
//
//   node scripts/update-manifest.mjs <setup.exe> <version> <download-base-url> [notes-file] > latest.json
//
// The installer must be named Flint_<version>_x64-setup.exe and its
// signature <setup.exe>.sig; the address is <download-base-url>/<file name>.
import { readFileSync, existsSync } from "node:fs";
import { basename } from "node:path";

const [setup, version, base, notesFile] = process.argv.slice(2);
if (!setup || !version || !base) {
  console.error("usage: update-manifest.mjs <setup.exe> <version> <download-base-url> [notes-file]");
  process.exit(2);
}
const name = basename(setup);
if (name !== `Flint_${version}_x64-setup.exe`) {
  console.error(`update-manifest: ${name} is not the installer of version ${version}`);
  process.exit(1);
}
if (!existsSync(`${setup}.sig`)) {
  console.error(`update-manifest: ${setup}.sig is missing (build with the signing key and createUpdaterArtifacts)`);
  process.exit(1);
}
const manifest = {
  version,
  notes: notesFile ? readFileSync(notesFile, "utf8").trim() : "",
  pub_date: new Date().toISOString(),
  platforms: {
    "windows-x86_64": {
      signature: readFileSync(`${setup}.sig`, "utf8").trim(),
      url: `${base.replace(/\/$/, "")}/${name}`,
    },
  },
};
process.stdout.write(JSON.stringify(manifest, null, 2) + "\n");
