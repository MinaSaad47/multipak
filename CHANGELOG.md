# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses
[Semantic Versioning](https://semver.org/).

## [Unreleased]

### Changed

- One isolation method for every app: each instance's data folder is mounted over the app's own
  `~/.var/app/<app-id>` folder in a private mount namespace (bubblewrap), so everything the app
  stores stays in the instance, whatever the app. Instances no longer rely on
  `XDG_CONFIG_HOME`/`XDG_DATA_HOME`/`XDG_CACHE_HOME`, `--user-data-dir` or `-profile`, which
  many apps ignore or only partly honour. The app type now only adds the window-class and
  side-by-side arguments.
- Data folders of Electron, Chromium and Firefox instances created by 0.1 are moved into the
  new layout once (Firefox profiles become the instance's default profile), and existing
  launchers are rewritten, when Multipak starts or the instance is launched.

### Added

- Hide instances, or Multipak's own window, from screenshots, screen recordings and screen
  sharing on KDE Plasma. Instances are matched per process (a KWin script plus an on-demand
  D-Bus helper), so only the chosen instance is hidden even when every copy of the app shares
  one window class. Available in the instance dialog, in Settings, and as
  `multipak capture on|off` and `multipak create --hide-from-capture`. The repair tool detects
  a missing, outdated or leftover helper.

## [0.1.0] - 2026-11-22

First release.

### Added

- Create, edit, duplicate, launch, stop and delete isolated instances of Flatpak apps, each
  with its own data folder, launcher, window class and optional badge icon.
- Launch strategies for Electron apps, Chromium-based browsers, Firefox-based browsers and a
  generic XDG-folder strategy, with automatic detection from a built-in app table, the app's
  Flatpak base app and its files. The table can be extended in `~/.config/multipak/app_types.toml`.
- Warnings for apps that probably allow only one running copy.
- Custom data folders with an explicit, reversible `flatpak override --user --filesystem`.
- Autostart entries, running status from `flatpak ps`, disk usage, clearing caches, and
  opening the data folder.
- Backup and restore of single instances as `.tar.zst`, with progress and cancellation.
- Repair ("doctor") for missing or outdated launchers, missing data folders, lost folder
  access, uninstalled apps and leftover files, with one-click fixes.
- Import and export of the setup as JSON, optionally with data backups.
- egui GUI (glow renderer, Wayland and X11) with system/light/dark themes, keyboard shortcuts
  and AccessKit support.
- CLI: `list`, `apps`, `create`, `launch`, `stop`, `delete`, `backup`, `restore`, `doctor`,
  `export`, `import`.
- AppImage and release tarball.

[Unreleased]: https://github.com/MinaSaad47/multipak/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/MinaSaad47/multipak/releases/tag/v0.1.0
