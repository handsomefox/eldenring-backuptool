# Elden Ring Save Guard

Save Guard keeps automatic, versioned backups of your vanilla Elden Ring save. It runs in the background every time you play, so there is always a known-good save to fall back on.

If your save is already ruined and you need it back now, go to [restore a save](#restore-a-save).

## What it does

You launch Elden Ring through Steam as usual. Save Guard copies your save before you play, again whenever the save changes during the session, and once more after you quit. It verifies each copy against the original bytes. Every copy is a separate, timestamped snapshot on your own PC.

## Why it exists

Players have reported malicious multiplayer users altering other players' saves: forcing boss deaths, pushing story progression, firing cutscenes, teleporting characters, unlocking achievements, and leaving world state broken. Ordinary failures ruin saves too. A crash, a power cut, or a bad sector can truncate one.

Steam Cloud does not cover this, because it synchronizes rather than keeping history. If your save is damaged, the damaged version becomes the newest copy and syncs everywhere. There is no button to go back a few versions.

Save Guard cannot stop an attack or a corruption while it is happening. Nothing running on your PC can. What it gives you is a history of good saves, so you can recover afterward.

## What is supported

- Windows 10 and Windows 11.
- The Steam version of Elden Ring.
- Vanilla saves: `ER0000.sl2` and its `ER0000.sl2.bak`.
- No administrator rights needed.

Seamless Co-op `.co2` saves are not handled in this release.

Save Guard does not modify the game, inject code, read game memory, or interfere with Easy Anti-Cheat. It reads and copies your save files, and nothing else. For the details, see [SAFETY.md](SAFETY.md).

## Install

1. Download the latest release ZIP. It holds one folder with `eldenring-backuptool.exe` in it. Put `eldenring-backuptool.exe` in a folder you will not move later, such as `C:\Tools\EldenRingSaveGuard\`. Do not leave it in Downloads or a temp folder. Step 6 points a Steam launch option at this exact path, so if you move, rename, or delete the file afterward, Elden Ring will not launch until you fix it. If you do move it, copy the launch option from the Help tab again and paste the new one into Steam.
2. Run `eldenring-backuptool.exe`.
3. On the Dashboard tab, pick the Steam account whose save you want to protect. If you have only one, it is already selected.
4. If you want the backups somewhere other than the default, change the destination on the Settings tab.
5. Open the Help tab and click **Copy launch option**.
6. In Steam, open **Elden Ring > Properties > General > Launch Options** and paste it there.
7. Launch Elden Ring normally. Backups now happen on their own.

The launch option looks like this, with your own path:

```
"C:\Tools\EldenRingSaveGuard\eldenring-backuptool.exe" --monitor %command%
```

`%command%` is Steam's own launch command. Save Guard passes it through untouched, so Easy Anti-Cheat and online play work exactly as they did before.

To upgrade, replace `eldenring-backuptool.exe` in that folder with the one from the new release. The launch option keeps working, because the path stays the same.

Versions 1.0.6 and earlier were called `Elden Ring Backuptool.exe`. If you upgrade from one of them, put `eldenring-backuptool.exe` where the old file was, then copy the launch option from the Help tab and paste it into Steam again. Until you do, Elden Ring does not start from Steam.

## Confirm it works

The Dashboard shows the selected account, the save file and its size, the backup destination, how many snapshots are stored, and when the last backup ran. Play a session, or click **Back up now**, and the status changes to "Protected — backups exist".

## Multiple Steam accounts

`%APPDATA%\EldenRing` can hold several numbered folders. Multiple Steam accounts, Family Sharing, old copies, and another person on the same PC all produce one. Save Guard lists the folders that actually contain a save and lets you choose. It does not back up every folder it finds, and it does not guess from modification time alone. Switching the selected account never merges or deletes another account's snapshots.

## Restore a save

Restoring is a manual step, so that nothing overwrites a live save behind your back.

1. Close Elden Ring completely, and exit Steam as well.
2. In the app, open the Backups tab, choose a snapshot, and click **Open** to reveal its folder in Explorer.
3. Double-click `save.zip`. Windows opens it like a folder. Copy the `.sl2` file, and the `.sl2.bak` file if there is one, into `%APPDATA%\EldenRing\<your-id>\`, replacing what is there.
4. Start Steam again. If Steam Cloud reports a conflict, choose the local copy, which is the one you just restored, rather than the newer cloud version. The wording of that prompt changes between Steam versions, so pick whichever option keeps your local files.

## Where backups go

The default destination is `Documents\Game Save Backups\Elden Ring\<SteamID64>\snapshots\`, and you can change it on the Settings tab.

Each snapshot folder is named for its UTC timestamp and a short content hash. It holds `save.zip` with the compressed save files, plus a small `metadata.json` recording hashes of both the original saves and the finished archive. Elden Ring saves are mostly empty space and compress well: the 27.6 MiB example save in `example-save/` shrinks about 18 times. Save Guard re-checks an existing snapshot before it displays it, compares it for deduplication, or prunes it.

## Uninstall

1. In Steam, open **Elden Ring > Properties > General > Launch Options** and clear the field. Do this first. If you delete the app while the launch option still points at it, Steam tries to run a missing file and the game will not start.
2. Delete the extracted application folder.
3. Your backups stay where they are. To remove them, delete the backup destination folder yourself.
4. To remove settings and logs as well, delete `%LOCALAPPDATA%\EldenRingSaveGuard\`.

## Limitations

- Save Guard does not prevent cheating or corruption. It gives you recovery points.
- It cannot undo Steam achievements that have already synced to your account.
- It backs up the one account, by SteamID64, that you selected.
- It handles vanilla saves only, not Seamless Co-op `.co2` saves.
- Snapshots use local disk space, so watch your free space if you keep many.
- A snapshot taken after a malicious change contains that change. Keep older snapshots, and do not rely on the newest one alone.
- The background monitor always reports exit code 0 for the game session. It detects the game by process name and cannot read the game's real exit code. Steam and the game are unaffected.

## Development

The toolchain is pinned to Rust 1.97.1 in `rust-toolchain.toml`, and the package uses edition 2024. This repo cross-builds Windows binaries from Linux with [`cargo-xwin`](https://github.com/rust-cross/cargo-xwin). On Windows, use the normal MSVC target.

CI runs these three commands on Ubuntu and the last two on Windows, along with `cargo audit` and `cargo machete`:

```sh
cargo fmt --all -- --check
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

The GUI sits behind the default `gui` feature. To build and test the portable core without pulling in eframe, egui, and X11:

```sh
cargo test --lib --no-default-features
cargo clippy --lib --no-default-features -- -D warnings
```

To build the Windows app:

```sh
cargo xwin build --release --target x86_64-pc-windows-msvc   # from Linux
cargo build --release                                        # on Windows
```

To pack the same archive and `SHA256SUMS` a release publishes, under `dist/`:

```sh
bash scripts/package-windows.sh
```

The packaging script needs `cargo-xwin` 0.23.1, `jq`, `zip`, `unzip`, and GNU `sha256sum`. It checks the archive contents and the checksums before it reports success.

The package has one library and one binary:

- `src/lib.rs` is the portable core (`config`, `discovery`, `snapshot`, `retention`, `launch`, `paths`, `monitor`, `platform`, `logging`) and holds the unit tests.
- `src/main.rs` dispatches between the GUI and `--monitor`.
- `src/gui.rs` is the egui dashboard, behind the `gui` feature so the core tests stay GUI-free.

## License

MIT. See [LICENSE](LICENSE).
