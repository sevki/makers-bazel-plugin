//! End-to-end tests for `bazel-export` in `mode=kbuild`, the build that can
//! take on the Linux kernel.
//!
//! The fixture is shaped like Kbuild: a top-level make that starts a second
//! make for one directory, and commands built by make functions from `$@`
//! and `$<`. See `tests/fixtures/kbuild_top.mk`. Requires the kbuild
//! component (`cargo component build --features kbuild --target-dir
//! target-kbuild`) and `MAKERS_MAKE`, as the other suite does.

mod harness;

use harness::{assert_clean, kbuild_component, run_make_with_args, workdir, Run};
use std::fs;

/// Build the fixture's tree and run the plugin over it.
fn kbuild_run(grants: &str) -> Run {
    let plugin = kbuild_component();
    let dir = workdir("kbuild_top.mk", &["main.c", "shared.h", "sub/a.c"]);
    fs::write(dir.join("main.c"), "#include \"gen.h\"\n").expect("write main.c");
    fs::copy(
        harness::manifest_dir().join("tests/fixtures/kbuild_sub.mk"),
        dir.join("sub.mk"),
    )
    .expect("copy the sub-make");
    let spec = format!("bazel={}", plugin.display());
    let args = format!(
        "bazel:mode=kbuild;bazel:goals=all;bazel:outs=app;bazel:out.build-files={}",
        dir.display()
    );
    // `all` on the command line: the root is recognised by the goals it was
    // given, which `bazel:goals` names.
    run_make_with_args(
        &dir,
        &["all"],
        &[
            ("MAKERS_PLUGINS", &spec),
            ("MAKERS_PLUGIN_ARGS", &args),
            ("MAKERS_PLUGIN_ALLOW", grants),
        ],
    )
}

const GRANTS: &str = "bazel:expand-variables,read-file-content";

/// The fragment a given make wrote, by the directory it was building.
fn fragment(run: &Run, prefix: &str) -> String {
    let dir = run.dir.join("kbuild");
    let mut hits: Vec<_> = fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("{}: {e}\n{}", dir.display(), run.stderr))
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().starts_with(prefix))
        .collect();
    assert_eq!(hits.len(), 1, "one fragment starts with `{prefix}`:\n{}", run.stderr);
    fs::read_to_string(hits.remove(0).path()).expect("read fragment")
}

/// The command in the rule is the one make would run for that target, with
/// `$@` and `$<` filled in by the host, and without Kbuild's `fixdep` step.
#[test]
fn a_rule_carries_the_expanded_command_without_fixdep() {
    let run = kbuild_run(GRANTS);
    assert_clean(&run);
    let root = run.artifact("kbuild/rules.bzl");
    assert!(root.contains("kbuild_declare"), "{root}");

    let sub = fragment(&run, "sub_");
    assert!(
        sub.contains("\"cc -c -o sub/a.o sub/a.c\""),
        "`$(call rule_cc)` expands for sub/a.o:\n{sub}"
    );
    assert!(
        !sub.contains("fixdep"),
        "the dependency-recording step is cut:\n{sub}"
    );
}

/// What the top-level make starts is found from its recursive command line
/// and loaded, so the root reaches every directory's rules.
#[test]
fn the_root_loads_the_make_it_starts() {
    let run = kbuild_run(GRANTS);
    assert_clean(&run);
    let build = run.artifact("BUILD.bazel");
    assert!(build.contains("name = \"kernel\""), "{build}");
    assert!(build.contains("\"app\""), "outs are gathered:\n{build}");

    let top = fragment(&run, "Makefile_");
    let sub = fragment(&run, "sub_");
    assert!(sub.contains("def kbuild_sub_"), "{sub}");
    assert!(
        top.contains("load(\"//:kbuild/sub_"),
        "the top-level fragment loads the sub-make's:\n{top}"
    );
}

/// Prerequisites become inputs, on-disk files and other makes' outputs alike,
/// and a thin archive is marked so its members travel with it.
#[test]
fn prerequisites_are_inputs_and_thin_archives_are_marked() {
    let run = kbuild_run(GRANTS);
    assert_clean(&run);
    let top = fragment(&run, "Makefile_");
    assert!(top.contains("\"main.c\"") && top.contains("\"shared.h\""), "{top}");
    assert!(
        top.contains("\"sub/lib.a\""),
        "the archive another make builds is an input of the link:\n{top}"
    );

    let sub = fragment(&run, "sub_");
    assert!(sub.contains("thin = True"), "`ar ...T` makes a thin archive:\n{sub}");
}

/// Without the grant the plugin cannot expand a recipe, and says so rather
/// than writing commands that are not make's.
#[test]
fn without_expand_variables_no_rules_are_written_from_unexpanded_recipes() {
    let run = kbuild_run("bazel:read-file-content");
    assert!(
        !run.stderr.contains("panicked"),
        "a withheld capability is reported, not a crash:\n{}",
        run.stderr
    );
    let fragments = run.dir.join("kbuild");
    let rules = fs::read_dir(&fragments)
        .map(|d| {
            d.filter_map(Result::ok)
                .filter(|e| e.file_name() != "rules.bzl")
                .filter_map(|e| fs::read_to_string(e.path()).ok())
                .any(|text| text.contains("cmds = ["))
        })
        .unwrap_or(false);
    assert!(!rules, "no rule carries a command:\n{}", run.stderr);
}

/// A recipe line that runs a command and then starts a make is one action,
/// make included, rather than a recursion with the command dropped.
#[test]
fn a_line_that_runs_a_command_then_a_make_is_kept_whole() {
    let run = kbuild_run(GRANTS);
    assert_clean(&run);
    let top = fragment(&run, "Makefile_");
    assert!(
        top.contains("out = \"linked\""),
        "`linked` is built by an action:\n{top}"
    );
    assert!(
        top.contains("cc -o linked main.o; ") && top.contains("sub.mk obj=post"),
        "the command and the make both run in it:\n{top}"
    );
}

/// A header the compile includes, that is not on disk yet, may be generated
/// by a target elsewhere. The scan cannot know, so the action lists it as a
/// candidate and Bazel, which has every target, decides.
#[test]
fn an_include_that_is_not_there_yet_is_a_candidate_source() {
    let run = kbuild_run(GRANTS);
    assert_clean(&run);
    let top = fragment(&run, "Makefile_");
    assert!(
        top.contains("maybe = [") && top.contains("\"gen.h\""),
        "the generated header is a candidate of the compile that includes it:\n{top}"
    );
    assert!(top.contains("out = \"gen.h\""), "and it has its own rule:\n{top}");
    assert!(
        top.contains("def kbuild_") && top.contains("return specs"),
        "a fragment returns its targets for `kbuild_declare`:\n{top}"
    );
    let rules = run.artifact("kbuild/rules.bzl");
    assert!(rules.contains("def kbuild_declare"), "{rules}");
}
