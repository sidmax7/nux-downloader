# Releasing

Who this is for: the maintainer cutting a release. Every step that
publishes something (pushing a tag, publishing the draft, pushing to the
AUR) is done by hand.

## One-time setup: the update signing key

Installed apps only accept updates signed with the project's key.

1. Generate it (choose a password; keep both safe, they cannot be
   recovered, and losing them means existing installs can no longer
   update):

   ```sh
   cd src-tauri
   ../ui/node_modules/.bin/tauri signer generate -w ~/.tauri/nux-downloader.key
   ```

2. Put the **public** key (`~/.tauri/nux-downloader.key.pub`, one line)
   into `src-tauri/tauri.conf.json` → `plugins.updater.pubkey`, and
   commit it. The public key is not secret.
3. In the GitHub repository → Settings → Secrets and variables → Actions,
   add `TAURI_SIGNING_PRIVATE_KEY` (the contents of
   `~/.tauri/nux-downloader.key`) and `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`.

The release workflow refuses to run while the public key is empty.

## Cutting a release

1. Pick the version (SemVer). A pre-release is `X.Y.Z-beta.N`.
2. Set it in three places: `Cargo.toml` (`[workspace.package] version`),
   `src-tauri/tauri.conf.json` (`version`) and `ui/package.json`
   (`version`). Run `cargo check` so `Cargo.lock` follows.
3. Move the `[Unreleased]` notes in `CHANGELOG.md` under a new
   `## [X.Y.Z]` heading. That section becomes the release notes and the
   text the in-app update banner shows.
4. If dependencies changed, regenerate the licence file:
   `python3 scripts/third-party-notices.py`.
5. Commit, then tag and push the tag:

   ```sh
   git tag vX.Y.Z
   git push origin vX.Y.Z
   ```

6. The **Release** workflow builds the Linux AppImage, `.deb` and `.rpm`
   and the Linux `nuxd` archive, and attaches them, `SHA256SUMS` and
   `latest.json` to a **draft** release. Tags with a `-` become
   pre-releases. If a build job fails, re-run the failed jobs from the
   run's page. If only the last step (checksums and update manifest)
   fails, you can also redo it by hand:
   `bash scripts/finish-release.sh vX.Y.Z` (`DRY_RUN=1` to only print
   the two files).
7. Download and try at least the Linux AppImage and `.deb` from the
   draft. Then publish it. Only a published, non-pre-release is "latest",
   so only then do installed apps see the update.

## Arch package (`nuxd-bin`)

Until the package is on the AUR, the README tells Arch users to run
`makepkg -si` in `packaging/aur/nuxd-bin/`. So after every release, bump
`pkgver` there and fill in the checksums (`updpkgsums`, checked against
`SHA256SUMS`), and commit, so those users get a verified build.

To publish on the AUR, once you have an account, after the GitHub
release is published:

```sh
cp packaging/aur/nuxd-bin/PKGBUILD <your clone of ssh://aur@aur.archlinux.org/nuxd-bin.git>/
cd <that clone>
# bump pkgver (and reset pkgrel=1), then:
updpkgsums                       # fills sha256sums from the release
makepkg -fsi && namcap PKGBUILD *.pkg.tar.zst
makepkg --printsrcinfo > .SRCINFO
git commit -am "Update to X.Y.Z" && git push
```

Check the sums against the release's `SHA256SUMS`. Copy the final
`PKGBUILD` back into `packaging/aur/nuxd-bin/` so the repository keeps
the current one.

## Adding macOS and Windows

Releases are Linux-only until those builds have been tried on real
machines. The pieces are in place, switched off:

1. On the Actions tab, run the **CI** workflow by hand ("Run workflow").
   A manual run adds the macOS and Windows legs (clippy, tests, app
   build) that pushes skip. Get them green.
2. Build and try the app on each system: install, add a download, quit
   and resume, the tray, an update.
3. In `.github/workflows/release.yml`, uncomment the `macos` and
   `windows` entries of the `app` matrix and the three non-Linux `cli`
   targets. `scripts/update-manifest.py` adds each platform to
   `latest.json` as soon as its files are in the release.
4. Bring back the Windows and macOS install sections in the README (they
   are in the git history of `README.md`, just before they were replaced
   by "Windows and macOS: not released yet"), and say so in the
   changelog.

## What the workflow does not do

- Code signing for macOS and Windows (planned once there are accounts).
  Until then users see the warnings described in the README.
- crates.io publishing.
