//! Builds the exact command line that starts an instance.

use std::path::{Path, PathBuf};

use crate::apptype::Strategy;
use crate::exec::CommandSpec;
use crate::instance::{Instance, MARKER_ENV};
use crate::paths::Paths;

/// The programs a launch command runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binaries {
    /// Absolute path of `flatpak`.
    pub flatpak: PathBuf,
    /// Absolute path of bubblewrap, which gives each instance its own app folder.
    pub bwrap: PathBuf,
}

impl Binaries {
    /// The usual locations, shown in previews until the real paths are known.
    pub fn standard() -> Self {
        Self { flatpak: PathBuf::from("/usr/bin/flatpak"), bwrap: PathBuf::from("/usr/bin/bwrap") }
    }
}

/// The full launch command.
///
/// Every instance is isolated the same way, whatever the app: `flatpak` starts inside a
/// private, unprivileged mount namespace (bubblewrap) in which the instance's data folder is
/// mounted over the app's own `~/.var/app/<app-id>`. Flatpak hands the sandbox that folder as
/// usual, so everything the app stores (settings, data, cache, state, lock files, profiles)
/// lands in the instance folder without the app knowing. The rest of the system is unchanged.
pub fn launch_spec(bins: &Binaries, paths: &Paths, instance: &Instance) -> CommandSpec {
    CommandSpec::new(bins.bwrap.display().to_string())
        .args(isolation_args(paths, instance))
        .args([bins.flatpak.display().to_string()])
        .args(run_args(instance))
}

/// The bubblewrap arguments before `flatpak`: keep the whole system and replace only the
/// app's folder with the instance's data folder.
pub fn isolation_args(paths: &Paths, instance: &Instance) -> Vec<String> {
    vec![
        "--dev-bind".into(),
        "/".into(),
        "/".into(),
        "--bind".into(),
        instance.resolved_data_dir(paths).display().to_string(),
        paths.app_var_dir(&instance.app_id).display().to_string(),
        "--".into(),
    ]
}

/// The arguments after `flatpak`. Besides the instance marker and the user's own variables
/// and arguments, a type only adds what lets several copies run side by side with their own
/// window class.
pub fn run_args(instance: &Instance) -> Vec<String> {
    let mut args = vec!["run".to_string(), format!("--env={MARKER_ENV}={}", instance.key())];
    for (k, v) in &instance.env {
        args.push(format!("--env={k}={v}"));
    }
    args.push(instance.app_id.clone());
    let class = &instance.wm_class;
    match instance.strategy {
        Strategy::Electron | Strategy::Chromium => args.push(format!("--class={class}")),
        Strategy::Firefox => {
            args.push("--new-instance".to_string());
            args.push(format!("--name={class}"));
            args.push(format!("--class={class}"));
        }
        Strategy::Generic => {}
    }
    args.extend(instance.extra_args.iter().cloned());
    args
}

/// Subfolders Flatpak creates in an app folder; created up front.
pub const APP_DIRS: [&str; 3] = ["config", "data", "cache"];

/// Folders that must exist before launching.
pub fn dirs_to_create(data_dir: &Path) -> Vec<PathBuf> {
    std::iter::once(data_dir.to_path_buf()).chain(APP_DIRS.iter().map(|d| data_dir.join(d))).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::instance::{DraftContext, InstanceDraft};

    fn instance(app: &str, strategy: Strategy) -> Instance {
        let paths = Paths::for_home("/home/u");
        let mut d = InstanceDraft::new(app, "App", "Work", strategy, &[]);
        d.extra_args = vec!["--extra flag".into()];
        d.env.insert("LANG".into(), "C".into());
        d.validate(DraftContext { paths: &paths, existing: &[], editing: None }).unwrap().instance
    }

    #[test]
    fn every_type_mounts_the_instance_folder_over_the_app_folder() {
        let paths = Paths::for_home("/home/u");
        for strategy in Strategy::ALL {
            let i = instance("com.example.App", strategy);
            let argv = launch_spec(&Binaries::standard(), &paths, &i).argv();
            assert_eq!(
                argv[..9],
                [
                    "/usr/bin/bwrap",
                    "--dev-bind",
                    "/",
                    "/",
                    "--bind",
                    "/home/u/.var/app/com.example.App/instances/work",
                    "/home/u/.var/app/com.example.App",
                    "--",
                    "/usr/bin/flatpak",
                ]
            );
            assert_eq!(argv[9..], run_args(&i)[..]);
            assert!(!argv.iter().any(|a| a.contains("XDG_") || a.contains("profile") || a.contains("user-data-dir")));
        }
    }

    #[test]
    fn type_specific_arguments() {
        let head = |app: &str| {
            vec![
                "run".to_string(),
                format!("--env=MULTIPAK_INSTANCE={app}/work"),
                "--env=LANG=C".into(),
                app.to_string(),
            ]
        };
        let electron = instance("com.slack.Slack", Strategy::Electron);
        let mut want = head("com.slack.Slack");
        want.extend(["--class=multipak-slack-work".into(), "--extra flag".into()]);
        assert_eq!(run_args(&electron), want);

        let chrome = instance("com.google.Chrome", Strategy::Chromium);
        let mut want = head("com.google.Chrome");
        want.extend(["--class=multipak-chrome-work".into(), "--extra flag".into()]);
        assert_eq!(run_args(&chrome), want);

        let firefox = instance("org.mozilla.firefox", Strategy::Firefox);
        let mut want = head("org.mozilla.firefox");
        want.extend([
            "--new-instance".into(),
            "--name=multipak-firefox-work".into(),
            "--class=multipak-firefox-work".into(),
            "--extra flag".into(),
        ]);
        assert_eq!(run_args(&firefox), want);

        let generic = instance("org.gnome.Fractal", Strategy::Generic);
        let mut want = head("org.gnome.Fractal");
        want.push("--extra flag".into());
        assert_eq!(run_args(&generic), want);
        assert_eq!(dirs_to_create(Path::new("/d")).len(), 4);
    }
}
