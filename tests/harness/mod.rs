//! Shared harness for the build-plugin end-to-end tests.
//!
//! Each test runs the *real* `make` binary against a real makefile with real
//! compiled plugin components, and asserts on the artifacts they publish.
//! Nothing here stubs the host: the point of these tests is that the WIT
//! contract, the wasmtime host, the SDK and the plugins agree, and only an
//! end-to-end run can show that.
//!
//! Lives in a subdirectory so Cargo does not compile it as a test target of
//! its own; each `tests/plugin_*.rs` pulls it in with `mod plugin_common;`.

// Each test binary compiles this module separately and uses a different
// subset of it.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

/// The `make` that loads plugins: a build of
/// [sevki/makers](https://github.com/sevki/makers) with `--features wasmtime`.
/// Set `MAKERS_MAKE` to its path.
pub fn rust_make() -> PathBuf {
    PathBuf::from(std::env::var_os("MAKERS_MAKE").expect(
        "set MAKERS_MAKE to a makers `make` built with `--features wasmtime`",
    ))
}

pub fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// This plugin's compiled component, built by `cargo component build`.
pub fn component() -> PathBuf {
    let path = manifest_dir().join("target/wasm32-wasip1/debug/makers_bazel_plugin.wasm");
    assert!(
        path.exists(),
        "build the plugin first: cargo component build (looked for {})",
        path.display()
    );
    path
}

/// A fresh working directory containing `fixture` as `Makefile` plus empty
/// source files.
pub fn workdir(fixture: &str, sources: &[&str]) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let dir = std::env::temp_dir().join(format!(
        "makers-plugin-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).expect("create workdir");
    std::fs::copy(
        manifest_dir().join("tests/fixtures").join(fixture),
        dir.join("Makefile"),
    )
    .expect("copy fixture");
    for source in sources {
        let path = dir.join(source);
        // Sources may sit in subdirectories: a fixture with targets in more
        // than one package needs them to exercise cross-package labels.
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create source directory");
        }
        std::fs::write(path, "").expect("write source");
    }
    dir
}

/// What one `make` run produced.
pub struct Run {
    pub stdout: String,
    pub stderr: String,
    pub status: std::process::ExitStatus,
    pub dir: PathBuf,
}

impl Run {
    /// Contents of a file the run was expected to produce.
    pub fn artifact(&self, name: &str) -> String {
        let path = self.dir.join(name);
        std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("{}: {e}\nstderr:\n{}", path.display(), self.stderr))
    }

    pub fn produced(&self, name: &str) -> bool {
        self.dir.join(name).exists()
    }
}

/// Run `make -rn` in `dir` with the given plugin-related environment.
pub fn run_make(dir: &Path, env: &[(&str, &str)]) -> Run {
    run_make_with_args(dir, &[], env)
}

/// As [`run_make`], plus extra command-line arguments.
///
/// Exists for command-line variable assignments (`make CC=gcc`), which are a
/// different `$(origin ...)` from the same name arriving through the
/// environment — a distinction the input digest turns on and which no
/// environment-only harness can produce.
pub fn run_make_with_args(dir: &Path, args: &[&str], env: &[(&str, &str)]) -> Run {
    let mut cmd = Command::new(rust_make());
    cmd.args(["--no-print-directory", "-r", "-n", "-f", "Makefile"])
        .args(args)
        .env_remove("MAKEFLAGS")
        .env_remove("GNUMAKEFLAGS")
        .env_remove("MAKEFILES")
        .env_remove("MAKERS_PLUGINS")
        .env_remove("MAKERS_PLUGIN_ARGS")
        .env_remove("MAKERS_PLUGIN_ALLOW")
        .env_remove("MAKERS_PLUGIN_DENY")
        .env_remove("MAKERS_PLUGIN_VERBOSE")
        .env_remove("MAKERS_WASM_EXTENSION")
        .current_dir(dir);
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("spawn make");
    Run {
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        status: out.status,
        dir: dir.to_path_buf(),
    }
}

/// Assert the run succeeded and no plugin blew up unexpectedly.
pub fn assert_clean(run: &Run) {
    assert!(run.status.success(), "make should succeed:\n{}", run.stderr);
    assert!(
        !run.stderr.contains("plugin failed"),
        "no plugin should fail to load:\n{}",
        run.stderr
    );
}
