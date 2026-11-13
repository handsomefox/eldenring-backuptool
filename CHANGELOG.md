# Changelog

Releases before 1.0.7 are listed on the [releases page](https://github.com/handsomefox/eldenring-backuptool/releases).

## 1.2.8

- The background monitor keeps running until the game exits even when it cannot back up, such as
  before an account is chosen. It used to exit once the launcher did, so Steam saw the game as
  closed while it still ran.
- A backup from the window and one from the background monitor no longer prune each other's new
  backup. With **Keep newest** set to 1 they could leave no backup at all.
- **Back up now** finds the save files again before copying, after any backup it has to wait for. A
  `ER0000.sl2.bak` the game wrote after the list was last refreshed was left out of the backup.
- The stage checkboxes on **Settings** take effect only when you press **Apply**, like the other
  settings. They changed the saved settings when you chose an account.
- Settings report a failure when the `%LOCALAPPDATA%` folder cannot be found, instead of saying
  **Settings saved.**
- Closing the window from the taskbar while it is minimized during a backup brings it back with
  "Wait for the backup to finish before closing.", as closing it normally does.
- The background monitor no longer waits for a backup from the window before starting the game. If
  one is running, it skips that snapshot.
- A `snapshots` folder inside the backup destination that is a link or junction is refused: backups
  fail with that reason and retention lists nothing there. If you linked it to another drive on
  purpose, point the backup destination at the real folder instead.
- Updates installed from this version on are safer with two copies of the app open. A copy that
  starts while another installs an update no longer deletes that download, and a copy no longer
  installs an older update over a newer one that another copy installed. Files you keep beside the
  app, and logs you renamed to keep, are no longer deleted at startup.

## 1.2.7

- Keep the backup that was just made when the clock was set back since an earlier one. Retention
  sorted it before the older backups and, with a low limit, deleted it.
- Show **Settings saved.** only when the settings were written. A failed save showed its error for
  a moment, and the dashboard kept the unsaved values while the background monitor read the old
  ones. A failed save now keeps its error and puts the previous settings back. Choosing an account
  works the same way.
- Opening a second copy of the dashboard while the first has an update waiting no longer deletes
  the first copy's download.

## 1.2.6

- Hash every backup again before deciding whether a new one is a duplicate, and which old ones to
  delete. A damaged backup whose size and date had not changed could be trusted from earlier in
  the session, stop a new backup, and keep its slot while a good older backup was deleted.

## 1.2.5

- **Check for updates**, which replaces **App updates…**, checks GitHub right away as it opens the
  update window. The old button only opened the window.
- Write the log to one file per day, `eldenring-backuptool-<date>.log`, and keep the last seven,
  in place of a single `app.log`. The files stay in the same `logs` folder, and a crash is logged
  with its backtrace.
- Ask Windows where `%APPDATA%` and `%LOCALAPPDATA%` are, and fall back to the environment
  variables only when it cannot answer.
- Get the self-updater, the update window, logging, and the folder lookups from [handsomefox/app-internals](https://github.com/handsomefox/app-internals),
  which all five apps share, instead of a copy inside this app.

## 1.2.4

- Get the self-updater from its own repository, [handsomefox/app-update](https://github.com/handsomefox/app-update), instead of a copy
  inside this app. Checking for, downloading, and installing an update work as before.

## 1.2.3

- Check each snapshot against its recorded hash before the **Backups** tab lists it and before retention counts it. A damaged snapshot is left off the list, does not take one of the snapshots you keep, is never deleted, and is named in the log. Each snapshot is checked once per session, and again if its file changes.
- Show the notice that an unreadable settings file was reset. The reset happened, but the notice never appeared.
- Remove the temporary files that a failed backup or a failed settings save left behind. A failed backup could leave a full copy of the save in the backup folder.
- Log each snapshot that retention fails to remove.
- Say in **Settings** that intervals below 60 seconds are rejected.
- Refuse an app update whose checksum on GitHub cannot be read, instead of skipping that check.
- Retry putting the current version back when installing an app update fails.

## 1.2.2

- New installs check for a new version each time the Save Guard window opens. Existing installs keep their setting. To change it, open the **Help** tab > **App updates**.
- Keep a found update visible after a restart. With a check period of a day or longer, a restart hid the new version until the next check was due.
- Remove the line in **App updates** that said no check had run, which appeared beside the time of the last check.

## 1.2.1

- Show a Save Guard icon in the title bar and the taskbar, where egui's default icon appeared before. The `.exe` file keeps the generic Windows icon in Explorer.
- Update eframe and egui to 0.37. Buttons are slightly wider.
- Update flate2 to 1.1.10, serde_json to 1.0.154, sha2 to 0.11.1, and zip to 9.2.0.

## 1.2.0

- Save Guard can now update itself from its GitHub releases. It checks once a day, and when a new version is out, a chip in the header names it. Click the chip to read what changed since your version, then download the update, skip that version, or leave it for later. The download must match its published SHA-256 checksums before it replaces the app, and a restart finishes the install. A restart waits for a running backup. To check more or less often, or never, open the **Help** tab > **App updates**. The background monitor that Steam starts never checks.
- Getting 1.2.0 itself is still a manual download. Later versions install from inside the app.

## 1.1.2

- Replace the yanked clipboard-win 5.4.1 with 5.4.3.

## 1.1.1

- Fix the Dashboard status dot and the Steam menu path on the Help tab, which showed as empty boxes.
- Update zip to 9.0.5, which fixes RustSec security advisories.

## 1.1.0

- Update egui and eframe to 0.36.2.
- Build with Rust 1.98.1.

## 1.0.7

- Upgrading from 1.0.6 or earlier breaks the Steam launch option until you copy it again. The
  executable is now `eldenring-backuptool.exe`, not `Elden Ring Backuptool.exe`, and the launch
  option still names the old file. Put the new executable where the old one was, open the Help
  tab, click **Copy launch option**, and paste the result into Steam. Until then, Elden Ring
  does not start from Steam.
- Ship `eldenring-backuptool-<version>-windows-x86_64.zip`, which holds a folder of the same
  name with `eldenring-backuptool.exe`, `README.md`, and `LICENSE` in it, beside a `SHA256SUMS`
  file. The executable was also attached on its own before.
