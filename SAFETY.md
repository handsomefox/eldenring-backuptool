# Safety and anti-cheat notes

Elden Ring runs Easy Anti-Cheat. Save Guard is built to stay outside everything EAC inspects, and to never leave a save at risk while copying it.

## Anti-cheat

The Steam launch option wraps the game as `"…\Elden Ring Backuptool.exe" --monitor %command%`. Save Guard forwards Steam's own command verbatim, so `start_protected_game.exe` and EAC start exactly as they would without it. Online play is unaffected.

Beyond that:

- Save Guard does not inject DLLs, install hooks, or read or write the game's memory.
- It does not modify any game file. It reads and copies save files under `%APPDATA%\EldenRing`.
- To tell whether the game is running, it enumerates process names with the standard `CreateToolhelp32Snapshot` API. It never opens a handle to `eldenring.exe`.

## Backup integrity

A snapshot is built so that a half-written copy can never pass for a good one. Save Guard:

1. Streams the source `ER0000.sl2`, and `ER0000.sl2.bak` if present, into a deflate `save.zip` while hashing the original bytes with SHA-256. Per-file and total size limits stop a corrupted file from exhausting memory.
2. Writes that archive in a uniquely named temporary folder under the destination's `snapshots/` directory. Same volume, so finalizing is an atomic rename rather than a cross-volume move. The archive is standard ZIP and deflate, so Windows Explorer can open it for a manual restore.
3. Re-reads the archive, decompresses it, and confirms the bytes hash to the original.
4. Re-hashes the source to confirm it did not change mid-copy. If it did, Save Guard discards the attempt and retries.
5. Writes `metadata.json`, recording hashes of both the original uncompressed bytes and the finished ZIP, then renames the temporary folder into place. This happens only after every check above has passed.

Some consequences worth stating outright:

- Deduplication compares content hashes, not modification times. An unchanged save does not produce a duplicate snapshot even if its timestamp moved.
- `ER0000.sl2` and `ER0000.sl2.bak` are hashed independently. Save Guard does not assume the two are in sync.
- A finalized snapshot is never overwritten.
- Save Guard hashes an existing archive before it trusts it for display, deduplication, or retention.

## Retention

Retention keeps the newest N snapshots per account, counting only those that pass their integrity check. It deletes nothing else: it removes finalized snapshot folders that sit directly under `<destination>/snapshots/`, ignores temporary folders, and never touches anything outside that tree.

The new snapshot is finalized before old ones are pruned, so a good save is never deleted before its replacement exists.

## Path guards

The backup destination may not sit inside the live save folder, and the save folder may not sit inside the backup destination. Save Guard checks both directions after resolving existing symlinks and Windows junctions, and the GUI and the background monitor apply the same check. Without it, backups could end up backing up backups, or a retention delete could reach the live save.
