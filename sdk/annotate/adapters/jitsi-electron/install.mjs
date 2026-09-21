#!/usr/bin/env node
// Casual Annotate — installer for a `jitsi-meet-electron` checkout.
//
//   node install.mjs /path/to/jitsi-meet-electron [--revert] [--dry]
//
// Applies the four touch points from README.md plus the build wiring, by EDITING the checkout in
// place. Every edit is marked with a sentinel comment and is idempotent: running twice changes
// nothing, and `--revert` removes exactly what was added.
//
// Why an installer rather than a fork: jitsi-meet-electron moves, and a fork means re-merging
// forever. These are additive lines next to the ones `@jitsi/electron-sdk` already contributes, so
// an upgrade re-applies in seconds — and when an edit no longer matches, the script says which one
// and why instead of silently producing a broken build.

import fs from 'node:fs';
import path from 'node:path';
import process from 'node:process';
import { fileURLToPath } from 'node:url';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const SDK_ROOT = path.resolve(HERE, '../..');
const MARK = 'casual-annotate';
const BEGIN = `// >>> ${MARK} (added by sdk/annotate/adapters/jitsi-electron/install.mjs)`;
const END = `// <<< ${MARK}`;

const args = process.argv.slice(2);
const target = args.find(a => !a.startsWith('--'));
const revert = args.includes('--revert');
const dry = args.includes('--dry');

if (!target) {
    console.error('usage: node install.mjs /path/to/jitsi-meet-electron [--revert] [--dry]');
    process.exit(2);
}

/** Wrap added lines so `--revert` can find them again with certainty. */
const block = body => `${BEGIN}\n${body}\n${END}`;

/**
 * One edit: find `anchor` in the file and put `body` after it (or before, with `where: 'before'`).
 *
 * `anchor` is matched literally. If it is missing, that is reported as a failure naming the file —
 * upstream moved, and guessing at a fuzzy match is how installers quietly produce broken builds.
 *
 * `indent` is the exact column the surrounding code sits at, per file: `main.ts` is 4, the JSX in
 * `Conference.tsx` is 8. Both repos lint indentation, so getting it wrong fails their lint, not ours.
 */
const edits = [
    {
        file: 'main.ts',
        why: 'create the overlay window and relay ops to it',
        anchor: "import config from './app/features/config';",
        body: block(
            "import setupAnnotateMain from '@casualoffice/annotate/adapters/jitsi-electron/main.js';\n"
            + "import * as annotatePath from 'path';",
        ),
    },
    {
        file: 'main.ts',
        why: 'install it once the meeting window exists',
        anchor: 'setupRemoteControlMain(meetingWindow',
        after: true,
        indent: 4,
        body: block(
            'setupAnnotateMain(meetingWindow, {\n'
            + '    overlayUrl: `file://${annotatePath.join(rootDir, \'build\', \'annotate-overlay.html\')}`\n'
            + '});',
        ),
    },
    {
        file: 'app/preload/preload.ts',
        why: 'expose window.casualAnnotate to the meeting renderer',
        anchor: 'installJitsiElectronSdk();',
        after: true,
        body: block(
            "import { installAnnotateBridge } from '@casualoffice/annotate/adapters/jitsi-electron/preload.js';\n"
            + 'installAnnotateBridge();',
        ),
    },
    {
        file: 'app/features/conference/components/Conference.tsx',
        why: 'relay endpoint messages and drive annotation from the share lifecycle',
        anchor: 'setupRemoteControlRender(this._api);',
        after: true,
        indent: 8,
        body: block(
            'setupAnnotateRender(this._api, {\n'
            + '    onRefused: (message: string) => console.warn(\'[annotate]\', message)\n'
            + '});',
        ),
    },
    {
        file: 'app/features/conference/components/Conference.tsx',
        why: 'import the renderer helper',
        anchor: "import JitsiMeetExternalAPI from '../external_api';",
        after: true,
        body: block(
            "import { setupAnnotateRender } from '@casualoffice/annotate/adapters/jitsi-electron/renderer.js';",
        ),
    },
    {
        file: 'esbuild.js',
        why: 'bundle the overlay page, its preload, and copy its HTML',
        anchor: 'const configs = {',
        before: true,
        body: block(
            "const ANNOTATE = require('path').join(\n"
            + "    __dirname, 'node_modules', '@casualoffice', 'annotate', 'adapters', 'jitsi-electron');",
        ),
    },
    {
        file: 'esbuild.js',
        why: 'add the overlay entry points',
        anchor: '    renderer: {',
        before: true,
        body: block(
            '    annotateOverlay: {\n'
            + '        ...common,\n'
            + "        entryPoints: { 'annotate-overlay': require('path').join(ANNOTATE, 'overlay-page.js') },\n"
            + '        outdir: OUTDIR,\n'
            + "        platform: 'browser',\n"
            + "        format: 'iife',\n"
            + '        target: RENDERER_TARGET\n'
            + '    },\n'
            + '    annotateOverlayPreload: {\n'
            + '        ...common,\n'
            + "        entryPoints: { 'annotate-overlay-preload': require('path').join(ANNOTATE, 'overlay-preload.js') },\n"
            + '        outdir: OUTDIR,\n'
            + "        platform: 'node',\n"
            + "        format: 'cjs',\n"
            + '        target: MAIN_TARGET,\n'
            + "        external: [ 'electron' ]\n"
            + '    },',
        ),
    },
];

/** Copy the overlay HTML into the build output, pointing at the bundled script + preload. */
function writeOverlayHtml(root) {
    const out = path.join(root, 'build');
    fs.mkdirSync(out, { recursive: true });
    const src = fs.readFileSync(path.join(HERE, 'overlay.html'), 'utf8');
    // The bundled entry is emitted as `annotate-overlay.js` beside this file.
    const html = src.replace('./overlay-page.js', './annotate-overlay.js').replace(' type="module"', '');
    write(path.join(out, 'annotate-overlay.html'), html);
}

function write(file, content) {
    if (dry) {
        console.log(`  would write ${path.relative(target, file)}`);
        return;
    }
    fs.writeFileSync(file, content);
}

/** Link the SDK into the checkout's node_modules so bare imports resolve. */
function linkSdk(root) {
    const dest = path.join(root, 'node_modules', '@casualoffice', 'annotate');
    if (dry) {
        console.log(`  would link ${path.relative(target, dest)} -> ${SDK_ROOT}`);
        return;
    }
    fs.mkdirSync(path.dirname(dest), { recursive: true });
    try {
        fs.rmSync(dest, { recursive: true, force: true });
    } catch { /* nothing there */ }
    fs.symlinkSync(SDK_ROOT, dest, 'junction');
}

function apply(root) {
    let applied = 0;
    let skipped = 0;
    const failures = [];

    for (const e of edits) {
        const file = path.join(root, e.file);
        if (!fs.existsSync(file)) {
            failures.push(`${e.file}: not found`);
            continue;
        }
        let src = fs.readFileSync(file, 'utf8');

        // Indent FIRST, then test for presence: the idempotency check has to compare the text we
        // would actually write, or an indented edit never matches itself and re-applies every run.
        const pad = e.indent ? ' '.repeat(e.indent) : '';
        const body = pad ? e.body.split('\n').map(l => (l ? pad + l : l)).join('\n') : e.body;

        if (src.includes(body)) {
            skipped++;
            continue;
        }
        const at = src.indexOf(e.anchor);
        if (at === -1) {
            failures.push(`${e.file}: anchor not found — "${e.anchor.slice(0, 48)}…" (${e.why})`);
            continue;
        }

        if (e.before) {
            // Exactly one newline, so apply and revert are precise inverses. Two would leave a
            // blank line behind on every revert and the checkout would drift from pristine.
            src = src.slice(0, at) + body + '\n' + src.slice(at);
        } else {
            const eol = src.indexOf('\n', at + e.anchor.length);
            const cut = eol === -1 ? src.length : eol + 1;
            src = src.slice(0, cut) + body + '\n' + src.slice(cut);
        }
        write(file, src);
        applied++;
    }

    linkSdk(root);
    writeOverlayHtml(root);
    return { applied, skipped, failures };
}

function undo(root) {
    let removed = 0;
    const seen = new Set(edits.map(e => e.file));
    for (const rel of seen) {
        const file = path.join(root, rel);
        if (!fs.existsSync(file)) continue;
        const src = fs.readFileSync(file, 'utf8');
        // Non-greedy between the sentinels, and repeated — a file may carry several blocks. The
        // leading indentation and BOTH surrounding newlines go too, or revert leaves a blank line
        // behind on every run and the checkout never returns to pristine.
        const cleaned = src.replace(
            new RegExp(`[ \\t]*${escapeRe(BEGIN)}[\\s\\S]*?${escapeRe(END)}\\n?`, 'g'),
            '',
        );
        if (cleaned !== src) {
            write(file, cleaned);
            removed++;
        }
    }
    const link = path.join(root, 'node_modules', '@casualoffice', 'annotate');
    if (!dry && fs.existsSync(link)) fs.rmSync(link, { recursive: true, force: true });
    const html = path.join(root, 'build', 'annotate-overlay.html');
    if (!dry && fs.existsSync(html)) fs.rmSync(html);
    return removed;
}

const escapeRe = s => s.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');

// ── run ─────────────────────────────────────────────────────────────────────────────────────────

const root = path.resolve(target);
if (!fs.existsSync(path.join(root, 'main.ts'))) {
    console.error(`not a jitsi-meet-electron checkout: ${root}`);
    process.exit(2);
}

if (revert) {
    const n = undo(root);
    console.log(`${dry ? '[dry] ' : ''}casual-annotate: removed from ${n} file(s)`);
    process.exit(0);
}

const { applied, skipped, failures } = apply(root);
console.log(`${dry ? '[dry] ' : ''}casual-annotate: ${applied} edit(s) applied, ${skipped} already present`);
if (failures.length) {
    console.error('\nFAILED — upstream has moved. Re-anchor these in install.mjs:');
    for (const f of failures) console.error(`  • ${f}`);
    process.exit(1);
}
console.log('\nNext:\n  cd ' + target + '\n  npm install && npm start');
