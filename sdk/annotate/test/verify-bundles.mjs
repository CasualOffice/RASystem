#!/usr/bin/env node
// Casual Annotate — bundle boundary check.
//
//   node test/verify-bundles.mjs        (skips cleanly if esbuild is not installed)
//
// Not a unit test: it bundles each Electron entry point the way the host app's esbuild config does
// and asserts what ended up INSIDE each bundle. That catches a class of mistake unit tests cannot —
// an import that quietly drags main-process APIs into a sandboxed preload, which fails at runtime
// in Electron and nowhere else.
//
// This exists because it already happened once: `preload.js` imported `CH` from `main.js`, which
// pulled `ipcMain`, `BrowserWindow` and `screen` into the preload bundle. Hence `channels.js`.

import { execFileSync } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import process from 'node:process';
import { fileURLToPath } from 'node:url';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const ROOT = path.resolve(HERE, '..');
const ADAPTER = path.join(ROOT, 'adapters', 'jitsi-electron');

/** esbuild is a dev convenience, not a dependency — skip rather than fail when it is absent. */
function findEsbuild() {
    for (const p of [
        path.join(ROOT, 'node_modules', '.bin', 'esbuild'),
        path.join(ROOT, '..', '..', 'node_modules', '.bin', 'esbuild'),
    ]) {
        if (fs.existsSync(p)) return p;
    }
    try {
        return execFileSync('which', [ 'esbuild' ], { encoding: 'utf8' }).trim() || null;
    } catch {
        return null;
    }
}

const esbuild = findEsbuild();
if (!esbuild) {
    console.log('verify-bundles: esbuild not found — skipped (npm i -D esbuild to enable)');
    process.exit(0);
}

const out = fs.mkdtempSync(path.join(os.tmpdir(), 'annotate-bundles-'));

/**
 * Each entry, the shape the host's esbuild config gives it, and what must and must not survive
 * bundling. The `forbid` lists are the point of this file.
 */
const CASES = [
    {
        name: 'meeting preload',
        entry: 'preload.js',
        args: [ '--platform=node', '--format=cjs', '--external:electron' ],
        forbid: [ 'ipcMain', 'BrowserWindow', 'getAllDisplays' ],
        require: [ 'ipcRenderer', 'contextBridge' ],
    },
    {
        name: 'overlay preload',
        entry: 'overlay-preload.js',
        args: [ '--platform=node', '--format=cjs', '--external:electron' ],
        forbid: [ 'ipcMain', 'BrowserWindow', 'getAllDisplays' ],
        require: [ 'ipcRenderer' ],
    },
    {
        name: 'overlay page',
        entry: 'overlay-page.js',
        args: [ '--platform=browser', '--format=iife', '--target=chrome120' ],
        // The overlay page is ordinary web content: it reaches Electron only through the bridge its
        // preload exposes, never by importing electron itself.
        forbid: [ 'require("electron")', 'ipcRenderer', 'ipcMain' ],
        require: [ 'casualAnnotateOverlay' ],
    },
    {
        name: 'main process',
        entry: 'main.js',
        args: [ '--platform=node', '--format=cjs', '--external:electron' ],
        forbid: [ 'contextBridge' ],
        require: [ 'ipcMain', 'BrowserWindow' ],
    },
    {
        name: 'meeting renderer helper',
        entry: 'renderer.js',
        args: [ '--platform=browser', '--format=esm' ],
        forbid: [ 'require("electron")', 'ipcMain', 'ipcRenderer' ],
        require: [ 'casualAnnotate' ],
    },
];

let failed = 0;

for (const c of CASES) {
    const file = path.join(out, `${c.entry}.bundle.js`);
    try {
        execFileSync(esbuild, [
            path.join(ADAPTER, c.entry), '--bundle', ...c.args, `--outfile=${file}`,
        ], { stdio: 'pipe' });
    } catch (e) {
        console.error(`✗ ${c.name}: failed to bundle\n${e.stderr?.toString() ?? e.message}`);
        failed++;
        continue;
    }

    const src = fs.readFileSync(file, 'utf8');
    const leaked = c.forbid.filter(t => src.includes(t));
    const missing = (c.require ?? []).filter(t => !src.includes(t));

    if (leaked.length || missing.length) {
        failed++;
        console.error(`✗ ${c.name} (${c.entry})`);
        if (leaked.length) console.error(`    leaked into the bundle: ${leaked.join(', ')}`);
        if (missing.length) console.error(`    expected but absent:    ${missing.join(', ')}`);
    } else {
        const kb = (src.length / 1024).toFixed(1);
        console.log(`✓ ${c.name.padEnd(24)} ${kb.padStart(6)} kb`);
    }
}

fs.rmSync(out, { recursive: true, force: true });

if (failed) {
    console.error(`\n${failed} bundle boundary violation(s).`);
    process.exit(1);
}
console.log('\nAll bundle boundaries hold.');
