import fs from 'node:fs';
import path from 'node:path';
import { execFileSync } from 'node:child_process';

// Package notices come from the exact downloaded sources used by this build.
const parts = ['# Third-party notices', 'Generated from the installed dependency sources. Agent Grid is MIT licensed.'];
function include(name, version, license, directory) {
  parts.push(`## ${name} ${version}\n\nLicense: ${license || 'See package source'}`);
  const files = fs.readdirSync(directory).filter(name => /^(licen[cs]e|copying|notice)([.-]|$)/i.test(name));
  for (const file of files) {
    const absolute = path.join(directory, file);
    if (fs.statSync(absolute).isFile()) parts.push(`### ${file}\n\n\`\`\`text\n${fs.readFileSync(absolute, 'utf8')}\n\`\`\``);
  }
}
const lock = JSON.parse(fs.readFileSync('package-lock.json', 'utf8'));
for (const [location] of Object.entries(lock.packages)) {
  if (!location) continue;
  const manifest = path.join(location, 'package.json');
  if (!fs.existsSync(manifest)) continue; // Optional dependencies for another OS.
  const pkg = JSON.parse(fs.readFileSync(manifest, 'utf8'));
  include(pkg.name, pkg.version, pkg.license, location);
}
const metadata = JSON.parse(execFileSync('cargo', ['metadata', '--locked', '--format-version', '1', '--manifest-path', 'src-tauri/Cargo.toml'], { encoding: 'utf8', maxBuffer: 32 * 1024 * 1024 }));
for (const pkg of metadata.packages) {
  if (!pkg.source) continue;
  include(pkg.name, pkg.version, pkg.license, path.dirname(pkg.manifest_path));
}
fs.writeFileSync('THIRD_PARTY_NOTICES.md', parts.join('\n\n') + '\n', 'utf8');
