# Repository guidelines

The [README](README.md) covers what Save Guard does and how to build it. [SAFETY.md](SAFETY.md) states the guarantees it makes. This page covers the rules for changing it.

## Where code goes

One library and one binary, split so the core stays testable without a GUI:

- `src/lib.rs` is the portable core: `config`, `discovery`, `snapshot`, `retention`, `launch`, `paths`, `monitor`, `platform`, and `logging`. The unit tests live here.
- `src/gui.rs` is the egui dashboard, behind the default `gui` feature.
- `src/main.rs` dispatches between the GUI and `--monitor`.

Put new logic in the core rather than in `gui.rs`, so it can be tested on Linux. A function that reaches for egui types is in the wrong file.

## Stay outside anti-cheat

Elden Ring runs Easy Anti-Cheat, and the launch option puts Save Guard in front of the game. No DLL injection, no hooks, no reading or writing game memory, and no handle opened to `eldenring.exe`. Process detection goes through `CreateToolhelp32Snapshot` by name only.

Save Guard forwards Steam's `%command%` verbatim. Never parse it, rewrite it, or add arguments to it.

## Do not weaken the snapshot guarantees

[SAFETY.md](SAFETY.md) is a promise to users, not a description. Keep all of it:

- A snapshot is verified before it is finalized: hash the source, write the archive to a temp folder on the same volume, re-read and decompress it, confirm the bytes, re-hash the source to catch a mid-copy change, then rename into place.
- An existing archive is hashed before it is trusted for display, deduplication, or retention.
- Retention deletes only finalized snapshot folders directly under `<destination>/snapshots/`, and only after the new snapshot is finalized.
- The destination and the live save folder may not contain each other, checked in both directions after resolving symlinks and junctions.
- A finalized snapshot is never overwritten.

Deduplication compares content hashes, never modification times.

## Tests

Unit tests live in `src/lib.rs` beside the code they cover. Use `tempfile` for filesystem scenarios, and cover the rejection path: a truncated source, a mid-copy change, a path guard violation, and a corrupted existing archive.

CI cannot reach the Steam launch option, the background monitor across a real game session, or a manual restore. Exercise those three by hand on Windows.

## Bump CI tool pins by hand

`scripts/install-ci-tool.sh` downloads cargo-audit, cargo-machete, actionlint, and zizmor from their release pages and checks each archive against a pinned SHA-256 before it extracts anything. Dependabot cannot bump these pins. To bump one, change its row in the script and take the new hash from the digest GitHub records for the asset:

```sh
gh release view <tag> -R <owner>/<repo> --json assets --jq '.assets[] | select(.name == "<asset>") | .digest'
```

CI runs actionlint, shellcheck, and `zizmor --persona pedantic` on every push. Run all three before you push a workflow change.
