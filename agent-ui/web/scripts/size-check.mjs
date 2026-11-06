// Fails the build when the single-file panel bundle grows past the limit given as the one argument (bytes of
// `dist/index.html` after gzip at level 9). The limit lives in `package.json`'s build line, which is a build input,
// so changing it rebuilds and re-checks the bundle.
import { readFileSync } from 'node:fs';
import { gzipSync } from 'node:zlib';

const args = process.argv.slice(2);
const limit = args.length === 1 && /^[1-9][0-9]*$/.test(args[0]) ? Number(args[0]) : NaN;
if (!Number.isSafeInteger(limit)) {
  console.error('usage: node scripts/size-check.mjs <limit in gzip bytes>');
  process.exit(2);
}
const size = gzipSync(readFileSync('dist/index.html'), { level: 9 }).length;
const message = `dist/index.html: ${size} bytes gzip -9 (limit ${limit} bytes)`;
if (size > limit) {
  console.error(message);
  process.exitCode = 1;
} else {
  console.log(message);
}
