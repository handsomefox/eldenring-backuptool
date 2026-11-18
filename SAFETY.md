# Safety and anti-cheat notes

Elden Ring runs Easy Anti-Cheat. Save Guard is built to stay outside everything EAC inspects, and to never leave a save at risk while copying it.

## Anti-cheat

The Steam launch option wraps the game as `"…\eldenring-backuptool.exe" --monitor %command%`. Save Guard forwards Steam's own command verbatim, so `start_protected_game.exe` and EAC start exactly as they would without it. Online play is unaffected.

Beyond that:

- Save Guard does not inject DLLs, install hooks, or read or write the game's memory.
- It does not modify any game file. It reads and copies save files under `%APPDATA%\EldenRing`.
- To tell whether the game is running, it enumerates process names with the standard `CreateToolhelp32Snapshot` API. It never opens a handle to `eldenring.exe`. The background monitor, the dashboard, and a restore all check this way.

## Backup integrity

A snapshot is built so that a half-written copy can never pass for a good one. Save Guard:

1. Streams the source `ER0000.sl2`, and `ER0000.sl2.bak` if present, into a deflate `save.zip` while hashing the original bytes with SHA-256. Per-file and total size limits stop a corrupted file from exhausting memory.
2. Writes that archive in a uniquely named temporary folder under the destination's `snapshots/` directory. Same volume, so finalizing is an atomic rename rather than a cross-volume move. The archive is standard ZIP and deflate, so Windows Explorer can open it for a manual restore.
3. Re-reads the archive, decompresses it, and confirms the bytes hash to the original.
4. Re-hashes the source to confirm it did not change mid-copy. If it did, Save Guard discards the attempt and retries.
5. Writes `metadata.json`, recording hashes of both the original uncompressed bytes and the finished ZIP, then renames the temporary folder into place. This happens only after every check above has passed. Both `save.zip` and `metadata.json` are flushed to disk before the rename, so a power cut cannot leave a finalized snapshot whose files were never written.

Some consequences worth stating outright:

- Deduplication compares content hashes, not modification times. An unchanged save does not produce a duplicate snapshot even if its timestamp moved.
- `ER0000.sl2` and `ER0000.sl2.bak` are hashed independently. Save Guard does not assume the two are in sync.
- A finalized snapshot is never overwritten.
- Save Guard hashes an existing archive before it trusts it for display, deduplication, retention, or a restore. The **Backups** tab lists a snapshot that fails this check as damaged, and offers no restore for it.

## Retention

Retention keeps, per account:

- the newest N snapshots, where N is the **Keep newest snapshots** setting
- the newest 10 pre-launch snapshots
- the newest 10 post-exit snapshots

A snapshot counts for these rules only if it passes its integrity check. Retention deletes a damaged snapshot once it is older than every snapshot it keeps, and never while it keeps no good snapshot.

Retention deletes nothing else. It removes finalized snapshot folders that sit directly under `<destination>/snapshots/`, and never touches anything outside that tree.

The new snapshot is finalized before old ones are pruned, so a good save is never deleted before its replacement exists. Every backup holds a lock on `<destination>/snapshots/.lock` from the copy through the prune, so a backup from the window and one from the background monitor never prune each other's new snapshot. While it holds that lock, a backup removes the temporary folders under `<destination>/snapshots/` that an interrupted backup left behind. It removes only real folders whose names start with `.tmp-`, never a link or junction.

## Restore

The **Restore…** button on the **Backups** tab puts a snapshot back as the live save. Before it changes anything, it backs up the save it replaces. Save Guard:

1. Refuses while `eldenring.exe` runs, found by process name as above. It checks again just before it moves any file.
2. Holds the backup lock until the last file moves, so no backup copies the save while files move.
3. Hashes the chosen snapshot's archive, then backs up the current save as a new manual snapshot, with every check from [Backup integrity](#backup-integrity). If that backup fails, nothing is restored. If the current save matches the newest snapshot already, no new snapshot is needed.
4. Extracts the snapshot into a temporary folder inside the save folder, so the final moves are renames on one volume. Each file is hashed while it is written, and again before the swap.
5. Moves the current `ER0000.sl2` and `ER0000.sl2.bak` into a second folder inside the save folder, then renames the extracted files into place. A `.bak` the snapshot lacks is moved aside too, so the game never pairs the restored save with an old `.bak`. If any rename fails, Save Guard moves every file back and reports the error. If moving them back also fails, the error names the folder that holds the previous save.
6. Deletes the folder of replaced files only after they hash the same as the backup from step 3. Otherwise it keeps the folder and names it.

## Path guards

The backup destination may not sit inside the live save folder, and the save folder may not sit inside the backup destination. Save Guard checks both directions after resolving existing symlinks and Windows junctions, and the GUI and the background monitor apply the same check. Without it, backups could end up backing up backups, or a retention delete could reach the live save. A `snapshots/` folder that is itself a symlink or junction is refused: nothing is written through it, and retention lists nothing in it.
