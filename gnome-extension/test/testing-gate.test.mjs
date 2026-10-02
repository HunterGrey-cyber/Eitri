import {test} from 'node:test';
import assert from 'node:assert/strict';
import {readFileSync} from 'node:fs';

import {TESTING_ENV, testingMethodsWanted} from '../policy.js';

test('the test-only methods are wanted only when the variable is exactly "1"', () => {
    assert.equal(TESTING_ENV, 'EITRI_SHELL_EXTENSION_TESTING');
    assert.equal(testingMethodsWanted('1'), true);
    for (const value of [null, undefined, '', '0', 'true', 'yes', ' 1', '1 ', '1\n', '01', 1, true])
        assert.equal(testingMethodsWanted(value), false, JSON.stringify(value));
});

test('extension.js imports testing.js only behind the variable', () => {
    const src = readFileSync(new URL('../extension.js', import.meta.url), 'utf8');
    const imports = [...src.matchAll(/import\(\s*['"]\.\/testing\.js['"]\s*\)/g)];
    assert.equal(imports.length, 1, 'one dynamic import of testing.js');
    assert.ok(!/^\s*import\b[^(]*['"]\.\/testing\.js['"]/m.test(src), 'no static import of testing.js');
    // The gate reads gnome-shell's own environment and is checked before the import.
    const gate = src.indexOf('testingMethodsWanted(GLib.getenv(TESTING_ENV))');
    assert.ok(gate >= 0, 'the gate reads the variable from the environment');
    assert.ok(gate < imports[0].index, 'the gate comes before the import');
    const between = src.slice(gate, imports[0].index);
    assert.ok(/\?/.test(between) || /\bif\b/.test(between), 'the import is conditional on the gate');
});
