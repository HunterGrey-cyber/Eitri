// The transcript page's policy, and the checks its build ends with.
//
// The phone's page is one HTML file with one inline module script. Its Content-Security-Policy pins that script by
// its SHA-256, as the desk's panel does: inline script is never allowed by keyword, so markup that slips past the
// sanitiser cannot run. The build writes the policy into the page's first `<meta>` and the hash beside the page
// (`script-sha256.txt`), which the app sends as the page's header. The page reaches nothing: no network
// (`connect-src 'none'`), no frames, images only as `data:`.
//
// Run after the build: `node scripts/transcript-bundle-check.mjs dist-transcript/index.html`. It fails when the
// keyboard machinery or a message the desk's page sends to its host is in the page, when the policy is missing, late,
// lets the page connect anywhere or names another script than the one in the file, and when `script-sha256.txt`
// disagrees.
import { createHash } from 'node:crypto';
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

/** Text only `src/keymap.ts` holds (its help rows): any of it in the page means the key machinery was bundled. */
export const KEYMAP_MARKERS = [
  'Next / previous row; a long row scrolls first',
  'Detailed view: every result, longer cuts, no collapsed runs',
  'A view up (Ctrl+b is the prefix)',
];

/** Modifier names `src/keymap.ts` asks `getModifierState` about (`hasSuperOrHyper`, `hasAltGraph`, which
 *  `isPlainAnswerKey` calls); only key handling names them. Either as a string in the page means key handling was
 *  bundled, whatever happened to the help text and however the minifier wrote the call. */
export const KEYMAP_CODE = ['AltGraph', 'Hyper'];

/** Message kinds the desk's page sends to its host (`type` in `src/bridge.ts`) that answer, send, stop or copy: the
 *  phone's page sends only its own four (`ready`, `more`, `open_link`, `want_snapshot`), so any of these as a string
 *  in the page means a module that posts them was bundled, even where nothing draws it. */
export const DESK_POSTS = [
  'permission_response',
  'question_answer',
  'question_decline',
  'question_draft',
  'edit_question_other',
  'trust_answer',
  'trust_cancel',
  'trust_command',
  'confirm_bypass',
  'restore_answer',
  'editor_card_answer',
  'remote_enroll_answer',
  'remote_listen_answer',
  'send_message',
  'send_now',
  'queue_message',
  'run_shell',
  'interrupt',
  'stop_task',
  'stop_background',
  'copy_primary',
  'open_url',
  'open_path',
];

/** The page's policy for a script whose source expression is `hash` (`sha256-<base64>`, without quotes). */
export function transcriptPolicy(hash) {
  return [
    "default-src 'none'",
    `script-src '${hash}'`,
    "style-src 'unsafe-inline'",
    'img-src data:',
    "connect-src 'none'",
    "frame-ancestors 'none'",
    "base-uri 'none'",
    "form-action 'none'",
  ].join('; ');
}

/** `sha256-<base64>` of a script's text exactly as written. */
export function scriptHash(body) {
  return `sha256-${createHash('sha256').update(body, 'utf8').digest('base64')}`;
}

const TAG_NAME_END = /[\t\n\f\r />]/;

/**
 * Every `<script>` element of `html`, with where it starts and its text. Throws on anything that would make the
 * browser read a script differently from this scan (a comment opener, a carriage return or a NUL in its text, an end
 * tag that is not one), on a script loaded from a file, and on a page with none: the same refusals as the desk's
 * extractor, so a hash is only ever taken of text the browser hashes the same way.
 */
export function inlineScripts(html) {
  const lower = html.toLowerCase();
  const scripts = [];
  let from = 0;
  for (;;) {
    const start = lower.indexOf('<script', from);
    if (start < 0) break;
    const nameEnd = start + '<script'.length;
    if (!TAG_NAME_END.test(lower[nameEnd] ?? '')) {
      from = nameEnd;
      continue;
    }
    const close = lower.indexOf('>', nameEnd);
    if (close < 0) throw new Error(`a <script start tag at byte ${start} never ends`);
    const tag = lower.slice(nameEnd, close);
    if ((tag.split('"').length - 1) % 2 !== 0 || (tag.split("'").length - 1) % 2 !== 0) {
      throw new Error(`the <script start tag at byte ${start} has an unbalanced quote`);
    }
    if (/(^|[\s/])src\s*=/.test(tag)) throw new Error(`the <script at byte ${start} loads a file`);
    const bodyEnd = lower.indexOf('</script', close + 1);
    if (bodyEnd < 0) throw new Error(`the <script at byte ${start} is never closed`);
    if (!TAG_NAME_END.test(lower[bodyEnd + '</script'.length] ?? '')) {
      throw new Error(`the <script at byte ${start} meets a </script that is not its end`);
    }
    const body = html.slice(close + 1, bodyEnd);
    if (body.includes('<!--')) throw new Error(`the <script at byte ${start} holds <!--`);
    if (body.includes('\r')) throw new Error(`the <script at byte ${start} holds a carriage return`);
    if (body.includes('\0')) throw new Error(`the <script at byte ${start} holds a NUL`);
    scripts.push({ start, body });
    from = bodyEnd + '</script'.length;
  }
  if (scripts.length === 0) throw new Error('the page has no inline <script>');
  return scripts;
}

const META = /<meta\s+http-equiv="content-security-policy"\s+content="([^"]*)"\s*\/?>/gi;

/**
 * The built page with its policy as the first element of `<head>`, and the hash of its one script. Throws when the
 * page has no `<head>`, already carries a policy, or does not hold exactly one inline script.
 */
export function withPolicy(html) {
  const scripts = inlineScripts(html);
  if (scripts.length !== 1) throw new Error(`the page holds ${scripts.length} scripts, not one`);
  if (html.match(META) !== null) throw new Error('the page already carries a policy');
  const head = /<head\s*>/i.exec(html);
  if (head === null || head.index > scripts[0].start) throw new Error('the page has no <head> before its script');
  const hash = scriptHash(scripts[0].body);
  const at = head.index + head[0].length;
  const meta = `<meta http-equiv="Content-Security-Policy" content="${transcriptPolicy(hash)}">`;
  return { html: html.slice(0, at) + meta + html.slice(at), hash };
}

function directives(policy) {
  const out = new Map();
  for (const part of policy.split(';')) {
    const [name, ...sources] = part.trim().split(/\s+/);
    if (name) out.set(name.toLowerCase(), sources);
  }
  return out;
}

/** What is wrong with a built page and the hash recorded beside it; empty when nothing is. */
export function checkBundle(html, recordedHash) {
  const problems = [];
  const quoted = (name) => new RegExp(`["'\`]${name}["'\`]`).test(html);
  for (const marker of KEYMAP_MARKERS) {
    if (html.includes(marker)) problems.push(`the keyboard machinery is in the page: ${JSON.stringify(marker)}`);
  }
  for (const name of KEYMAP_CODE) {
    if (quoted(name)) problems.push(`the keyboard machinery is in the page: the modifier ${name}`);
  }
  for (const kind of DESK_POSTS) {
    if (quoted(kind)) problems.push(`a message the desk's page posts is in the page: ${kind}`);
  }
  let scripts;
  try {
    scripts = inlineScripts(html);
  } catch (error) {
    return [...problems, String(error.message)];
  }
  if (scripts.length !== 1) problems.push(`the page holds ${scripts.length} scripts, not one`);
  const metas = [...html.matchAll(META)];
  if (metas.length !== 1) return [...problems, `the page carries ${metas.length} policies, not one`];
  if (metas[0].index > scripts[0].start) problems.push('the policy must come before the script it pins');
  const policy = directives(metas[0][1]);
  const hash = scriptHash(scripts[0].body);
  if ((policy.get('default-src') ?? []).join(' ') !== "'none'") problems.push("default-src is not 'none'");
  const pinned = (policy.get('script-src') ?? []).join(' ');
  if (pinned !== `'${hash}'`) problems.push(`script-src is ${JSON.stringify(pinned)}, not the script's '${hash}'`);
  for (const [name, sources] of policy) {
    if (name.startsWith('script-src-')) problems.push(`${name} must not be set`);
    if (name === 'connect-src' && sources.join(' ') !== "'none'") problems.push(`connect-src is ${sources.join(' ')}`);
  }
  if (!policy.has('connect-src')) problems.push("connect-src is missing, so default-src alone would decide it");
  if (recordedHash.trim() !== hash) problems.push(`script-sha256.txt says ${recordedHash.trim()}, the script is ${hash}`);
  return problems;
}

function main(path) {
  const html = readFileSync(path, 'utf8');
  const recorded = readFileSync(join(dirname(path), 'script-sha256.txt'), 'utf8');
  const problems = checkBundle(html, recorded);
  for (const problem of problems) console.error(`${path}: ${problem}`);
  if (problems.length > 0) process.exit(1);
  console.log(`${path}: one script, pinned by ${recorded.trim()}; no keyboard machinery or desk messages; connects nowhere`);
}

if (process.argv[1] !== undefined && fileURLToPath(import.meta.url) === process.argv[1]) {
  if (process.argv.length !== 3) {
    console.error('usage: node scripts/transcript-bundle-check.mjs <built page>');
    process.exit(2);
  }
  main(process.argv[2]);
}
