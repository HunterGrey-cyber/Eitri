// The checks `npm run build:transcript` ends with; `npm test` runs this file with `node --test`.
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { readFileSync } from 'node:fs';
import { test } from 'node:test';
import {
  DESK_POSTS,
  KEYMAP_CODE,
  KEYMAP_MARKERS,
  checkBundle,
  inlineScripts,
  scriptHash,
  transcriptPolicy,
  withPolicy,
} from './transcript-bundle-check.mjs';

const body = 'console.log("transcript")';
const hash = `sha256-${createHash('sha256').update(body).digest('base64')}`;
const page = (script = body, policy = transcriptPolicy(hash)) =>
  `<!doctype html><html><head><meta http-equiv="Content-Security-Policy" content="${policy}">` +
  `<meta charset="UTF-8"><script type="module" crossorigin>${script}</script></head><body></body></html>`;

test('a built page with the pinned hash passes', () => {
  assert.deepEqual(checkBundle(page(), hash), []);
});

test('the policy pins the script by hash and reaches nothing', () => {
  const policy = transcriptPolicy(hash);
  assert.match(policy, /^default-src 'none'; script-src 'sha256-[A-Za-z0-9+/=]+'; style-src 'unsafe-inline'; /);
  assert.match(policy, /; img-src data:; connect-src 'none'; frame-ancestors 'none'/);
  assert.equal(policy.split("'unsafe-inline'").length, 2, 'only style-src holds it');
});

test('a keymap marker in the built page fails', () => {
  assert.ok(KEYMAP_MARKERS.length >= 2);
  for (const marker of KEYMAP_MARKERS) {
    const script = `${body};const help=${JSON.stringify(marker)}`;
    const own = `sha256-${createHash('sha256').update(script).digest('base64')}`;
    const problems = checkBundle(page(script, transcriptPolicy(own)), own);
    assert.equal(problems.length, 1, marker);
    assert.match(problems[0], /keyboard/);
  }
});

test('keymap code in the built page fails', () => {
  assert.ok(KEYMAP_CODE.length >= 2);
  for (const name of KEYMAP_CODE) {
    const code = `((t=e.getModifierState)==null?void 0:t.call(e,"${name}"))===!0`;
    const script = `${body};const f=(e)=>{var t;return ${code}}`;
    const own = scriptHash(script);
    const problems = checkBundle(page(script, transcriptPolicy(own)), own);
    assert.equal(problems.length, 1, code);
    assert.match(problems[0], /keyboard/);
  }
});

test('a desk message kind in the built page fails, in either quote', () => {
  for (const kind of [
    'question_answer',
    'trust_cancel',
    'copy_primary',
    'send_message',
    'permission_response',
    'remote_devices_open',
    'remote_start_enroll',
    'remote_revoke_ask',
    'remote_revoke',
  ]) {
    assert.ok(DESK_POSTS.includes(kind), kind);
  }
  for (const kind of DESK_POSTS) {
    for (const quoted of [`"${kind}"`, `'${kind}'`, `\`${kind}\``]) {
      const script = `${body};post({type:${quoted}})`;
      const own = scriptHash(script);
      const problems = checkBundle(page(script, transcriptPolicy(own)), own);
      assert.equal(problems.length, 1, quoted);
      assert.match(problems[0], /desk/);
    }
  }
  // The page's own four kinds, and a desk kind's name inside other text, are not the desk's messages.
  for (const own of ['"ready"', '"more"', '"open_link"', '"want_snapshot"', '"interrupted"', '"send_message_x"']) {
    const script = `${body};post({kind:${own}})`;
    const hashed = scriptHash(script);
    assert.deepEqual(checkBundle(page(script, transcriptPolicy(hashed)), hashed), [], own);
  }
});

test('the desk kinds are the bridge\'s', () => {
  const bridge = readFileSync(new URL('../src/bridge.ts', import.meta.url), 'utf8');
  for (const kind of DESK_POSTS) assert.ok(bridge.includes(`type: "${kind}"`), kind);
  for (const own of ['ready', 'more', 'open_link', 'want_snapshot']) assert.ok(!DESK_POSTS.includes(own), own);
});

test('the keymap code is keymap.ts code', () => {
  const keymap = readFileSync(new URL('../src/keymap.ts', import.meta.url), 'utf8');
  for (const name of KEYMAP_CODE) assert.ok(keymap.includes(`getModifierState?.("${name}")`), name);
});

test('the markers are keymap.ts text', () => {
  const keymap = readFileSync(new URL('../src/keymap.ts', import.meta.url), 'utf8');
  for (const marker of KEYMAP_MARKERS) assert.ok(keymap.includes(marker), marker);
});

test('connect-src other than none fails', () => {
  for (const connect of ["connect-src 'self'", "connect-src 'none' https://example.com", 'connect-src *']) {
    const policy = transcriptPolicy(hash).replace("connect-src 'none'", connect);
    assert.ok(checkBundle(page(body, policy), hash).some((p) => /connect-src/.test(p)), connect);
  }
  const without = transcriptPolicy(hash).replace("; connect-src 'none'", '');
  assert.ok(checkBundle(page(body, without), hash).some((p) => /connect-src/.test(p)));
});

test('a script-src hash that is not the script fails', () => {
  const other = `sha256-${createHash('sha256').update('other').digest('base64')}`;
  assert.ok(checkBundle(page(body, transcriptPolicy(other)), other).some((p) => /script-src/.test(p)));
  const loose = transcriptPolicy(hash).replace(`script-src '${hash}'`, `script-src '${hash}' 'unsafe-inline'`);
  assert.ok(checkBundle(page(body, loose), hash).some((p) => /script-src/.test(p)));
});

test('the recorded hash must be the script', () => {
  const other = `sha256-${createHash('sha256').update('other').digest('base64')}`;
  assert.ok(checkBundle(page(), other).some((p) => /script-sha256\.txt/.test(p)));
});

test('a page without its policy, or with the policy after the script, fails', () => {
  const none = page().replace(/<meta http-equiv[^>]*>/, '');
  assert.ok(checkBundle(none, hash).length > 0);
  const late = page().replace(/(<meta http-equiv[^>]*>)(.*)(<\/head>)/, '$2$1$3');
  assert.ok(checkBundle(late, hash).some((p) => /before/.test(p)));
});

test('a script the hash cannot be sure of is refused', () => {
  for (const bad of ['a<!--b', 'a\rb', 'a\0b']) assert.throws(() => inlineScripts(page(bad)));
  assert.throws(() => inlineScripts('<script src="x.js"></script>'));
  assert.throws(() => inlineScripts('<html></html>'));
  assert.equal(inlineScripts(page()).length, 1);
  assert.equal(scriptHash(body), hash);
});

test('withPolicy puts the meta first in head and hands back the hash', () => {
  const built = `<!doctype html><html><head><meta charset="UTF-8"><script type="module">${body}</script></head></html>`;
  const { html, hash: got } = withPolicy(built);
  assert.equal(got, hash);
  assert.deepEqual(checkBundle(html, got), []);
  assert.ok(html.indexOf('Content-Security-Policy') < html.indexOf('charset'));
  assert.throws(() => withPolicy(`${built}<script>second</script>`));
});

test('the transcript build ends with the size check and this check', () => {
  const pkg = JSON.parse(readFileSync(new URL('../package.json', import.meta.url)));
  assert.equal(pkg.scripts['build:transcript'], 'tsc -b && vite build -c vite.transcript.config.ts && ' +
    'node scripts/size-check.mjs 200000 dist-transcript/index.html && ' +
    'node scripts/transcript-bundle-check.mjs dist-transcript/index.html');
  assert.match(pkg.scripts.test, /node --test scripts\/size-check\.test\.mjs scripts\/transcript-bundle-check\.test\.mjs/);
});
