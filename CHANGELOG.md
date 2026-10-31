# Changelog

Releases before 1.0.7 are listed on the [releases page](https://github.com/handsomefox/eldenring-backuptool/releases).

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
