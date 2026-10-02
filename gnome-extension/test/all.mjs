// `node --test gnome-extension/test` names this directory, and this node resolves a directory through its
// package.json's `main`: so this file is what runs, and it loads every test file here.
import './direction.test.mjs';
import './policy.test.mjs';
import './pull.test.mjs';
import './testing-gate.test.mjs';
