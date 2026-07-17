# Elden Ring Save Guard

Automatic, versioned backups of your **vanilla Elden Ring** save — created quietly in the
background every time you play, so you always have a known-good save to fall back on.

## What this program does

Every time you launch Elden Ring through Steam, Save Guard makes a verified copy of your save
before you play, again whenever the save changes while you play, and once more after you quit.
Each copy is a separate, timestamped snapshot kept on your own PC. If your save is ever ruined,
you can copy an older snapshot back.

## Why it exists

- **Malicious multiplayer users** have been reported altering or ruining other players' saves —
  forcing boss deaths, pushing story progression, triggering cutscenes, teleporting characters,
  unlocking achievements, or leaving world state broken.
- **Ordinary problems** — crashes, power loss, disk errors — can corrupt or truncate a save too.
- **Steam Cloud is synchronization, not backup history.** If your save is damaged, the damaged
  version can simply become the newest copy that syncs everywhere. There is no built-in "go back
  a few versions" button.

Save Guard **cannot stop an attack or corruption from happening during a session** — nothing on
your PC can. What it does is keep a history of good saves so you can recover afterward.

## Supported

- Windows 10 and Windows 11
- The **Steam** version of Elden Ring
- **Vanilla** saves (`ER0000.sl2` and its `.sl2.bak`)
- **Not** supported in this release: Seamless Co-op `.co2` saves
- **No administrator rights required**

It does **not** modify the game, inject code, read game memory, or interfere with Easy
Anti-Cheat. It only reads and copies your save files. See [SAFETY.md](SAFETY.md).

## Install

1. Download the latest release ZIP and extract it to a **permanent location** you won't move
   later — e.g. `C:\Tools\EldenRingSaveGuard\`, not your Downloads or a temp folder. The Steam
   launch option points at this exact path (see step 6); if you move, rename, or delete the
   folder afterward, **Elden Ring will fail to launch** until you fix it. If you do move it, just
   re-copy the launch option from the Help tab and paste the new one into Steam.
2. Run **`eldenring-backuptool.exe`**.
3. On the **Dashboard**, pick the Steam account (save folder) you want to protect. If you only
   have one, it's already selected.
4. Optionally change the backup destination on the **Settings** tab.
5. Open the **Help** tab and click **Copy launch option**.
6. In Steam: **Elden Ring → Properties → General → Launch Options**, and paste it there.
7. Launch Elden Ring normally. Backups now happen automatically.

The launch option looks like this (your path will differ):

```
"C:\Users\You\Desktop\eldenring-backuptool\eldenring-backuptool.exe" --monitor %command%
```

`%command%` is Steam's own launch command — it is passed through untouched, so Easy Anti-Cheat
and online play work exactly as before.

## Confirming it works

On the **Dashboard** you'll see the selected account, the save file and its size, the backup
destination, how many snapshots are stored, and when the last backup happened. After you play a
session (or press **Back up now**), the status turns to **Protected — backups exist**.

## Multiple Steam accounts

`%APPDATA%\EldenRing` can contain several numbered folders (multiple Steam accounts, Family
Sharing, old copies, another person on the same PC). Save Guard lists the ones that actually
contain a save and lets you choose which to protect. It does **not** blindly back up every
folder, and it does **not** guess based only on which was modified most recently. Switching the
selected account never merges or deletes another account's snapshots.

## Restoring a save (manual)

Restore is done by hand, so you stay in full control:

1. **Fully close Elden Ring**, and preferably **exit Steam completely**.
2. In the app, open the **Backups** tab, choose a snapshot, and click **Open** to reveal its
   folder in Explorer.
3. Double-click **`save.zip`** (Windows opens it like a folder) and copy the `.sl2` file (and
   `.sl2.bak` if present) into your save folder (`%APPDATA%\EldenRing\<your-id>\`), replacing the
   current files.
4. Reopen Steam. **If Steam Cloud reports a conflict, choose the LOCAL copy** — the one you just
   restored — rather than the newer cloud version. (The exact wording of the conflict prompt
   varies between Steam versions; pick the option that keeps your **local** files.)

## Backup location

Default: `Documents\Game Save Backups\Elden Ring\<SteamID64>\snapshots\`. You can change it on
the **Settings** tab. Each snapshot folder is named by date and time and contains `save.zip` (the
compressed save files — Elden Ring saves are mostly empty space and shrink roughly 15–20×) plus a
small `metadata.json` with verification hashes.

## Uninstalling

1. In Steam, open **Elden Ring → Properties → General → Launch Options** and **clear the field**
   first. (If you delete the app before clearing this, Steam will try to run a missing file and
   the game won't launch.)
2. Delete the extracted application folder.
3. Your backups are kept. To remove them, delete the backup destination folder yourself.
4. Optional: delete `%LOCALAPPDATA%\EldenRingSaveGuard\` to remove settings and logs.

## Limitations

- It does **not** prevent cheating or corruption — it gives you recovery points.
- It **cannot** undo Steam achievements that have already synced to your account.
- Only the **one** account (SteamID64) you selected is backed up.
- Vanilla only; Seamless Co-op `.co2` saves are not handled.
- Backups use local disk space — keep an eye on free space for large snapshot counts.
- A snapshot taken **after** a malicious change will itself contain that change, so keep older
  snapshots around; don't rely only on the newest one.
- The background monitor reports exit code 0 for the game session (it detects the game by process
  name and can't recover the game's real exit code). This does not affect Steam or the game.

## Development

Requires stable Rust (edition 2024). This repo cross-builds Windows binaries from Linux with
[`cargo-xwin`](https://github.com/rust-cross/cargo-xwin); on Windows you can use the normal
MSVC target.

```sh
# Portable core: builds and tests on any OS, no GUI/X11 needed
cargo test --lib --no-default-features
cargo clippy --lib --no-default-features -- -D warnings
cargo fmt --check

# Full Windows build (from Linux)
cargo xwin build --release --target x86_64-pc-windows-msvc
# ...or on Windows
cargo build --release
```

Project layout (single package, two targets):

- `src/lib.rs` — portable core (`config`, `discovery`, `snapshot`, `retention`, `launch`,
  `paths`, `monitor`, `platform`, `logging`). Contains all the unit tests.
- `src/main.rs` — dispatches GUI vs `--monitor`.
- `src/gui.rs` — the egui dashboard (behind the `gui` feature so core tests stay GUI-free).

## License

MIT — see [LICENSE](LICENSE).
