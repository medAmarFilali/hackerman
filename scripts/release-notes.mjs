// Prints the notes for a release: how to install it, and its part of the changelog.
//   node scripts/release-notes.mjs 1.2.3
import { readFileSync } from "node:fs";

const version = process.argv[2];
if (!version) {
  console.error("Which version?");
  process.exit(1);
}
const changelog = readFileSync(new URL("../CHANGELOG.md", import.meta.url), "utf8").replaceAll("\r\n", "\n");
const start = changelog.indexOf(`\n## ${version}\n`);
if (start < 0) {
  console.error(`CHANGELOG.md has nothing for ${version}`);
  process.exit(1);
}
const rest = changelog.slice(start + 1);
const next = rest.indexOf("\n## ", 1);
const section = (next < 0 ? rest : rest.slice(0, next)).split("\n").slice(1).join("\n").trim();

console.log(`## Install

For Windows 10 and 11, on two or more computers on the same network.

- **\`Hackerman_${version}_x64-setup.exe\`** installs it, and is what most people want.
- **\`Hackerman_${version}_x64-portable.zip\`** needs no installing. Unpack it and run \`hackerman.exe\`.

These builds aren't code-signed yet, so Windows may say "Windows protected your PC". Choose **More
info**, then **Run anyway**. The [README](https://github.com/medAmarFilali/hackerman#checking-a-download)
says how to check a download first.

## What's new

${section}`);
