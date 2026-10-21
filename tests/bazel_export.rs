//! End-to-end tests for the `bazel-export` plugin.
//!
//! The plugin is the reason `artifacts` grew a `directory` output, so these
//! tests are as much about that interface addition as about the Bazel
//! translation: a plugin that names its own entries at runtime is a new
//! shape of authority, and the interesting assertions are about where those
//! entries are and are not allowed to land.
//!
//! Every test runs the real `make` binary against a real component. See
//! `tests/harness/mod.rs`. Requires the component to be built
//! (`cargo component build`) and `MAKERS_MAKE` to name a makers `make` built
//! with `--features wasmtime`: without the plugin host every assertion here
//! would fail against a build that simply ignores `MAKERS_PLUGINS`.
mod harness;

use harness::{assert_clean, component, run_make, workdir};

/// The fixture's sources, including one in a subdirectory so that
/// cross-package labels are exercised.
const SOURCES: &[&str] = &["main.c", "src/util.c"];

fn bazel_run(env_extra: &[(&str, &str)]) -> harness::Run {
    let plugin = component();
    let dir = workdir("plugin_bazel.mk", SOURCES);
    let spec = format!("bazel={}", plugin.display());
    let mut env: Vec<(&str, &str)> = vec![("MAKERS_PLUGINS", &spec)];
    env.extend_from_slice(env_extra);
    run_make(&dir, &env)
}

/// One `BUILD.bazel` per directory that owns targets — which is the whole
/// reason a single declared output file could not express this plugin.
/// Bazel's unit of organisation is the package, and which packages exist is
/// only known once the graph has been walked.
#[test]
fn one_build_file_is_emitted_per_package() {
    let run = bazel_run(&[]);
    assert_clean(&run);

    assert!(
        run.produced("BUILD.bazel"),
        "the root package owns `prog`, `main.o`, `stamp` and `build`:\n{}",
        run.stderr
    );
    assert!(
        run.produced("src/BUILD.bazel"),
        "`src/util.o` puts a target in the `src` package:\n{}",
        run.stderr
    );
    // Notes are suppressed unless the operator asks for them, so the count
    // is asserted under `MAKERS_PLUGIN_VERBOSE` rather than by default.
    let verbose = bazel_run(&[("MAKERS_PLUGIN_VERBOSE", "1")]);
    assert!(
        verbose.stderr.contains("wrote 2 BUILD.bazel file(s)"),
        "the plugin reports what it wrote:\n{}",
        verbose.stderr
    );
}

/// A target in another directory is referenced by label, and one in the same
/// package by bare name. Getting this wrong is the difference between a
/// build file Bazel loads and one it rejects.
#[test]
fn cross_package_prerequisites_become_labels() {
    let run = bazel_run(&[]);
    assert_clean(&run);
    let root = run.artifact("BUILD.bazel");

    assert!(
        root.contains("\"//src:util.o\""),
        "`src/util.o` is in another package and needs a label:\n{root}"
    );
    assert!(
        root.contains("\"main.o\""),
        "`main.o` is in this package and is named bare:\n{root}"
    );
}

/// An order-only prerequisite is an ordering constraint, not an input.
///
/// This is the correctness fix the interface makes possible: the in-core
/// `--dump-bazel` walked a graph whose edges carried no flags, so `| build`
/// came out as a `srcs` entry and Bazel then demanded a target producing a
/// directory nobody declared. `node.dep-edges()` carries the flags.
#[test]
fn order_only_prerequisites_are_not_srcs() {
    let run = bazel_run(&[]);
    assert_clean(&run);
    let root = run.artifact("BUILD.bazel");

    let main_o = genrule_named(&root, "main_o");
    assert!(
        main_o.contains("\"main.c\""),
        "the real input is there:\n{main_o}"
    );
    assert!(
        !main_o.contains("\"build\""),
        "`| build` is an ordering constraint and must not be an input:\n{main_o}"
    );
}

/// Phony targets are not files Bazel can produce. A genrule whose `outs`
/// never appears fails the build rather than describing it.
#[test]
fn phony_targets_are_not_emitted() {
    let run = bazel_run(&[]);
    assert_clean(&run);
    let root = run.artifact("BUILD.bazel");

    for phony in ["\"all\"", "\"clean\""] {
        assert!(
            !root.contains(phony),
            "{phony} is phony and must not become a genrule:\n{root}"
        );
    }
    assert!(
        root.contains("name = \"prog\""),
        "a real recipe-bearing target is emitted:\n{root}"
    );
}

/// The `$` forms the two tools disagree about, each decided rather than
/// copied. A genrule that Bazel rejects at analysis time, or one that runs
/// a link with no inputs, is worse than no genrule.
#[test]
fn recipe_dollar_forms_are_translated_for_bazel() {
    let run = bazel_run(&[]);
    assert_clean(&run);
    let root = run.artifact("BUILD.bazel");
    let src = run.artifact("src/BUILD.bazel");

    // `$^` is make-only. Left bare the shell sees nothing and the link
    // produces a broken binary that looks like a successful build.
    let prog = genrule_named(&root, "prog");
    assert!(
        prog.contains("$(SRCS)"),
        "`$^` becomes Bazel's `$(SRCS)`:\n{prog}"
    );
    assert!(!prog.contains("$^"), "and no bare `$^` survives:\n{prog}");

    // `$@` and `$<` mean the same thing in both and pass through.
    assert!(prog.contains("-o $@"), "`$@` is shared vocabulary:\n{prog}");

    // A make function Bazel does not have is quoted so Bazel does not try
    // to expand it and reject the rule.
    let util = genrule_named(&src, "util_o");
    assert!(
        util.contains("$$(call extra_flags,util)"),
        "`$(call ...)` is quoted for the shell:\n{util}"
    );

    // `$$` is a literal dollar in both. Quoting it like a make function
    // would produce `$$$(`, which Bazel reads as a literal `$` followed by
    // an unknown variable.
    let stamp = genrule_named(&root, "stamp");
    assert!(
        stamp.contains("$$(date -u)") && !stamp.contains("$$$(date"),
        "`$$(...)` shell substitution passes through unchanged:\n{stamp}"
    );
}

/// A plain variable is substituted in the target's scope, because Bazel has
/// no `CC`: left alone, `$(CC)` becomes a genrule that runs a command called
/// `CC`. The automatics around it still survive for Bazel to fill.
#[test]
fn plain_variables_are_substituted_but_automatics_are_not() {
    let run = bazel_run(&[]);
    assert_clean(&run);
    let main_o = genrule_named(&run.artifact("BUILD.bazel"), "main_o");

    assert!(
        main_o.contains("cmd = \"cc -c -o $@ $<\""),
        "`$(CC)` becomes its value while `$@` and `$<` stay for Bazel:\n{main_o}"
    );
}

/// Withholding `read-variables` costs the substitution and nothing else: the
/// structure is still emitted, with the unresolved reference quoted so Bazel
/// does not try to expand a variable it has never heard of.
#[test]
fn withholding_read_variables_leaves_references_quoted() {
    let run = bazel_run(&[("MAKERS_PLUGIN_DENY", "bazel:read-variables")]);
    assert_clean(&run);
    let main_o = genrule_named(&run.artifact("BUILD.bazel"), "main_o");

    assert!(
        main_o.contains("$$(CC)"),
        "an unresolvable reference is quoted, not guessed at:\n{main_o}"
    );
    assert!(
        main_o.contains("\"main.c\""),
        "and the dependency structure is unaffected:\n{main_o}"
    );
}

/// Byte-for-byte identical across runs. These files get committed, so a
/// generator that reshuffles them produces a diff for every run.
#[test]
fn output_is_byte_stable_across_runs() {
    let first = bazel_run(&[]);
    assert_clean(&first);
    let second = bazel_run(&[]);
    assert_clean(&second);

    for name in ["BUILD.bazel", "src/BUILD.bazel"] {
        assert_eq!(
            first.artifact(name),
            second.artifact(name),
            "{name} must not differ between runs"
        );
    }
}

/// The declared root is retargetable like any other output, so a dump can be
/// inspected without writing into the source tree.
#[test]
fn the_output_root_is_configurable() {
    let run = bazel_run(&[("MAKERS_PLUGIN_ARGS", "bazel:out.build-files=generated")]);
    assert_clean(&run);

    assert!(
        run.produced("generated/BUILD.bazel") && run.produced("generated/src/BUILD.bazel"),
        "the whole tree moves under the configured root:\n{}",
        run.stderr
    );
    assert!(
        !run.produced("BUILD.bazel"),
        "and nothing is left at the default root:\n{}",
        run.stderr
    );
}

/// Withholding `read-recipes` degrades rather than fails: the dependency
/// structure is still described. Recipe text is what this plugin copies
/// verbatim into files people commit, so an operator refusing it is a case
/// worth supporting rather than an error.
#[test]
fn withholding_read_recipes_is_reported_and_degrades() {
    let run = bazel_run(&[("MAKERS_PLUGIN_DENY", "bazel:read-recipes")]);
    assert_clean(&run);

    assert!(
        run.stderr.contains("withheld capabilities: read-recipes"),
        "the host says what it withheld:\n{}",
        run.stderr
    );
    assert!(
        !run.produced("BUILD.bazel"),
        "with no recipe visible there is no genrule to write:\n{}",
        run.stderr
    );
}

/// `write-outputs` is what the whole plugin depends on, so withholding it
/// is a plugin failure — but an advisory one, and the build still succeeds.
#[test]
fn withholding_write_outputs_fails_the_plugin_but_not_the_build() {
    let run = bazel_run(&[("MAKERS_PLUGIN_DENY", "bazel:write-outputs")]);

    assert!(
        run.status.success(),
        "an advisory plugin failure must not fail the build:\n{}",
        run.stderr
    );
    assert!(
        run.stderr.contains("write-outputs"),
        "and the reason is named:\n{}",
        run.stderr
    );
    assert!(!run.produced("BUILD.bazel"), "nothing was written");
}

/// Extract the text of one `genrule(...)` block by its `name`, so an
/// assertion about one rule cannot accidentally be satisfied by another.
fn genrule_named(document: &str, name: &str) -> String {
    let needle = format!("name = \"{name}\",");
    let start = document
        .find(&needle)
        .unwrap_or_else(|| panic!("no genrule named `{name}` in:\n{document}"));
    let block_start = document[..start]
        .rfind("genrule(")
        .expect("a name is inside a genrule");
    let end = document[block_start..]
        .find("\n)\n")
        .map(|e| block_start + e)
        .unwrap_or(document.len());
    document[block_start..end].to_string()
}

// ─── the input digest ────────────────────────────────────────────────────

/// The digest a run computed, from the verbose summary.
fn digest_of(run: &harness::Run) -> String {
    let start = run
        .stderr
        .find("digest: ")
        .unwrap_or_else(|| panic!("no digest in:\n{}", run.stderr))
        + "digest: ".len();
    let rest = &run.stderr[start..];
    let end = rest.find(';').unwrap_or(rest.len());
    rest[..end].trim().to_string()
}

/// A command-line variable assignment reaches `session.input-digest`.
///
/// This is the defect that kept `bazel-export` from declaring
/// `deterministic`: `make CC=gcc` and `make CC=clang` build an identical
/// graph with identical unexpanded recipe text, so a digest covering only
/// the file arena was the same for both while the plugin's output — which
/// substitutes `$(CC)` — differed. A cache keyed on it would have served the
/// wrong artifact.
///
/// Both runs share one working directory on purpose. `CURDIR` and
/// `MAKEFILE_LIST` are themselves globals, so two runs in two temporary
/// directories would differ for reasons that have nothing to do with the
/// variable under test.
#[test]
fn a_command_line_variable_reaches_the_digest() {
    let plugin = component();
    let dir = workdir("plugin_bazel.mk", SOURCES);
    let spec = format!("bazel={}", plugin.display());
    let env = [
        ("MAKERS_PLUGINS", spec.as_str()),
        ("MAKERS_PLUGIN_VERBOSE", "1"),
    ];

    let gcc = harness::run_make_with_args(&dir, &["PROBE=gcc"], &env);
    let clang = harness::run_make_with_args(&dir, &["PROBE=clang"], &env);
    assert_clean(&gcc);
    assert_clean(&clang);

    assert_ne!(
        digest_of(&gcc),
        digest_of(&clang),
        "a command-line assignment must move the digest"
    );
    // Same assignment twice is the same question, or the digest is useless
    // as a cache key.
    assert_eq!(
        digest_of(&gcc),
        digest_of(&harness::run_make_with_args(
            &dir,
            &["PROBE=gcc"],
            &env
        )),
        "and the same assignment must reproduce it"
    );
}

/// An environment variable does not, and that is the deliberate hole.
///
/// Every process carries `TERM`, `SSH_AUTH_SOCK` and a shell's worth of
/// other noise, all of which make imports into the global set. Folding those
/// in would turn the digest over between two runs of the same build in the
/// same tree, leaving `deterministic` correct and never cacheable. The cost
/// is that a plugin holding only `read-variables` can still read an
/// environment-origin value the digest does not cover — which is why
/// `vars.get` reports the true origin, so a plugin that cares can tell.
///
/// The test uses the same variable name as the command-line case above, so
/// the only difference between them is `$(origin ...)`.
#[test]
fn an_environment_variable_does_not_reach_the_digest() {
    let plugin = component();
    let dir = workdir("plugin_bazel.mk", SOURCES);
    let spec = format!("bazel={}", plugin.display());

    let digest_with = |probe: &str| {
        digest_of(&run_make(
            &dir,
            &[
                ("MAKERS_PLUGINS", spec.as_str()),
                ("MAKERS_PLUGIN_VERBOSE", "1"),
                ("PROBE", probe),
            ],
        ))
    };
    assert_eq!(
        digest_with("gcc"),
        digest_with("clang"),
        "an environment-origin variable is excluded from the digest"
    );
}

// ─── the `read-environment` gate on variable reads ───────────────────────

/// Run the plugin against the `EXTRA_CFLAGS` fixture, with `args` on the
/// command line and `env_extra` in the environment.
fn env_gate_run(args: &[&str], env_extra: &[(&str, &str)]) -> harness::Run {
    let plugin = component();
    let dir = workdir("plugin_bazel_env.mk", &["probe.c"]);
    let spec = format!("bazel={}", plugin.display());
    let mut env: Vec<(&str, &str)> = vec![("MAKERS_PLUGINS", &spec)];
    env.extend_from_slice(env_extra);
    harness::run_make_with_args(&dir, args, &env)
}

/// A plugin holding only `read-variables` cannot resolve an
/// environment-origin value.
///
/// This is what makes the digest's environment exclusion sound rather than
/// merely convenient. The digest deliberately omits environment-origin
/// globals — hashing `TERM` and `SSH_AUTH_SOCK` would turn it over between
/// two runs of an identical build — and that omission only holds up if a
/// plugin cannot read what the digest does not cover. Otherwise a cache
/// keyed on the digest serves output built from a different `$(HOME)`.
///
/// The reference is left quoted, exactly as it is for a plugin denied
/// `read-variables` outright: an unresolvable reference is preserved, never
/// guessed at.
#[test]
fn an_environment_variable_is_withheld_without_read_environment() {
    let run = env_gate_run(&[], &[("EXTRA_CFLAGS", "-O2")]);
    assert_clean(&run);
    let probe = genrule_named(&run.artifact("BUILD.bazel"), "probe_o");

    assert!(
        probe.contains("$$(EXTRA_CFLAGS)"),
        "an environment-origin value is not readable under `read-variables` \
         alone, so the reference stays quoted:\n{probe}"
    );
    assert!(
        !probe.contains("-O2"),
        "and its value must not appear anywhere in the output:\n{probe}"
    );
}

/// An operator cannot hand the value back by allowing `read-environment`,
/// because this plugin never asked for it.
///
/// The grant is the intersection of what the manifest requests and what the
/// operator allows (`requested & allowed`), so `MAKERS_PLUGIN_ALLOW` widens
/// a plugin only up to its own declaration. That is what makes a manifest
/// worth reading: a plugin's ceiling is fixed by what it admits to needing,
/// not by the environment it happens to run in.
///
/// The other half of the rule — that a plugin which *does* request and
/// receive `read-environment` sees the value — has no end-to-end case here
/// because no in-tree plugin requests it. It is covered by
/// `plugin::host::environment_gate_tests`, which exercises the gate against
/// both grant states directly.
#[test]
fn allowing_read_environment_does_not_widen_a_plugin_that_never_asked() {
    let run = env_gate_run(
        &[],
        &[
            ("EXTRA_CFLAGS", "-O2"),
            ("MAKERS_PLUGIN_ALLOW", "bazel:read-environment"),
        ],
    );
    assert_clean(&run);
    let probe = genrule_named(&run.artifact("BUILD.bazel"), "probe_o");

    assert!(
        probe.contains("$$(EXTRA_CFLAGS)"),
        "`bazel-export` does not request `read-environment`, so allowing it \
         grants nothing:\n{probe}"
    );
}

/// The same name assigned on the command line resolves without it.
///
/// `make EXTRA_CFLAGS=-O2` is origin `command line`, not `environment`, and
/// the digest covers it — so there is nothing for the gate to protect and
/// withholding it would only break the common way of varying a build from
/// outside the makefile. The two cases differ in `$(origin ...)` and
/// nothing else, which is precisely the distinction being drawn.
#[test]
fn a_command_line_variable_is_not_withheld() {
    let run = env_gate_run(&["EXTRA_CFLAGS=-O2"], &[]);
    assert_clean(&run);
    let probe = genrule_named(&run.artifact("BUILD.bazel"), "probe_o");

    assert!(
        probe.contains("-O2"),
        "a command-line assignment is covered by the digest and stays \
         readable:\n{probe}"
    );
}

/// `mode=wrap` is for makefiles whose recipes re-invoke make, Kbuild above
/// all: the top-level graph is a fragment, so instead of a genrule per target
/// the plugin emits one action around the whole build.
fn wrap_run(settings: &str) -> harness::Run {
    bazel_run(&[("MAKERS_PLUGIN_ARGS", settings)])
}

#[test]
fn wrap_mode_emits_one_action_around_the_build() {
    let run = wrap_run(
        "bazel:mode=wrap;bazel:outs=prog,sub/image;bazel:goals=prog;bazel:configure=defconfig",
    );
    assert_clean(&run);

    assert!(run.produced("MODULE.bazel"), "Bazel needs a module:\n{}", run.stderr);
    assert!(
        !run.produced("src/BUILD.bazel"),
        "wrap mode does not translate the graph:\n{}",
        run.stderr
    );
    let build = run.artifact("BUILD.bazel");
    assert!(build.contains("name = \"kernel\""), "{build}");
    assert!(build.contains("\"sub/image\","), "declared outputs are returned:\n{build}");
    assert!(build.contains("mk defconfig"), "configure goals run first:\n{build}");
    assert!(build.contains("mk 'prog'"), "goals are quoted into the script:\n{build}");
    assert!(
        build.contains("$(RULEDIR)") && build.contains("$$work"),
        "Bazel's own variable survives, the shell's is doubled:\n{build}"
    );
}

#[test]
fn wrap_mode_refuses_outputs_outside_the_tree() {
    let run = wrap_run("bazel:mode=wrap;bazel:outs=../escape");
    assert!(
        !run.produced("BUILD.bazel"),
        "an output outside the tree must not be written:\n{}",
        run.stderr
    );
}

#[test]
fn wrap_mode_requires_outs() {
    let run = wrap_run("bazel:mode=wrap");
    assert!(!run.produced("BUILD.bazel"), "{}", run.stderr);
}
