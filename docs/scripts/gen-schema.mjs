#!/usr/bin/env node
/// Regenerates docs/src/data/schema.json from the Rust models by running
/// `cargo run --bin docgen`. Called automatically by `npm run build`.
import { execSync } from 'child_process';
import { writeFileSync, mkdirSync } from 'fs';
import { join, dirname } from 'path';
import { fileURLToPath } from 'url';

const __dirname = dirname(fileURLToPath(import.meta.url));
const repoRoot = join(__dirname, '..', '..');
const outPath = join(__dirname, '..', 'src', 'data', 'schema.json');

try {
  console.log('Generating schema.json from Rust models...');
  const json = execSync('cargo run --bin docgen --quiet', {
    cwd: repoRoot,
    encoding: 'utf8',
    stdio: ['pipe', 'pipe', 'inherit'],
  });
  mkdirSync(dirname(outPath), { recursive: true });
  writeFileSync(outPath, json);
  console.log(`Written: ${outPath}`);
} catch (err) {
  console.warn('Warning: cargo run --bin docgen failed. Using existing schema.json if present.');
  console.warn(err.message);
}
