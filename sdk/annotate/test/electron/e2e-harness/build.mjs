#!/usr/bin/env node
// Casual Annotate — build the harness's Electron-boundary bundles (ADR-107 Decision 10).
//
// Only the pieces that genuinely need bundling: a sandboxed preload (must be CJS, must not carry
// `electron`'s Node half into the sandbox) and the two scripts that get loaded into — or injected
// into — a webpage that is not this package (the overlay page, and the in-page relay). Everything
// else (`renderer.js`, loaded by `host.html`) is plain ES modules resolved straight off disk, the
// same way a real Electron renderer can.

import { execFileSync } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const ROOT = path.resolve(HERE, '..', '..', '..');
const ADAPTER = path.join(ROOT, 'adapters', 'jitsi-electron');
const OUT = process.env.CASUAL_ANNOTATE_BUILD_DIR ?? '/tmp/casual-annotate-build';

fs.mkdirSync(OUT, { recursive: true });

function esbuild(entry, outfile, args) {
    execFileSync('npx', [ 'esbuild', entry, '--bundle', `--outfile=${outfile}`, ...args ],
        { cwd: ROOT, stdio: 'inherit' });
}

// NOT `adapters/jitsi-electron/preload.js` directly: that file only EXPORTS
// `installAnnotateBridge`/`installOverlayBridge`, deliberately leaving the choice of which to call
// (and for which window) to whoever wires a specific host — it never calls either itself. Bundling it
// raw, as an earlier version of this script did, produced a `harness-preload.cjs` that defined
// `installAnnotateBridge` but never ran it, so `window.casualAnnotate` was never installed and every
// call into it failed with "preload bridge missing" — a real bug in the TEST harness, not the SDK.
esbuild(path.join(HERE, 'preload-entry.js'), path.join(OUT, 'harness-preload.cjs'),
    [ '--platform=node', '--format=cjs', '--external:electron' ]);

esbuild(path.join(ADAPTER, 'overlay-preload.js'), path.join(OUT, 'overlay-preload.cjs'),
    [ '--platform=node', '--format=cjs', '--external:electron' ]);

esbuild(path.join(ADAPTER, 'overlay-page.js'), path.join(OUT, 'annotate-overlay-page.js'),
    [ '--platform=browser', '--format=iife', '--target=chrome120' ]);

esbuild(path.join(ADAPTER, 'injected-relay.js'), path.join(OUT, 'annotate-relay.js'),
    [ '--platform=browser', '--format=iife', '--target=chrome120' ]);

esbuild(path.join(ROOT, 'standalone', 'inject.js'), path.join(OUT, 'casual-annotate.js'),
    [ '--platform=browser', '--format=iife', '--target=chrome120' ]);

// overlay.html references `./overlay-page.js` — rewrite that one reference for the bundled name
// rather than duplicating the whole file.
const overlayHtml = fs.readFileSync(path.join(ADAPTER, 'overlay.html'), 'utf8')
    .replace('./overlay-page.js', './annotate-overlay-page.js');
fs.writeFileSync(path.join(OUT, 'annotate-overlay.html'), overlayHtml);

console.log(`Harness bundles written to ${OUT}`);
