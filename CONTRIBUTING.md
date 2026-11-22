# Contributing to Multipak

Thanks for helping! Bug reports, app-table additions, translations of error messages into
clearer English, documentation and code are all welcome.

## Project layout

```text
multipak/
├── crates/
│   ├── multipak-core/        library: all logic, no GUI code
│   │   ├── data/app_types.toml   built-in app-type table
│   │   ├── src/
│   │   │   ├── manager.rs    Manager: every user-facing operation
│   │   │   ├── instance.rs   instance model and input validation
│   │   │   ├── launch.rs     launch command for each strategy
│   │   │   ├── desktop.rs    .desktop generation and escaping
│   │   │   ├── apptype.rs    strategies and app-type detection
│   │   │   ├── flatpak.rs    flatpak list/ps/info/kill/override
│   │   │   ├── config.rs     instances.toml, versioning, migration, locking
│   │   │   ├── backup.rs     .tar.zst backup and restore
│   │   │   ├── doctor.rs     checks and one-click fixes
│   │   │   ├── transfer.rs   JSON import/export
│   │   │   ├── icons.rs      icon lookup and badge rendering
│   │   │   ├── capture.rs    hiding windows from screen capture (KWin script + D-Bus helper)
│   │   │   ├── fsutil.rs     disk usage, cache cleanup, guarded deletion
│   │   │   ├── exec.rs       Host trait: commands and /proc, real implementation
│   │   │   └── fake.rs       FakeHost for tests
│   │   └── tests/manager.rs  integration tests with a temporary HOME
│   └── multipak-gui/         the `multipak` binary: egui GUI + clap CLI
│       └── src/
│           ├── main.rs       argument parsing, logging, GUI or CLI
│           ├── cli.rs        subcommands
│           ├── capture_service.rs  D-Bus helper answering the KWin capture script
│           └── gui/
│               ├── model.rs  form state, validation, view models (tested without rendering)
│               ├── worker.rs background job pool
│               ├── app.rs    state and actions
│               ├── views.rs  panels and instance cards
│               └── dialogs.rs modal dialogs
├── packaging/                .desktop file, icon, AppStream metadata, AppImage script
└── .github/                  CI, release workflow, issue and PR templates
```

Principles:

- **All logic lives in `multipak-core`.** The GUI and CLI only collect input and show results.
- **Everything external goes through the `Host` trait** (`exec.rs`). Tests use `FakeHost`
  and a temporary `HOME` (`Paths::for_home`), so they never touch a real Flatpak install.
- **Never build shell strings.** Commands use `std::process::Command` with separate arguments.
- **Write files atomically** with `atomic::write_atomic`.
- **No panics in normal use.** Return a typed `Error` (core) or `anyhow` error (binary) with a
  message a user can act on.
- **The UI thread never blocks.** Anything that runs a command or touches more than a couple
  of files goes through `Worker::spawn` or `Worker::spawn_long`.

## Building and checking

You need Rust (via [rustup](https://rustup.rs); the toolchain is pinned in
`rust-toolchain.toml`) and the system packages listed in the README for your distribution.

```sh
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
cargo run -- --verbose          # GUI
cargo run -- list               # CLI
```

CI runs exactly these checks on every pull request.

To try changes without touching your real setup, point `HOME` (and the XDG variables) at a
scratch folder, and keep `FLATPAK_USER_DIR` pointing at your real user installation so your
apps are still listed:

```sh
mkdir -p /tmp/mp-home
HOME=/tmp/mp-home XDG_CONFIG_HOME= XDG_DATA_HOME= XDG_STATE_HOME= XDG_CACHE_HOME= \
  FLATPAK_USER_DIR=$HOME/.local/share/flatpak cargo run -- list
```

## Adding an app to the app-type table

The table is [`crates/multipak-core/data/app_types.toml`](crates/multipak-core/data/app_types.toml).
Each entry looks like this:

```toml
[[app]]
id = "com.example.App"      # exact Flatpak application ID
kind = "electron"           # electron | chromium | firefox | generic
single_instance = false     # optional: true if a second copy opens a window in the first
note = "Short remark"       # optional: shown under the app's name in the GUI
```

Before opening a pull request:

1. **Test it.** Create two instances with the kind you propose, sign in to different accounts
   (or change a setting) in each, close both, start both again at the same time, and check
   that they stay separate.
2. **Pick the right kind.** Isolation is the same for every kind; the kind only decides the
   extra arguments. Use `electron` for Electron apps and `chromium` for browsers built on
   Chromium (both get `--class`), `firefox` for browsers and mail clients built on Gecko (they
   get `--new-instance`, `--name` and `--class`), and `generic` for everything else. Only add an app if
   automatic detection gets it wrong or cannot tell (check with `multipak apps --detect`).
3. **Mark single-instance apps.** If a second copy only opens a window in the first one,
   set `single_instance = true` so users get a warning.
4. Put the entry in the right section, keep IDs sorted within a section, and run
   `cargo test -p multipak-core` (a test checks that the table parses).

Users can make the same additions locally, without rebuilding, in
`~/.config/multipak/app_types.toml`.

## Writing tests

- Core logic: unit tests next to the code, plus `crates/multipak-core/tests/manager.rs` for
  end-to-end flows with `FakeHost`. `FakeHost::on_with` can imitate side effects (see the
  `flatpak override` handler in the integration tests).
- GUI: put logic in plain structs in `gui/model.rs` and test those; rendering code should stay
  thin.

## Changelog and releases

Add user-facing changes to `CHANGELOG.md` under **Unreleased**. To release:

1. Move the Unreleased entries under a new version heading and update the version in the
   workspace `Cargo.toml` and the AppStream metadata.
2. Commit, tag `vX.Y.Z` and push the tag. The release workflow builds the x86_64 tarball and
   AppImage and attaches them to a GitHub release.

## Code of conduct

Be kind and constructive. Assume good intent, and keep discussions focused on the work.

## License

By contributing, you agree that your contributions are licensed under the MIT license
(see [LICENSE](LICENSE)).
