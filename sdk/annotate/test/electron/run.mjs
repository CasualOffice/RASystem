#!/usr/bin/env node
// Builds the overlay bundles into `test/electron/.build`, then runs `smoke.mjs` under Electron.
//
//   node test/electron/run.mjs
//
// Skips cleanly when esbuild or electron is missing — neither is a dependency of this package, and
// a smoke test that cannot run should say so rather than fail the suite.

import { execFileSync, spawnSync } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import process from 'node:process';
import { fileURLToPath } from 'node:url';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const ROOT = path.resolve(HERE, '../..');
const ADAPTER = path.join(ROOT, 'adapters', 'jitsi-electron');
const OUT = path.join(HERE, '.build');

/** Look in this package, then the repo root, then PATH, then an explicit env override. */
function find(bin, env) {
    if (process.env[env] && fs.existsSync(process.env[env])) return process.env[env];
    for (const base of [ ROOT, path.resolve(ROOT, '../..'), process.cwd() ]) {
        const p = path.join(base, 'node_modules', '.bin', bin);
        if (fs.existsSync(p)) return p;
    }
    const r = spawnSync('which', [ bin ], { encoding: 'utf8' });
    return r.status === 0 ? r.stdout.trim() : null;
}

const esbuild = find('esbuild', 'ESBUILD_PATH');
const electron = find('electron', 'ELECTRON_PATH');

if (!esbuild || !electron) {
    console.log(`electron smoke: skipped (${!esbuild ? 'esbuild' : 'electron'} not found).`);
    console.log('  npm i -D esbuild electron   — or set ESBUILD_PATH / ELECTRON_PATH');
    process.exit(0);
}

fs.rmSync(OUT, { recursive: true, force: true });
fs.mkdirSync(OUT, { recursive: true });

execFileSync(esbuild, [
    path.join(ADAPTER, 'overlay-page.js'), '--bundle',
    '--platform=browser', '--format=iife', '--target=chrome120',
    `--outfile=${path.join(OUT, 'annotate-overlay.js')}`,
], { stdio: 'inherit' });

execFileSync(esbuild, [
    path.join(ADAPTER, 'overlay-preload.js'), '--bundle',
    '--platform=node', '--format=cjs', '--external:electron',
    `--outfile=${path.join(OUT, 'annotate-overlay-preload.js')}`,
], { stdio: 'inherit' });

const html = fs.readFileSync(path.join(ADAPTER, 'overlay.html'), 'utf8')
    .replace('./overlay-page.js', './annotate-overlay.js')
    .replace(' type="module"', '');
fs.writeFileSync(path.join(OUT, 'annotate-overlay.html'), html);

const r = spawnSync(electron, [ path.join(HERE, 'smoke.mjs') ], {
    stdio: 'inherit',
    env: { ...process.env, ANNOTATE_BUILD: OUT, ELECTRON_DISABLE_SECURITY_WARNINGS: '1' },
});
process.exit(r.status ?? 1);
