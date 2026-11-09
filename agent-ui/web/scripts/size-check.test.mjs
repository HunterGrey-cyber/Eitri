// The bundle size check `npm run build` ends with; `npm test` runs this file with `node --test`.
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { mkdtempSync, mkdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { test } from 'node:test';
import { gzipSync } from 'node:zlib';

const script = fileURLToPath(new URL('./size-check.mjs', import.meta.url));
const LIMIT = 250000;

function check(bytes, args = [String(LIMIT)], file = 'dist/index.html') {
  const cwd = mkdtempSync(join(tmpdir(), 'eitri-size-'));
  try {
    if (bytes !== undefined) {
      mkdirSync(join(cwd, dirname(file)), { recursive: true });
      writeFileSync(join(cwd, file), bytes);
    }
    const run = spawnSync(process.execPath, [script, ...args], { cwd, encoding: 'utf8' });
    return { status: run.status, stdout: run.stdout, stderr: run.stderr };
  } finally {
    rmSync(cwd, { recursive: true, force: true });
  }
}

function noise(length) {
  const bytes = Buffer.alloc(length);
  let state = 0x13579bdf;
  for (let i = 0; i < length; i++) {
    state ^= state << 13;
    state ^= state >>> 17;
    state ^= state << 5;
    bytes[i] = state & 255;
  }
  return bytes;
}

test('compressible HTML is measured after gzip at level nine', () => {
  const bytes = Buffer.from('<html>hello</html>'.repeat(100000));
  const result = check(bytes);
  assert.equal(result.status, 0, result.stderr);
  assert.match(result.stdout, new RegExp(`\\b${gzipSync(bytes, { level: 9 }).length} bytes`));
});

test('an oversized gzip fails and names the measured bytes and the limit', () => {
  const bytes = noise(LIMIT + 10000);
  const result = check(bytes);
  assert.equal(result.status, 1, result.stderr);
  assert.match(result.stderr, new RegExp(`\\b${gzipSync(bytes, { level: 9 }).length} bytes`));
  assert.match(result.stderr, new RegExp(`limit ${LIMIT} bytes`));
});

test('a missing build fails', () => {
  assert.notEqual(check(undefined).status, 0);
});

test('a missing or malformed limit fails before measuring', () => {
  const bytes = Buffer.from('<html></html>');
  for (const args of [[], ['0'], ['-1'], ['12kb'], ['1.5'], ['100', 'dist/index.html', 'extra'], ['100', '']]) {
    const result = check(bytes, args);
    assert.equal(result.status, 2, `limit ${JSON.stringify(args)}: ${result.stderr}`);
    assert.match(result.stderr, /usage/);
  }
});

test('the build ends with the size check and names the limit, so changing it rebuilds', () => {
  const pkg = JSON.parse(readFileSync(new URL('../package.json', import.meta.url)));
  assert.match(pkg.scripts.build, new RegExp(`vite build && node scripts/size-check\\.mjs ${LIMIT}$`));
  assert.match(pkg.scripts.test, /node --test scripts\/size-check\.test\.mjs/);
});

test('exactly the limit passes and one more byte fails', () => {
  const bytes = noise(LIMIT);
  let boundary;
  for (let n = LIMIT - 150; n <= bytes.length; n++) {
    if (gzipSync(bytes.subarray(0, n), { level: 9 }).length === LIMIT) {
      boundary = n;
      break;
    }
  }
  assert.notEqual(boundary, undefined, 'the deterministic fixture reaches the exact limit');
  for (const delta of [-1, 0, 1]) {
    const input = bytes.subarray(0, boundary + delta);
    assert.equal(gzipSync(input, { level: 9 }).length, LIMIT + delta);
    const result = check(input);
    assert.equal(result.status, delta > 0 ? 1 : 0, result.stderr);
  }
});

test('a second argument names the file to measure', () => {
  const bytes = Buffer.from('<html>transcript</html>'.repeat(1000));
  const file = 'dist-transcript/index.html';
  const result = check(bytes, ['200000', file], file);
  assert.equal(result.status, 0, result.stderr);
  assert.match(result.stdout, new RegExp(`^dist-transcript/index\\.html: ${gzipSync(bytes, { level: 9 }).length} bytes`));
  assert.match(result.stdout, /limit 200000 bytes/);
  assert.notEqual(check(bytes, ['200000', 'dist/index.html'], file).status, 0, 'the default file is not there');
});
