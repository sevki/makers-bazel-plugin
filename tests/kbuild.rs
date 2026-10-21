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
    let dir = workdir("kbuild_top.mk", &["main.c", "shared.h", "sub/a.c", "plain.c", "miss.c", "zdir/ord.h", "adir/ord.h", "run.sh", "list.txt"]);
    fs::write(dir.join("main.c"), "#include \"gen.h\"\n").expect("write main.c");
    fs::write(dir.join("miss.c"), "#include \"nothere.h\"\n").expect("write miss.c");
    fs::write(dir.join("ord.c"), "#include <ord.h>\n").expect("write ord.c");
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

/// An implicit rule's prerequisites are inputs too: `img-out.gen` is built
/// from `preout.x` by `img-%.gen: pre%.x`. (In the kernel's vdso Makefile the
/// host reports such an edge with an empty as-written name, and the plugin
/// takes the file from the edge's target; this fixture's edges are named.)
#[test]
fn a_prerequisite_from_an_implicit_rule_is_an_input() {
    let run = kbuild_run(GRANTS);
    assert_clean(&run);
    let top = fragment(&run, "Makefile_");
    let at = top.find("out = \"img-out.gen\"").unwrap_or_else(|| panic!("no rule for img-out.gen:\n{top}"));
    let rule = &top[at..];
    let end = rule.find("cmds = [").expect("rule has commands");
    assert!(
        rule[..end].contains("\"preout.x\""),
        "`preout.x` is an input of `img-out.gen`:\n{}",
        &rule[..end]
    );
}

/// A make that a command starts rather than a recipe's recursive line (the
/// fixture's `linked` runs `$(MAKE) -f sub.mk obj=post`, as Kbuild's link runs
/// a make for `init/version-timestamp.o`) is named by no parent. The root
/// loads it all the same, or its rules would never be declared.
#[test]
fn a_make_a_command_starts_is_loaded_by_the_root() {
    let run = kbuild_run(GRANTS);
    assert_clean(&run);
    let build = run.artifact("BUILD.bazel");
    assert!(
        build.contains("load(\"//:kbuild/post_") && build.contains(" + kbuild_post_"),
        "the root declares the make `linked` started:\n{build}"
    );
    let list = run.artifact("kbuild/makes.list");
    assert!(list.contains("post_"), "the make registered itself:\n{list}");
}

/// Two targets one recipe makes (`%.c1 %.h1: %.src`) each get an action that
/// runs it, and neither is an input of the other.
#[test]
fn targets_one_recipe_makes_each_get_an_action_and_are_not_inputs_of_each_other() {
    let run = kbuild_run(GRANTS);
    assert_clean(&run);
    let top = fragment(&run, "Makefile_");
    let rule = |out: &str| -> String {
        let at = top
            .find(&format!("out = \"{out}\""))
            .unwrap_or_else(|| panic!("no rule for {out}:\n{top}"));
        let rest = &top[at..];
        rest[..rest.find("cmds = [").expect("rule has commands")].to_string()
    };
    let c = rule("grp.c1");
    let h = rule("grp.h1");
    assert!(c.contains("\"grp.src\"") && h.contains("\"grp.src\""), "{c}\n{h}");
    assert!(!c.contains("grp.h1"), "the header is not an input of the source:\n{c}");
    assert!(!h.contains("grp.c1"), "the source is not an input of the header:\n{h}");
}

/// A compile of a generated source, or of one that includes a file that is
/// not there yet, cannot be scanned for includes, so it depends on the tree's
/// headers instead; a compile of a source whose includes all exist does not.
#[test]
fn a_compile_of_a_generated_source_depends_on_the_tree_s_headers() {
    let run = kbuild_run(GRANTS);
    assert_clean(&run);
    let top = fragment(&run, "Makefile_");
    let rule = |out: &str| -> String {
        let at = top
            .find(&format!("out = \"{out}\""))
            .unwrap_or_else(|| panic!("no rule for {out}:\n{top}"));
        let rest = &top[at..];
        rest[..rest.find("specs.append").unwrap_or(rest.len())].to_string()
    };
    assert!(rule("gen2.o").contains("infra = True"), "{}", rule("gen2.o"));
    assert!(rule("miss.o").contains("infra = True"), "miss.c includes a file that is not there:\n{}", rule("miss.o"));
    assert!(!rule("plain.o").contains("infra = True"), "{}", rule("plain.o"));
}

/// An include is found in the first search directory that has it, in the
/// order of the command line, as the compiler does.
#[test]
fn an_include_is_found_in_command_line_order() {
    let run = kbuild_run(GRANTS);
    assert_clean(&run);
    let top = fragment(&run, "Makefile_");
    let at = top.find("out = \"ord.o\"").unwrap_or_else(|| panic!("no rule for ord.o:\n{top}"));
    let rest = &top[at..];
    let rule = &rest[..rest.find("cmds = [").expect("rule has commands")];
    assert!(rule.contains("\"zdir/ord.h\""), "the first -I directory's header:\n{rule}");
    assert!(!rule.contains("adir/ord.h"), "not the one that sorts first:\n{rule}");
}

/// A compile fails if a directory it is told to search is missing, and in the
/// sandbox a directory exists only if an input is in it: the action creates
/// its `-I` directories.
#[test]
fn an_action_creates_the_include_directories_of_its_compile() {
    let run = kbuild_run(GRANTS);
    assert_clean(&run);
    let top = fragment(&run, "Makefile_");
    assert!(
        top.contains("\"zdir\"") && top.contains("\"adir\"") && top.contains("maybe_dirs = DIRS_"),
        "the compile's -I directories are carried to the action:\n{top}"
    );
    let rules = run.artifact("kbuild/rules.bzl");
    assert!(rules.contains("for d in ctx.attr.dirs") && rules.contains("mkdir -p"), "{rules}");
}

/// Each output of a recipe that makes several records all of them, so that
/// whoever uses one is given the rest (the generated `.c` includes its `.h`).
#[test]
fn the_outputs_of_one_recipe_are_recorded_as_a_group() {
    let run = kbuild_run(GRANTS);
    assert_clean(&run);
    let top = fragment(&run, "Makefile_");
    let at = top.find("out = \"grp.c1\"").unwrap_or_else(|| panic!("no rule:\n{top}"));
    let rest = &top[at..];
    let rule = &rest[..rest.find("specs.append").unwrap_or(rest.len())];
    assert!(
        rule.contains("group = [") && rule.contains("\"grp.h1\"") && rule.contains("\"grp.c1\""),
        "{rule}"
    );
}

/// A file a script is handed by name, and that is on disk, is an input even
/// though make does not list it (Kbuild's `usr/gen_initramfs.sh` and
/// `usr/default_cpio_list`).
#[test]
fn a_file_a_script_is_handed_by_name_is_an_input() {
    let run = kbuild_run(GRANTS);
    assert_clean(&run);
    let top = fragment(&run, "Makefile_");
    let at = top.find("out = \"listed.out\"").unwrap_or_else(|| panic!("no rule:\n{top}"));
    let rest = &top[at..];
    let rule = &rest[..rest.find("cmds = [").expect("rule has commands")];
    assert!(rule.contains("\"list.txt\"") && rule.contains("\"run.sh\""), "{rule}");
}

/// A file a command names beside its target, that nothing is told to produce,
/// is a candidate output of its action: `kbuild_declare` makes it one if no
/// target does. `flask2.h` names `other.h`, which has a rule of its own, and
/// so `other.h` is a candidate too: the decision is Bazel's.
#[test]
fn a_file_a_command_names_beside_its_target_is_a_candidate_output() {
    let run = kbuild_run(GRANTS);
    assert_clean(&run);
    let top = fragment(&run, "Makefile_");
    let at = top.find("out = \"flask.h\"").unwrap_or_else(|| panic!("no rule:\n{top}"));
    let rest = &top[at..];
    let rule = &rest[..rest.find("specs.append").unwrap_or(rest.len())];
    assert!(rule.contains("side = [") && rule.contains("\"perms.h\""), "{rule}");
    let rules = run.artifact("kbuild/rules.bzl");
    assert!(rules.contains("if c not in outs"), "{rules}");
}

/// A hidden file beside the target (`filechk`'s temporary) is not a candidate
/// output, and a candidate that is declared is not allowed to fail the build
/// if its command did not make it.
#[test]
fn a_hidden_temporary_is_not_a_side_output_and_a_guessed_one_may_be_missing() {
    let run = kbuild_run(GRANTS);
    assert_clean(&run);
    let top = fragment(&run, "Makefile_");
    let at = top.find("out = \"hid.h\"").unwrap_or_else(|| panic!("no rule:\n{top}"));
    let rest = &top[at..];
    let rule = &rest[..rest.find("specs.append").unwrap_or(rest.len())];
    assert!(!rule.contains("side = ["), "{rule}");
    let rules = run.artifact("kbuild/rules.bzl");
    assert!(rules.contains("lenient"), "{rules}");
}

/// A path a command names, that does not exist yet and is not a prerequisite,
/// is a candidate input: if a rule makes it (a tool built elsewhere), it is.
#[test]
fn a_tool_a_command_runs_is_a_candidate_input() {
    let run = kbuild_run(GRANTS);
    assert_clean(&run);
    let top = fragment(&run, "Makefile_");
    let at = top.find("out = \"used.out\"").unwrap_or_else(|| panic!("no rule:\n{top}"));
    let rest = &top[at..];
    let rule = &rest[..rest.find("specs.append").unwrap_or(rest.len())];
    assert!(rule.contains("maybe = [") && rule.contains("\"tools/mk\""), "{rule}");
}
