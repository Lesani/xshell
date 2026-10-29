import assert from 'node:assert/strict';
import fs from 'node:fs';
import { test } from 'node:test';
import { metadataPaths, olderFixtureMetadata } from './windows-older-fixture-metadata.mjs';

const files = Object.fromEntries(metadataPaths.map(file => [file, fs.readFileSync(file, 'utf8')]));
test('older fixture changes exactly eight self-version lines, retaining dependency metadata', () => {
  const output = olderFixtureMetadata(files);
  let changes = 0;
  for (const file of metadataPaths) {
    const before = files[file].split('\n');
    const after = output[file].split('\n');
    assert.equal(before.length, after.length);
    for (let i = 0; i < before.length; i++) {
      if (before[i] === after[i]) continue;
      changes++;
      assert.equal(after[i], before[i].replace('1.5.0', '1.4.9'), `${file}:${i + 1}`);
    }
  }
  assert.equal(changes, 8);
});
test('older fixture refuses unexpected versions and cannot transform twice', () => {
  for (const file of metadataPaths) {
    assert.throws(() => olderFixtureMetadata({ ...files, [file]: files[file].replaceAll('1.5.0', '1.6.0') }));
  }
  assert.throws(() => olderFixtureMetadata(olderFixtureMetadata(files)));
});
test('older fixture handles Windows CRLF without unrelated changes', () => {
  const lf = Object.fromEntries(metadataPaths.map(file => [file, files[file].replaceAll('\r\n', '\n')]));
  const crlf = Object.fromEntries(metadataPaths.map(file => [file, lf[file].replaceAll('\n', '\r\n')]));
  const output = olderFixtureMetadata(crlf);
  const lfOutput = olderFixtureMetadata(lf);
  for (const file of metadataPaths) assert.equal(output[file].replaceAll('\r\n', '\n'), lfOutput[file]);
});
