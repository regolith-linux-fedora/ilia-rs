//! Working out one expression in the libqalculate helper.
//!
//! libqalculate cannot interrupt every calculation (a large factorial runs
//! to the end), so each expression is worked out by a helper process of its
//! own, `ilia-calculator`, which the meson build compiles from
//! `src/calculator-helper.cc`. Dropping an [`Evaluation`] kills its helper at
//! once, and a helper also dies with the launcher, even when the launcher is
//! killed.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use gio::prelude::*;

use super::answer::Value;

const HELPER_NAME: &str = "ilia-calculator";
/// Where `meson install` puts the helper; meson passes it to the crate.
const INSTALLED_HELPER: Option<&str> = option_env!("ILIA_CALCULATOR_HELPER");
/// The installed location for a crate built by cargo alone.
const DEFAULT_HELPER: &str = "/usr/libexec/ilia-calculator";

/// One expression being worked out. Dropping it abandons the work.
pub struct Evaluation {
    helper: gio::Subprocess,
    cancellable: gio::Cancellable,
}

impl Evaluation {
    /// Starts working out `expression`, written by
    /// [`translate`](super::expression::translate). `done` runs on the main
    /// loop with the value, or `None` when the expression has none; it never
    /// runs once the evaluation is dropped.
    pub fn start(
        expression: &str,
        done: impl FnOnce(Option<Value>) + 'static,
    ) -> Result<Self, glib::Error> {
        let launcher = gio::SubprocessLauncher::new(
            gio::SubprocessFlags::STDIN_PIPE
                | gio::SubprocessFlags::STDOUT_PIPE
                | gio::SubprocessFlags::STDERR_SILENCE,
        );
        // The helper keeps this process's environment: it reads the locale's
        // decimal mark from it, then works in the C locale.
        let helper_path = helper_path();
        // The helper ends itself when this process is gone; it checks that
        // its parent is still this process once it has asked for that.
        let parent = std::process::id().to_string();
        let helper = launcher.spawn(&[helper_path.as_os_str(), OsStr::new(&parent)])?;
        let cancellable = gio::Cancellable::new();
        helper.communicate_utf8_async(
            Some(expression.to_owned()),
            Some(&cancellable),
            move |result| {
                // A cancelled evaluation was dropped: nobody waits for it.
                if let Err(error) = &result {
                    if error.matches(gio::IOErrorEnum::Cancelled) {
                        return;
                    }
                }
                let value = result
                    .ok()
                    .and_then(|(stdout, _)| stdout)
                    .and_then(|stdout| Value::parse(&stdout));
                done(value);
            },
        );
        Ok(Self {
            helper,
            cancellable,
        })
    }
}

impl Drop for Evaluation {
    fn drop(&mut self) {
        self.cancellable.cancel();
        // Does nothing once the helper has finished.
        self.helper.force_exit();
    }
}

/// The helper next to the launcher (in the build tree), else the installed
/// one.
fn helper_path() -> PathBuf {
    let beside_launcher = std::env::current_exe()
        .ok()
        .and_then(|launcher| launcher.parent().map(|dir| dir.join(HELPER_NAME)));
    match beside_launcher {
        Some(path) if path.is_file() => path,
        _ => Path::new(INSTALLED_HELPER.unwrap_or(DEFAULT_HELPER)).to_path_buf(),
    }
}
