//! `BUILD.bazel` generation from a resolved make graph.
//!
//! This is [#632][pr] done as a plugin. That version put the emitter in
//! `src/depgraph.rs` behind a new `--dump-bazel` option, which is the shape
//! every such feature takes when the build tool has no plugin interface: a
//! flag in the getopt table, a module in the core, and a CI job that has to
//! be maintained alongside make itself. The output here is the same
//! per-package genrules; what changed is who owns them.
//!
//! [pr]: https://github.com/sevki/makers/pull/632
//!
//! Three things the plugin interface buys over the in-core version:
//!
//! * **Order-only prerequisites are excluded from `srcs`.** `foo.o: foo.c |
//!   build/` names `build/` as an ordering constraint, not an input. The
//!   in-core version emitted it as a `srcs` label, and Bazel then demands a
//!   target that produces a directory nobody declared. `node.dep-edges()`
//!   carries the flags, so the distinction is available here; the graph
//!   walk #632 used had no flags on its edges at all.
//! * **It runs under a capability grant.** Recipe text is where credentials
//!   end up, and this plugin transcribes recipes verbatim into files that
//!   get committed. `read-recipes` being withheld is a supported mode:
//!   the dump degrades to `cmd = "true"` genrules that still describe the
//!   dependency structure.
//! * **It cannot write outside its declared root.** The in-core version
//!   called `std::fs::write` on a path derived from a target name, so a
//!   makefile with a target named `../../etc/thing` wrote there. Here the
//!   root is declared and every entry path is confined to it by the host.
//!
//! It never needs `expand-variables`: a genrule wants the recipe *mostly*
//! unexpanded, because Bazel does its own expansion and the automatics the
//! two tools share have to survive into the generated file rather than being
//! substituted away. Plain variables are the exception — Bazel has no `CC`,
//! so `$(CC)` left alone becomes a genrule that runs `CC` as a command — and
//! those are substituted in the *target's* scope through `node.variable()`,
//! which needs only `read-variables`.
//!
//! It declares `deterministic`, and the declaration is now sound rather
//! than merely plausible. Two things had to be true for it. First
//! `session.input-digest` had to cover the global variable set, or `make
//! CC=gcc` and `make CC=clang` would share a digest while this plugin —
//! which substitutes `$(CC)` — produced different output for them. Second,
//! and the part that took longer: the digest deliberately *excludes*
//! environment-origin globals, because hashing `TERM` and `SSH_AUTH_SOCK`
//! would turn it over between two runs of an identical build and leave
//! `deterministic` correct but never cacheable. That exclusion only holds
//! up if a plugin cannot read what the digest does not cover, so
//! environment-origin values are now gated behind `read-environment` — a
//! capability this plugin does not request.
//!
//! What that buys: everything this plugin can see is either in the digest
//! or not readable by it. `node.variable()` still falls back to the global
//! set, but an environment-origin answer comes back as `none` now rather
//! than as a value the digest never saw, so `CC=gcc make` against a
//! makefile that does not define `CC` leaves the reference quoted instead
//! of silently baking in a value. It never requests `expand-variables`
//! either, which is the remaining way to reach an environment value without
//! naming it. See `docs/plugin-api.md` §9 in sevki/makers.

//! # `mode=kbuild`: recursive makes such as the Linux kernel
//!
//! Per-target genrules describe a build whose whole graph is in one `make`
//! process. Kbuild is not that: each directory is its own `make`, and the
//! command for `kernel/fork.o` only exists after `$(call if_changed_rule,
//! cc_o_c)` has been expanded against that target. Built with `--features
//! kbuild`, the plugin asks the host to expand each target's recipe
//! (`node.expand`), runs in every make Kbuild starts, and writes one Bazel
//! action per target. See [`kbuild`] for how the pieces fit.
//!
//! That build cannot declare `deterministic`: expanding a recipe can run
//! `$(shell ...)`, which the input digest does not cover. So it is a second
//! component, and the default build keeps the property.
//!
//! Settings (`--plugin-arg bazel:<key>=<value>`):
//!
//! | key    | meaning                                                          |
//! |--------|------------------------------------------------------------------|
//! | `mode` | `kbuild` to enable; anything else is the per-target mode         |
//! | `outs` | comma-separated files the top-level goals produce (`//:kernel`)  |
//! | `name` | name of the target that gathers `outs` (default `kernel`)        |
//!
//! The host grants `expand-variables` only on request:
//! `MAKERS_PLUGIN_ALLOW=bazel:expand-variables`.

#[cfg(feature = "kbuild")]
mod kbuild;

use makers_plugin::prelude::*;
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write as _;

/// Published per emitted genrule so another plugin can see the make-target
/// to Bazel-label mapping without parsing `BUILD.bazel`. Payload is
/// `<package>\0<rule name>`.
const GENRULE: &str = "makers:bazel/genrule";

struct BazelExport;

/// One genrule, before rule names have been made unique.
struct Candidate {
    package: String,
    output: String,
    srcs: Vec<String>,
    cmd: String,
}

thread_local! {
    static CANDIDATES: RefCell<Vec<Candidate>> = const { RefCell::new(Vec::new()) };
}

/// Split a target name into its Bazel package (the directory) and the file
/// within it. Backslashes are normalised so a makefile written on Windows
/// does not produce a package named `src\util`.
fn package_and_output(path: &str) -> (String, String) {
    let path = path.replace('\\', "/");
    match path.rsplit_once('/') {
        Some((dir, file)) => (dir.to_string(), file.to_string()),
        None => (String::new(), path),
    }
}

/// A label for `dep_name` as written from inside `pkg`: bare within the same
/// package, `//other/pkg:file` across packages.
fn dep_label_for_pkg(dep_name: &str, pkg: &str) -> Option<String> {
    let dep = dep_name.replace('\\', "/");
    let (dep_pkg, dep_out) = package_and_output(&dep);
    if dep_out.is_empty() {
        return None;
    }
    if dep_pkg == pkg {
        Some(dep_out)
    } else if dep_pkg.is_empty() {
        Some(format!("//:{dep_out}"))
    } else {
        Some(format!("//{dep_pkg}:{dep_out}"))
    }
}

/// Bazel target names are `[A-Za-z0-9_]`-ish; make target names are
/// anything. Collisions after sanitising are resolved by the caller, which
/// is why this is not responsible for uniqueness.
fn sanitize_rule_name(name: &str) -> String {
    let mut out: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if out.is_empty() {
        out.push_str("target");
    }
    if out.as_bytes()[0].is_ascii_digit() {
        out.insert(0, '_');
    }
    out
}

fn bazel_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Translate a make recipe into a genrule `cmd` string.
///
/// The two tools share a syntax and disagree about what it means, so every
/// `$` form has to be decided rather than copied:
///
/// * `$$` is a literal dollar in *both*, and passes through unchanged. Left
///   to the general `$(` rule below it would come out as `$$$(`, which Bazel
///   reads as a literal `$` followed by an unknown variable and rejects —
///   and `$$(date)` for shell command substitution is ordinary in makefiles.
/// * `$@` and `$<` mean the output and the first input in both.
/// * `$^` — every prerequisite — is make-only; Bazel spells it `$(SRCS)`.
///   Passing it through bare would hand the shell an empty string, so a
///   link rule would run with no inputs and produce a broken artifact that
///   looks like a successful build.
/// * `$(location ...)`, `$(@D)`, `$(@F)` and `$(RULEDIR)` are Bazel's own
///   and pass through.
/// * Everything else in `$(...)` is a make function or variable Bazel does
///   not have. Quoted to `$$(`, which reaches the shell as a literal `$(`.
///   Left bare, a `$(call if_changed,objcopy)` makes Bazel reject the rule
///   at analysis time.
fn bazel_cmd_escape(s: &str) -> String {
    let escaped = bazel_escape(s);
    let bytes = escaped.as_bytes();
    let mut out = String::with_capacity(escaped.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'$' {
            let start = i;
            while i < bytes.len() && bytes[i] != b'$' {
                i += 1;
            }
            out.push_str(&escaped[start..i]);
            continue;
        }
        match bytes.get(i + 1) {
            Some(b'$') => {
                out.push_str("$$");
                i += 2;
            }
            Some(b'^') => {
                out.push_str("$(SRCS)");
                i += 2;
            }
            Some(b'(') => {
                let rest = &escaped[i + 2..];
                let shared = ["@D)", "@F)", "RULEDIR)", "location ", "locations "]
                    .iter()
                    .any(|prefix| rest.starts_with(prefix));
                out.push_str(if shared { "$(" } else { "$$(" });
                i += 2;
            }
            // `$@`, `$<`, and any other one-character reference. Sized by
            // character rather than by byte: a recipe is arbitrary UTF-8 and
            // slicing a fixed two bytes would split a multi-byte character
            // and trap the component.
            Some(_) => {
                let ch = escaped[i + 1..].chars().next().expect("byte follows");
                let end = i + 1 + ch.len_utf8();
                out.push_str(&escaped[i..end]);
                i = end;
            }
            None => {
                out.push('$');
                i += 1;
            }
        }
    }
    out
}

/// The automatic variables. `node.variable()` would not resolve these
/// anyway — they are properties of the edge make is building, not variables
/// — but naming them explicitly is what keeps the substitution below from
/// depending on that.
fn is_automatic(name: &str) -> bool {
    matches!(
        name,
        "@" | "<"
            | "^"
            | "?"
            | "*"
            | "+"
            | "|"
            | "%"
            | "@D"
            | "@F"
            | "<D"
            | "<F"
            | "^D"
            | "^F"
            | "?D"
            | "?F"
            | "*D"
            | "*F"
    )
}

/// Substitute make variables in this target's scope, leaving the automatics
/// in place for Bazel to fill.
///
/// The split is the whole trick. `$(CC)` has to go — Bazel has no `CC`, and
/// a genrule whose `cmd` starts with `$(CC)` runs a command called `CC` and
/// fails. `$@` has to stay — Bazel substitutes the output path there, and
/// baking make's answer in would hard-code a path Bazel is about to choose
/// for itself. Target scope rather than global scope for the same reason
/// `compile-commands` needs it: `debug.o: CFLAGS := -O0` is the value that
/// target compiles with, and the global `CFLAGS` is a different string.
fn expand_variables_only(target: &Node, line: &str) -> String {
    makers_plugin::expand_with(line, 8, |name| {
        if is_automatic(name) {
            None
        } else {
            target.variable(name).map(|v| v.value)
        }
    })
}

/// A name make handles happily that has no Bazel spelling: pattern stems
/// belong to rules rather than targets, and an empty name is not a file.
fn emittable(name: &str) -> bool {
    !name.is_empty() && !name.contains('%')
}

pub(crate) fn setting(key: &str) -> Option<String> {
    makers_plugin::session::setting(key).filter(|v| !v.trim().is_empty())
}

impl Analyzer for BazelExport {
    fn describe() -> PluginInfo {
        let manifest = Manifest::new("bazel-export", env!("CARGO_PKG_VERSION"))
            .description("BUILD.bazel files from the resolved make graph")
            .capability(Capability::ReadRecipes)
            // For `$(CC)` and friends. Not `expand-variables`: the recipe
            // must keep its shape, and the full expander would also run
            // `$(shell ...)`.
            .capability(Capability::ReadVariables)
            .capability(Capability::WriteOutputs);
        // `deterministic`: everything this plugin reads is covered by
        // `session.input-digest`. Not `read-environment` — and that is what
        // makes the promise true, since environment-origin values are the
        // ones the digest excludes. See the module docs. The kbuild build
        // asks for `expand-variables` and so cannot make the promise.
        #[cfg(feature = "kbuild")]
        let manifest = manifest
            .capability(Capability::ExpandVariables)
            // To tell a file that is on disk from one make would remake.
            .capability(Capability::ReadFileContent);
        #[cfg(not(feature = "kbuild"))]
        let manifest = manifest.deterministic();
        manifest
            .output_directory(
                "build-files",
                ".",
                "root for the generated per-package BUILD.bazel files",
            )
            .build()
    }

    fn analyze(target: &Node) -> Result<Vec<Provider>, Error> {
        // Only recipe-bearing real files become genrules. A phony target is
        // not a file Bazel can produce, and emitting one yields a rule whose
        // `outs` never appears — which fails the build rather than
        // describing it.
        #[cfg(feature = "kbuild")]
        if kbuild::enabled() {
            return kbuild::analyze(target).map(|()| Vec::new());
        }
        if target.phony() {
            return Ok(Vec::new());
        }
        let name = target.name();
        if !emittable(&name) {
            return Ok(Vec::new());
        }
        let (package, output) = package_and_output(&name);
        if output.is_empty() {
            return Ok(Vec::new());
        }

        // `recipe()` returns `none` both when the target has no recipe and
        // when `read-recipes` is withheld. The two are deliberately
        // indistinguishable, so this plugin treats the withheld case as
        // "every target has an empty recipe" and still emits the structure.
        let Some(recipe) = target.recipe() else {
            return Ok(Vec::new());
        };

        let mut srcs = Vec::new();
        let mut seen = BTreeSet::new();
        for edge in target.dep_edges() {
            // An order-only prerequisite is an ordering constraint, not an
            // input. Bazel has no equivalent for a genrule, and listing it
            // in `srcs` asks for a label that nothing produces.
            if edge.flags.contains(DepFlags::ORDER_ONLY) {
                continue;
            }
            if !emittable(&edge.name) {
                continue;
            }
            if let Some(label) = dep_label_for_pkg(&edge.name, &package) {
                if seen.insert(label.clone()) {
                    srcs.push(label);
                }
            }
        }
        srcs.sort_unstable();

        let lines: Vec<String> = recipe
            .lines
            .iter()
            .map(|line| expand_variables_only(target, line.text.trim()))
            .filter(|line| !line.is_empty())
            .collect();
        let cmd = if lines.is_empty() {
            "true".to_string()
        } else {
            lines.join(" && ")
        };

        let rule_name = sanitize_rule_name(&output);
        CANDIDATES.with(|c| {
            c.borrow_mut().push(Candidate {
                package: package.clone(),
                output,
                srcs,
                cmd,
            })
        });
        Ok(vec![Provider {
            id: GENRULE.to_string(),
            payload: format!("{package}\0{rule_name}").into_bytes(),
        }])
    }

    fn finish() -> Result<(), Error> {
        #[cfg(feature = "kbuild")]
        if kbuild::enabled() {
            return kbuild::finish();
        }
        let candidates = CANDIDATES.with(|c| std::mem::take(&mut *c.borrow_mut()));
        if candidates.is_empty() {
            makers_plugin::note("no recipe-bearing file targets: nothing to emit");
            return Ok(());
        }

        // Sort before assigning names so the disambiguating suffix a
        // collision earns does not depend on analysis order. Analysis order
        // is dependency order, which is stable, but it is not the order the
        // file is written in, and a rule named `foo_2` in one run and `foo`
        // in the next is a spurious diff in a file people commit.
        let mut by_package: BTreeMap<String, Vec<Candidate>> = BTreeMap::new();
        for candidate in candidates {
            by_package
                .entry(candidate.package.clone())
                .or_default()
                .push(candidate);
        }

        let mut written = 0usize;
        for (package, mut targets) in by_package {
            targets.sort_by(|a, b| a.output.cmp(&b.output));

            let mut taken: BTreeSet<String> = BTreeSet::new();
            let mut document = String::from(
                "# Generated by the `bazel-export` makers plugin after dependency\n\
                 # resolution. This is a best-effort translation of make targets to\n\
                 # Bazel genrules; edit the makefile, not this file.\n\n",
            );
            for target in &targets {
                let mut rule_name = sanitize_rule_name(&target.output);
                if !taken.insert(rule_name.clone()) {
                    let base = rule_name.clone();
                    let mut suffix = 2usize;
                    while !taken.insert(format!("{base}_{suffix}")) {
                        suffix += 1;
                    }
                    rule_name = format!("{base}_{suffix}");
                }

                document.push_str("genrule(\n");
                document.push_str(&format!("    name = \"{}\",\n", bazel_escape(&rule_name)));
                if target.srcs.is_empty() {
                    document.push_str("    srcs = [],\n");
                } else {
                    document.push_str("    srcs = [\n");
                    for src in &target.srcs {
                        document.push_str(&format!("        \"{}\",\n", bazel_escape(src)));
                    }
                    document.push_str("    ],\n");
                }
                document.push_str(&format!(
                    "    outs = [\"{}\"],\n",
                    bazel_escape(&target.output)
                ));
                document.push_str(&format!(
                    "    cmd = \"{}\",\n",
                    bazel_cmd_escape(&target.cmd)
                ));
                document.push_str(")\n\n");
            }

            let entry = if package.is_empty() {
                "BUILD.bazel".to_string()
            } else {
                format!("{package}/BUILD.bazel")
            };
            let mut out = makers_plugin::open_output_in("build-files", &entry)?;
            out.write_all(document.as_bytes())
                .map_err(|e| makers_plugin::fail(e.to_string()))?;
            out.finish()?;
            written += 1;
        }

        makers_plugin::note(&format!("wrote {written} BUILD.bazel file(s)"));
        Ok(())
    }
}

export_plugin!(BazelExport);
