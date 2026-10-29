import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

export const metadataPaths = [
  'src-tauri/Cargo.toml',
  'src-tauri/crates/xshelld/Cargo.toml',
  'src-tauri/Cargo.lock',
  'src-tauri/tauri.conf.json',
  'package.json',
  'package-lock.json',
];

// Fixed test-only self versions; never rewrite dependency constraints or regenerate locks.
export function olderFixtureMetadata(files) {
  const output = { ...files };
  function replaceOnce(file, pattern, replacement) {
    assert.equal([...output[file].matchAll(new RegExp(pattern.source, `${pattern.flags}g`))].length, 1, file);
    output[file] = output[file].replace(pattern, replacement);
  }
  for (const [file, name] of [
    ['src-tauri/Cargo.toml', 'xshell'],
    ['src-tauri/crates/xshelld/Cargo.toml', 'xshelld'],
  ]) {
    replaceOnce(file, new RegExp(`(\\[package\\]\\r?\\nname = "${name}"\\r?\\nversion = ")1\\.5\\.0(")`), '$11.4.9$2');
  }
  for (const name of ['xshell', 'xshelld']) {
    replaceOnce('src-tauri/Cargo.lock', new RegExp(`(\\[\\[package\\]\\]\\r?\\nname = "${name}"\\r?\\nversion = ")1\\.5\\.0(")`), '$11.4.9$2');
  }
  for (const file of ['src-tauri/tauri.conf.json', 'package.json', 'package-lock.json']) {
    const before = JSON.parse(files[file]);
    assert.equal(before.version, '1.5.0', file);
    replaceOnce(file, /(^ {2}"version": ")1\.5\.0(")/m, '$11.4.9$2');
    const expected = structuredClone(before);
    expected.version = '1.4.9';
    if (file === 'package-lock.json') {
      assert.equal(before.packages[''].version, '1.5.0');
      replaceOnce(file, /(^ {6}"version": ")1\.5\.0(")/m, '$11.4.9$2');
      expected.packages[''].version = '1.4.9';
    }
    assert.deepEqual(JSON.parse(output[file]), expected, file);
  }
  return output;
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const root = process.argv[2];
  assert.ok(root, 'usage: node windows-older-fixture-metadata.mjs <disposable-worktree>');
  const files = Object.fromEntries(metadataPaths.map(file => [file, fs.readFileSync(path.join(root, file), 'utf8')]));
  // Validate every input before writing any file.
  const output = olderFixtureMetadata(files);
  for (const file of metadataPaths) fs.writeFileSync(path.join(root, file), output[file]);
}
