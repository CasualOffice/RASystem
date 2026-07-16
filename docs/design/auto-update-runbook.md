# Auto-update runbook — activating signed updates (ADR-078)

> The updater is **scaffolded but inert** in the repo: the Tauri updater plugin, the
> `check_for_updates` / `install_update` commands, the `updater:default` capability, the
> `plugins.updater` config, and the CI signing env are all wired, but `bundle.createUpdaterArtifacts`
> is **off** and `plugins.updater.pubkey` is an **empty placeholder**. This keeps keyless CI green and
> ships **no** throwaway key. Follow these steps once to turn on signed auto-update.

This is the **free update-integrity** layer (Tauri's Ed25519/minisign signing). It is independent of
the paid **OS code-signing / notarization** (Gatekeeper/SmartScreen), which stays deferred until a
sponsor funds certificates (ADR-072). Doing this does **not** remove the "unsigned by the OS" warning
on first launch — but it does guarantee that every *update* is cryptographically verified against a
key only you hold. Unsigned-by-the-OS ≠ unverified updates.

## One-time setup

1. **Generate the signing keypair** (holds the private key locally; never commit it):
   ```sh
   # from app/ (where the Tauri CLI resolves), or via npx:
   npx @tauri-apps/cli signer generate -w ~/.casual-ras/updater.key
   ```
   This prints a **public key** and writes the password-protected **private key** to the path given.
   Keep the private key and its password in a password manager / secrets vault — losing it means you
   can never publish an update that existing installs will accept.

2. **Embed the public key.** Paste the printed public key into `app/src-tauri/tauri.conf.json`:
   ```json
   "plugins": { "updater": { "pubkey": "<PASTE PUBLIC KEY>", "endpoints": [ ... ] } }
   ```

3. **Turn on artifact generation.** In `app/src-tauri/tauri.conf.json`, add under `bundle`:
   ```json
   "bundle": { "createUpdaterArtifacts": true, ... }
   ```
   (Producing updater artifacts **requires** a signing key at build time — do this step *with* the
   secrets from step 4 in place, or local `tauri build` will error asking for the key.)

4. **Add the CI secrets** (GitHub → Settings → Secrets and variables → Actions):
   - `TAURI_SIGNING_PRIVATE_KEY` — the **contents** of `~/.casual-ras/updater.key`.
   - `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` — the password chosen in step 1.
   The release workflow already reads both (`.github/workflows/release.yml`).

5. **Release.** Push a `v*` tag. `tauri-action` builds, **signs** each bundle, and uploads the
   artifacts plus a generated **`latest.json`** to the GitHub Release. The app's configured endpoint
   (`…/releases/latest/download/latest.json`) then serves it.

## How it behaves once active

- The user clicks **Check for updates** on the home screen. If a newer version is published, the
  button becomes **"Install vX.Y & restart"**; a second click downloads, **verifies the signature
  against the embedded pubkey**, installs, and relaunches. A bad/missing signature aborts the install
  — no unsigned code is ever run. Nothing updates silently (Inv 1).

## Key hygiene

- The private key lives only in your keystore + the CI secret. **Never** commit it, never paste it in
  logs/issues.
- Rotating the key invalidates auto-update for already-installed copies signed under the old key (they
  can't verify the new signature) — they must be re-installed manually. Rotate deliberately.
- This key signs *the software artifacts*. It has nothing to do with the session identity/grant keys
  (`ras-identity`/`ras-grant`) — different trust domain.
