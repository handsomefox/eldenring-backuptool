# Safety & anti-cheat notes

Elden Ring runs **Easy Anti-Cheat (EAC)**. Save Guard is designed to stay entirely outside
anything EAC inspects, and to never put a save at risk while backing it up.

## Anti-cheat

- The Steam launch option wraps the game as `"…\eldenring-backuptool.exe" --monitor %command%`.
  Steam's own command (`%command%`) is forwarded **verbatim**, so `start_protected_game.exe` and
  EAC start exactly as they would without Save Guard. Online play is unaffected.
- Save Guard does **not** inject DLLs, hook, or read/write the game's memory.
- Save Guard does **not** modify any game files — it only reads and copies save files under
  `%APPDATA%\EldenRing`.
- To tell whether the game is running, it enumerates process **names** with the standard
  `CreateToolhelp32Snapshot` API. It never opens a handle to `eldenring.exe`.

## Backup integrity

Snapshots are created so a half-written copy can never look valid:

1. Hash the source `.sl2` (and `.sl2.bak`) with SHA-256.
2. Write them into a deflate `save.zip` inside a uniquely named **temporary folder under the
   destination's `snapshots/` dir** (same volume, so the finalize step is an atomic rename — never
   a cross-volume move). The archive uses standard ZIP/deflate so Windows Explorer can open it for
   a manual restore.
3. Re-read the archive, decompress it, and confirm the bytes hash to the original source.
4. Re-hash the source to confirm it didn't change mid-copy; if it did, discard and retry.
5. Only after verification: write `metadata.json` (recording hashes of the **original**,
   uncompressed bytes), then atomically rename the temp folder into place.

- **Deduplication is by content hash**, not modification time — an unchanged save (even if its
  timestamp changed) does not create a duplicate snapshot.
- `.sl2` and `.sl2.bak` are hashed **independently**; they are not assumed to be in sync.
- An existing finalized snapshot is never overwritten.

## Retention

- Retention keeps the newest N snapshots **per account** and only ever deletes finalized snapshot
  folders that live directly under `<destination>/snapshots/`. Temporary folders are ignored, and
  nothing outside the managed tree is touched.
- The new snapshot is finalized **before** old ones are pruned, so a good save is never deleted
  before its replacement exists.

## Path guards

- The backup destination may not be inside the live save folder, and the save folder may not be
  inside the backup destination (checked in both directions). This prevents backups from backing
  up backups, or a delete touching the live save.
