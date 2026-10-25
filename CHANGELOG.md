# Changelog

Releases before 1.0.7 are listed on the [releases page](https://github.com/handsomefox/eldenring-backuptool/releases).

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
