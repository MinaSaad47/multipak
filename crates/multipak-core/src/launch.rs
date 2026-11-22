//! Builds the exact `flatpak run` command line for an instance.

use std::path::{Path, PathBuf};

use crate::apptype::Strategy;
use crate::exec::CommandSpec;
use crate::instance::{Instance, MARKER_ENV};

/// The arguments after the `flatpak` binary for launching `instance` with data in `data_dir`.
pub fn run_args(instance: &Instance, data_dir: &Path) -> Vec<String> {
    let dir = data_dir.display().to_string();
    let mut args = vec!["run".to_string(), format!("--env={MARKER_ENV}={}", instance.key())];
    match instance.strategy {
        Strategy::Electron => args.push(format!("--env=XDG_CONFIG_HOME={dir}")),
        Strategy::Generic => {
            for (var, sub) in generic_dirs() {
                args.push(format!("--env={var}={}", data_dir.join(sub).display()));
            }
        }
        Strategy::Chromium | Strategy::Firefox => {}
    }
    for (k, v) in &instance.env {
        args.push(format!("--env={k}={v}"));
    }
    args.push(instance.app_id.clone());
    let class = &instance.wm_class;
    match instance.strategy {
        Strategy::Electron => args.push(format!("--class={class}")),
        Strategy::Chromium => {
            args.push(format!("--user-data-dir={dir}"));
            args.push(format!("--class={class}"));
        }
        Strategy::Firefox => {
            args.extend(["-profile".to_string(), dir, "--new-instance".to_string()]);
            args.push(format!("--name={class}"));
            args.push(format!("--class={class}"));
        }
        Strategy::Generic => {}
    }
    args.extend(instance.extra_args.iter().cloned());
    args
}

/// The full launch command, using the absolute path of the `flatpak` binary.
pub fn launch_spec(flatpak: &Path, instance: &Instance, data_dir: &Path) -> CommandSpec {
    CommandSpec::new(flatpak.display().to_string()).args(run_args(instance, data_dir))
}

/// Subfolders used by the generic strategy.
pub fn generic_dirs() -> [(&'static str, &'static str); 3] {
    [("XDG_CONFIG_HOME", "config"), ("XDG_DATA_HOME", "data"), ("XDG_CACHE_HOME", "cache")]
}

/// Folders that must exist before launching.
pub fn dirs_to_create(instance: &Instance, data_dir: &Path) -> Vec<PathBuf> {
    let mut dirs = vec![data_dir.to_path_buf()];
    if instance.strategy == Strategy::Generic {
        dirs.extend(generic_dirs().iter().map(|(_, sub)| data_dir.join(sub)));
    }
    dirs
}

/// Profile name registered in Firefox's `profiles.ini` (no spaces allowed).
pub fn firefox_profile_name(instance: &Instance) -> String {
    format!("multipak-{}", instance.slug)
}

/// `flatpak run <app> -CreateProfile "<name> <dir>"`.
pub fn create_profile_spec(flatpak: &Path, instance: &Instance, data_dir: &Path) -> CommandSpec {
    CommandSpec::new(flatpak.display().to_string()).args([
        "run".to_string(),
        instance.app_id.clone(),
        "-CreateProfile".to_string(),
        format!("{} {}", firefox_profile_name(instance), data_dir.display()),
    ])
}

/// Whether a Firefox profile already exists in `data_dir`.
pub fn firefox_profile_exists(data_dir: &Path) -> bool {
    ["times.json", "prefs.js", "compatibility.ini"].iter().any(|f| data_dir.join(f).exists())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::instance::{DraftContext, InstanceDraft};
    use crate::paths::Paths;

    fn instance(app: &str, strategy: Strategy) -> Instance {
        let paths = Paths::for_home("/home/u");
        let mut d = InstanceDraft::new(app, "App", "Work", strategy, &[]);
        d.extra_args = vec!["--extra flag".into()];
        d.env.insert("LANG".into(), "C".into());
        d.validate(DraftContext { paths: &paths, existing: &[], editing: None }).unwrap().instance
    }

    const DIR: &str = "/home/u/.var/app/x/instances/work";

    #[test]
    fn electron() {
        let i = instance("com.slack.Slack", Strategy::Electron);
        assert_eq!(
            run_args(&i, Path::new(DIR)),
            vec![
                "run",
                "--env=MULTIPAK_INSTANCE=com.slack.Slack/work",
                &format!("--env=XDG_CONFIG_HOME={DIR}"),
                "--env=LANG=C",
                "com.slack.Slack",
                "--class=multipak-slack-work",
                "--extra flag",
            ]
        );
    }

    #[test]
    fn chromium() {
        let i = instance("com.google.Chrome", Strategy::Chromium);
        assert_eq!(
            run_args(&i, Path::new(DIR)),
            vec![
                "run",
                "--env=MULTIPAK_INSTANCE=com.google.Chrome/work",
                "--env=LANG=C",
                "com.google.Chrome",
                &format!("--user-data-dir={DIR}"),
                "--class=multipak-chrome-work",
                "--extra flag",
            ]
        );
    }

    #[test]
    fn firefox() {
        let i = instance("org.mozilla.firefox", Strategy::Firefox);
        assert_eq!(
            run_args(&i, Path::new(DIR)),
            vec![
                "run",
                "--env=MULTIPAK_INSTANCE=org.mozilla.firefox/work",
                "--env=LANG=C",
                "org.mozilla.firefox",
                "-profile",
                DIR,
                "--new-instance",
                "--name=multipak-firefox-work",
                "--class=multipak-firefox-work",
                "--extra flag",
            ]
        );
        let spec = create_profile_spec(Path::new("/usr/bin/flatpak"), &i, Path::new(DIR));
        assert_eq!(
            spec.argv(),
            vec!["/usr/bin/flatpak", "run", "org.mozilla.firefox", "-CreateProfile", &format!("multipak-work {DIR}")]
        );
    }

    #[test]
    fn generic() {
        let i = instance("org.gnome.Fractal", Strategy::Generic);
        assert_eq!(
            run_args(&i, Path::new(DIR)),
            vec![
                "run",
                "--env=MULTIPAK_INSTANCE=org.gnome.Fractal/work",
                &format!("--env=XDG_CONFIG_HOME={DIR}/config"),
                &format!("--env=XDG_DATA_HOME={DIR}/data"),
                &format!("--env=XDG_CACHE_HOME={DIR}/cache"),
                "--env=LANG=C",
                "org.gnome.Fractal",
                "--extra flag",
            ]
        );
        assert_eq!(dirs_to_create(&i, Path::new(DIR)).len(), 4);
    }

    #[test]
    fn profile_detection() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!firefox_profile_exists(dir.path()));
        std::fs::write(dir.path().join("times.json"), "{}").unwrap();
        assert!(firefox_profile_exists(dir.path()));
    }
}
