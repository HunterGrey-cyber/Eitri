// Fails the build when a single-file bundle grows past the limit given as the first argument (bytes after gzip at
// level 9). The second argument names the file, `dist/index.html` (the panel) when left out. The limit lives in
// `package.json`'s build line, which is a build input, so changing it rebuilds and re-checks the bundle.
import { readFileSync } from 'node:fs';
import { gzipSync } from 'node:zlib';

const args = process.argv.slice(2);
const limit = (args.length === 1 || args.length === 2) && /^[1-9][0-9]*$/.test(args[0]) ? Number(args[0]) : NaN;
const file = args.length === 2 ? args[1] : 'dist/index.html';
if (!Number.isSafeInteger(limit) || file === '') {
  console.error('usage: node scripts/size-check.mjs <limit in gzip bytes> [file, default dist/index.html]');
  process.exit(2);
}
const size = gzipSync(readFileSync(file), { level: 9 }).length;
const message = `${file}: ${size} bytes gzip -9 (limit ${limit} bytes)`;
if (size > limit) {
  console.error(message);
  process.exitCode = 1;
} else {
  console.log(message);
}
