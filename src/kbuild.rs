//! `mode=kbuild`: Bazel files for a recursive make, Kbuild above all.
//!
//! Kbuild is one `make` per directory (`$(MAKE) -f scripts/Makefile.build
//! obj=kernel/sched`), so no single run sees the whole graph and the
//! top-level recipes are the recursion, not the compiles. The compile of
//! `kernel/fork.o` only exists as text after `$(call if_changed_rule,cc_o_c)`
//! has been expanded against `$@` and `$<` inside the sub-make for `kernel`.
//!
//! The plugin therefore runs in every one of those makes, because makers
//! loads it into each, and each run describes only its own directory:
//!
//! * a target whose recipe recurses is an edge, not a rule. Its expanded
//!   command line names the child make, and the run records the child's
//!   fragment so the parent can load it;
//! * any other target with a command to run becomes one [`kbuild_spec`],
//!   carrying the command make would run, as `node.expand` gives it. A
//!   target that already exists, or whose recipe expands to nothing, is a
//!   file the build starts from;
//! * the run writes `kbuild/<key>.bzl`, a macro defining its rules and
//!   calling its children's. The top-level run also writes `BUILD.bazel`,
//!   `MODULE.bazel` and `kbuild/rules.bzl`.
//!
//! Everything lives in the root package: a label is the file's path, and a
//! source, a pre-generated file and another run's output are all spelled the
//! same way.
//!
//! The commands are Kbuild's own, cut at the point where Kbuild would call
//! `fixdep` to record dependencies for its next run; Bazel keeps its own.
//! What is left depends on files the recipe does not name, chiefly headers,
//! so each action is given the `*.h` under the directories its command
//! passes with `-I`, and the build-system files when it runs a script.
//!
//! The tree is expected to have been configured and prepared (`make
//! defconfig prepare`), and the plugin run is a dry run of the goals
//! (`make -n vmlinux bzImage`). Generated headers and host tools exist by
//! then, so their rules expand to nothing and are treated as inputs.

use makers_plugin::prelude::*;
use std::cell::RefCell;
use std::collections::BTreeSet;
use std::io::Write as _;

const RULES_BZL: &str = include_str!("kbuild_rules.bzl");

/// One target to build, before it is written out.
struct Rule {
    out: String,
    srcs: Vec<String>,
    cmds: Vec<String>,
    thin: bool,
    env: bool,
    infra: bool,
    /// Includes the scan could not find on disk: they may be files another
    /// make generates, which only Bazel, once it has every rule, can tell.
    /// `maybe` are exact paths (a quoted include beside its includer);
    /// `maybe_names` are names to look for in each of `maybe_dirs`.
    maybe: Vec<String>,
    maybe_names: Vec<String>,
    maybe_dirs: Vec<String>,
}

#[derive(Default)]
struct Run {
    rules: Vec<Rule>,
    children: BTreeSet<String>,
    env: Option<Vec<(String, String)>>,
    scanner: Scanner,
}

thread_local! {
    static RUN: RefCell<Run> = RefCell::new(Run::default());
}

pub fn enabled() -> bool {
    super::setting("mode").as_deref() == Some("kbuild")
}

fn norm(path: &str) -> String {
    let mut p = path.replace('\\', "/");
    while let Some(rest) = p.strip_prefix("./") {
        p = rest.to_string();
    }
    p
}

fn ident(s: &str) -> String {
    let mut out: String = s
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    if out.is_empty() {
        out.push('_');
    }
    out
}

/// The name of the rule that produces `out`. A rule named like a file in the
/// same package is ambiguous as a label (`vmlinux`), so it gets a suffix
/// when its output has nothing in it for `ident` to replace.
fn rule_name(out: &str) -> String {
    let name = ident(out);
    if name == out {
        format!("{name}_")
    } else {
        name
    }
}

/// FNV-1a, hex. Only used to keep names short and stable.
fn short_hash(parts: &[&str]) -> String {
    let mut h: u32 = 0x811c_9dc5;
    for p in parts {
        for b in p.bytes().chain(std::iter::once(0)) {
            h ^= u32::from(b);
            h = h.wrapping_mul(0x0100_0193);
        }
    }
    format!("{h:08x}")
}

/// Which make a fragment describes: where it ran, which makefile it read,
/// which directory of the tree it was building (`obj=`) and its goals.
///
/// Two ends have to agree on this. The parent, which reads it off the
/// recursive command line it expands, and the child, which reads it off its
/// own state. So it is made of things both can see and compared in absolute
/// terms: a child started with `-C tools/objtool` sees only its own working
/// directory, while its parent sees the relative path and its own.
#[derive(Clone, PartialEq, Eq)]
struct Identity {
    cwd: String,
    makefile: String,
    obj: Option<String>,
    goals: Vec<String>,
}

/// `path` resolved against `base`, lexically: `.` and `..` folded, no
/// symlinks followed.
fn resolve(base: &str, path: &str) -> String {
    let joined = if path.starts_with('/') {
        path.to_string()
    } else {
        format!("{base}/{path}")
    };
    let mut parts: Vec<&str> = Vec::new();
    for part in joined.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            p => parts.push(p),
        }
    }
    format!("/{}", parts.join("/"))
}

impl Identity {
    fn new(cwd: &str, makefile: &str, obj: Option<&str>, goals: &[String]) -> Self {
        let mut goals: Vec<String> = goals
            .iter()
            .map(|g| norm(g))
            .filter(|g| !g.is_empty())
            .collect();
        goals.sort();
        Identity {
            cwd: resolve("/", cwd),
            makefile: resolve(cwd, makefile),
            obj: obj.filter(|o| !o.is_empty()).map(norm),
            goals,
        }
    }

    /// File and function name for this make's fragment.
    fn key(&self) -> String {
        let goals = self.goals.join(" ");
        let hash = short_hash(&[
            &self.cwd,
            &self.makefile,
            self.obj.as_deref().unwrap_or(""),
            &goals,
        ]);
        let label = match &self.obj {
            Some(obj) => ident(obj),
            None => ident(self.makefile.rsplit('/').next().unwrap_or("make")),
        };
        format!("{label}_{hash}")
    }

    fn is_root(&self) -> bool {
        let wanted: Vec<String> = super::setting("goals")
            .map(|g| {
                g.split(|c: char| c == ',' || c.is_whitespace())
                    .map(norm)
                    .filter(|g| !g.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        !wanted.is_empty() && self.goals == {
            let mut w = wanted;
            w.sort();
            w
        }
    }
}

fn this_identity() -> Identity {
    // The first makefile make read, which is the one `-f` named. Read as a
    // variable: it is the list `session.makefiles` is meant to be.
    //
    // Not `MAKEFILES`: makefiles injected for the plugin run, such as the
    // stand-in rules for a dry run, are read first and are named by
    // `bazel:skip-makefiles` so that they do not stand for the make.
    let skip: Vec<String> = super::setting("skip-makefiles")
        .map(|v| v.split(',').map(|s| resolve("/", s.trim())).collect())
        .unwrap_or_default();
    let cwd = makers_plugin::session::working_directory();
    let makefile = makers_plugin::vars::get("MAKEFILE_LIST")
        .and_then(|v| {
            v.value
                .split_whitespace()
                .find(|m| !skip.contains(&resolve(&cwd, m)))
                .map(str::to_string)
        })
        .unwrap_or_else(|| "Makefile".to_string());
    let obj = makers_plugin::vars::get("obj").map(|v| v.value);
    // `MAKECMDGOALS` is the goals as written on the command line, and empty
    // when make picked the default goal, which is also what a parent reading
    // the command sees. `session.goal-names` is not that: it names the
    // default goal too.
    let goals: Vec<String> = makers_plugin::vars::get("MAKECMDGOALS")
        .map(|v| v.value.split_whitespace().map(str::to_string).collect())
        .unwrap_or_default();
    Identity::new(
        &makers_plugin::session::working_directory(),
        &makefile,
        obj.as_deref(),
        &goals,
    )
}

/// Split shell text into words, honouring quotes and backslashes. The
/// operators that end a command (`;`, `&&`, `||`, `|`, `>`, `<`) come out as
/// words of their own, and only when unquoted.
fn shell_words(text: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                in_word = true;
                for q in chars.by_ref() {
                    if q == '\'' {
                        break;
                    }
                    cur.push(q);
                }
            }
            '"' => {
                in_word = true;
                while let Some(q) = chars.next() {
                    match q {
                        '"' => break,
                        '\\' => {
                            if let Some(n) = chars.next() {
                                if !matches!(n, '"' | '\\' | '$' | '`') {
                                    cur.push('\\');
                                }
                                cur.push(n);
                            }
                        }
                        q => cur.push(q),
                    }
                }
            }
            '\\' => match chars.next() {
                // A line continuation joins words; it does not make one.
                Some('\n') | None => {}
                Some(n) => {
                    in_word = true;
                    cur.push(n);
                }
            },
            c if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut cur));
                    in_word = false;
                }
            }
            ';' | '|' | '&' | '>' | '<' => {
                if in_word {
                    words.push(std::mem::take(&mut cur));
                    in_word = false;
                }
                let mut op = c.to_string();
                if matches!(c, '|' | '&' | '>') && chars.peek() == Some(&c) {
                    op.push(chars.next().expect("peeked"));
                }
                words.push(op);
            }
            c => {
                in_word = true;
                cur.push(c);
            }
        }
    }
    if in_word {
        words.push(cur);
    }
    words
}

/// The make a recursive command line starts, if it starts one, as seen from
/// the make running it in `cwd`.
fn child_identity(cwd: &str, command: &str) -> Option<Identity> {
    let words = shell_words(command);
    let mut words = words.iter().map(String::as_str);
    words.by_ref().find(|w| *w == "make" || w.ends_with("/make"))?;
    let (mut dir, mut makefile, mut obj) = (None, None, None);
    let mut goals = Vec::new();
    while let Some(w) = words.next() {
        if w == "-C" {
            dir = words.next().map(str::to_string);
        } else if let Some(d) = w.strip_prefix("--directory=") {
            dir = Some(d.to_string());
        } else if w == "-f" {
            makefile = words.next().map(str::to_string);
        } else if let Some(f) = w.strip_prefix("--file=") {
            makefile = Some(f.to_string());
        } else if let Some(o) = w.strip_prefix("obj=") {
            obj = Some(o.to_string());
        } else if matches!(w, "|" | ";" | "&&" | "||" | ">" | "<" | "&") {
            break;
        } else if w.starts_with('-') || w.contains('=') {
            // A flag or a variable assignment.
            continue;
        } else {
            goals.push(w.to_string());
        }
    }
    let cwd = match dir {
        Some(d) => resolve(cwd, &d),
        None => cwd.to_string(),
    };
    let makefile = makefile.unwrap_or_else(|| "Makefile".to_string());
    Some(Identity::new(&cwd, &makefile, obj.as_deref(), &goals))
}

/// The commands make would run for one expanded recipe line, in order, each
/// with whether a failure is ignored (`-`).
///
/// A recipe line that expands to several lines (Kbuild's `rule_*` macros)
/// runs them as separate commands, so they are split the same way. The
/// `@`, `-` and `+` prefixes are make's, not the shell's. And the `fixdep`
/// step Kbuild appends to a compile is cut: it records dependencies in a
/// `.cmd` file for Kbuild's next run, which is Bazel's job here.
fn commands(expanded: &str) -> Vec<(String, bool)> {
    let mut logical: Vec<String> = Vec::new();
    let mut carry = String::new();
    for line in expanded.split('\n') {
        let trailing = line.len() - line.trim_end_matches('\\').len();
        if trailing % 2 == 1 {
            carry.push_str(line);
            carry.push('\n');
            continue;
        }
        carry.push_str(line);
        logical.push(std::mem::take(&mut carry));
    }
    if !carry.is_empty() {
        logical.push(carry);
    }
    let mut out = Vec::new();
    for line in logical {
        let mut text = line.trim_start();
        let mut ignore = false;
        while let Some(c) = text.chars().next() {
            match c {
                '@' | '+' => text = text[1..].trim_start(),
                '-' => {
                    ignore = true;
                    text = text[1..].trim_start();
                }
                _ => break,
            }
        }
        let text = match text.find("; scripts/basic/fixdep ") {
            Some(i) => &text[..i],
            None => text,
        };
        let text = text.trim_end();
        if text.is_empty() || text == ":" {
            continue;
        }
        out.push((text.to_string(), ignore));
    }
    out
}

/// A command line's statements: split at `;`, `&&`, `||` and newlines that
/// are not quoted. `a | b` stays one statement.
fn statements(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut chars = text.chars().peekable();
    let (mut single, mut double) = (false, false);
    while let Some(c) = chars.next() {
        match c {
            '\\' if !single => {
                cur.push(c);
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
            }
            '\'' if !double => {
                single = !single;
                cur.push(c);
            }
            '"' if !single => {
                double = !double;
                cur.push(c);
            }
            ';' | '\n' if !single && !double => out.push(std::mem::take(&mut cur)),
            '&' | '|' if !single && !double && chars.peek() == Some(&c) => {
                chars.next();
                out.push(std::mem::take(&mut cur));
            }
            c => cur.push(c),
        }
    }
    out.push(cur);
    out.into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn starts_make(statement: &str) -> bool {
    shell_words(statement)
        .first()
        .is_some_and(|w| w == "make" || w.ends_with("/make"))
}

/// Statements that only announce or tidy, and so do not make a line more than
/// a recursion: Kbuild wraps its commands in `set -e`, an `echo` of the
/// short form and `trap`s that delete the target on interrupt.
fn is_noise(statement: &str) -> bool {
    let word = statement.split_whitespace().next().unwrap_or("");
    matches!(word, "set" | "echo" | "trap" | ":")
}

/// Directories a command asks the compiler to search for headers.
fn include_dirs(cmds: &[String]) -> BTreeSet<String> {
    let mut dirs = BTreeSet::new();
    for cmd in cmds {
        let mut words = cmd.split_whitespace();
        while let Some(w) = words.next() {
            let dir = if w == "-I" {
                words.next()
            } else {
                w.strip_prefix("-I")
            };
            if let Some(d) = dir {
                let d = norm(d.trim_matches(|c| c == '\'' || c == '"'));
                if !d.is_empty() && d != "." && !d.starts_with('/') && !d.contains("..") {
                    dirs.insert(d);
                }
            }
        }
    }
    dirs
}

fn is_thin_archive(cmds: &[String]) -> bool {
    cmds.iter().any(|c| {
        let mut words = c.split_whitespace();
        while let Some(w) = words.next() {
            if w == "ar" {
                return words.next().is_some_and(|flags| flags.contains('T'));
            }
        }
        false
    })
}

/// What a source file includes, as `(quoted, name)`: `#include "x"` is
/// `(true, "x")`, `#include <x>` is `(false, "x")`.
///
/// A scan, not a preprocessor: it reads every `#include` line whether or not
/// the `#if` around it holds, so it finds a superset of what the compiler
/// opens. That is the safe side, since an input that is not read costs a
/// rebuild at worst and one that is missing is a failure.
fn parse_includes(text: &str) -> Vec<(bool, String)> {
    let mut out = Vec::new();
    for line in text.lines() {
        let Some(rest) = line.trim_start().strip_prefix('#') else {
            continue;
        };
        let rest = rest.trim_start();
        let Some(rest) = rest
            .strip_prefix("include_next")
            .or_else(|| rest.strip_prefix("include"))
        else {
            continue;
        };
        let rest = rest.trim_start();
        let (quoted, close) = match rest.chars().next() {
            Some('"') => (true, '"'),
            Some('<') => (false, '>'),
            _ => continue,
        };
        if let Some(end) = rest[1..].find(close) {
            out.push((quoted, rest[1..1 + end].to_string()));
        }
    }
    out
}

/// Finds the files a compile reads, the way `fixdep` records them after the
/// fact: by following `#include` from the source through the directories the
/// command line searches.
///
/// Resolving an include means probing the disk in each search directory, and
/// the same header is reached from hundreds of sources, so what each file
/// resolves to is remembered per set of search directories.
#[derive(Default)]
struct Scanner {
    includes: std::collections::HashMap<String, Vec<(bool, String)>>,
    resolved: std::collections::HashMap<(String, String), std::rc::Rc<Resolved>>,
}

/// What one file's includes resolved to.
#[derive(Default)]
struct Resolved {
    found: Vec<String>,
    /// Quoted includes that are not on disk, as the path beside the includer.
    missing_beside: Vec<String>,
    /// Names that are not on disk in any search directory.
    missing_names: Vec<String>,
}

/// Everything reachable from some files by `#include`.
#[derive(Default)]
struct Reach {
    files: BTreeSet<String>,
    missing_beside: BTreeSet<String>,
    missing_names: BTreeSet<String>,
}

impl Scanner {
    fn includes_of(&mut self, file: &str) -> &Vec<(bool, String)> {
        self.includes.entry(file.to_string()).or_insert_with(|| {
            match std::fs::read(file) {
                Ok(bytes) => parse_includes(&String::from_utf8_lossy(&bytes)),
                Err(_) => Vec::new(),
            }
        })
    }

    /// The files `file` includes that exist, searching `dirs` after the
    /// file's own directory for the quoted form.
    fn resolve_all(&mut self, file: &str, dirs: &[String], dirs_key: &str) -> std::rc::Rc<Resolved> {
        let key = (dirs_key.to_string(), file.to_string());
        if let Some(hit) = self.resolved.get(&key) {
            return hit.clone();
        }
        let here = file.rsplit_once('/').map_or("", |(d, _)| d).to_string();
        let wanted = self.includes_of(file).clone();
        let mut out = Resolved::default();
        for (quoted, name) in wanted {
            let beside = quoted.then(|| from_root(&here, &name)).flatten();
            let hit = beside
                .clone()
                .into_iter()
                .chain(dirs.iter().filter_map(|d| from_root(d, &name)))
                .find(|candidate| on_disk(candidate));
            match hit {
                Some(hit) => out.found.push(hit),
                None => {
                    out.missing_beside.extend(beside);
                    if !name.starts_with('/') && !name.split('/').any(|p| p == ".." || p == ".") {
                        out.missing_names.push(name);
                    }
                }
            }
        }
        let out = std::rc::Rc::new(out);
        self.resolved.insert(key, out.clone());
        out
    }

    /// Every file reachable by `#include` from `start`, found in `dirs`.
    /// Paths are relative to the tree root; this only works in a make that
    /// runs there.
    fn reachable(&mut self, start: &[String], dirs: &[String]) -> Reach {
        let dirs_key = dirs.join("\n");
        let mut reach = Reach::default();
        let mut queue: Vec<String> = start.to_vec();
        while let Some(file) = queue.pop() {
            if !reach.files.insert(file.clone()) {
                continue;
            }
            let next = self.resolve_all(&file, dirs, &dirs_key);
            queue.extend(next.found.iter().filter(|f| !reach.files.contains(*f)).cloned());
            reach.missing_beside.extend(next.missing_beside.iter().cloned());
            reach.missing_names.extend(next.missing_names.iter().cloned());
        }
        reach
    }
}

/// Files a command forces into every translation unit: `-include x`.
fn forced_includes(cmds: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for cmd in cmds {
        let words = shell_words(cmd);
        let mut it = words.iter();
        while let Some(w) = it.next() {
            if matches!(w.as_str(), "-include" | "-imacros") {
                if let Some(f) = it.next() {
                    out.push(norm(f));
                }
            }
        }
    }
    out
}

fn is_c_like(path: &str) -> bool {
    matches!(
        path.rsplit('.').next(),
        Some("c" | "S" | "s" | "cc" | "cpp" | "h")
    )
}

/// Whether a command compiles or preprocesses, and so reads headers.
fn reads_headers(cmds: &[String]) -> bool {
    cmds.iter().any(|c| {
        let words = shell_words(c);
        words.iter().any(|w| w == "-c" || w == "-E") && words.iter().any(|w| w.starts_with("-I"))
    })
}

fn q(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Where this make runs, as a path below the tree root (the output root), or
/// `None` if it is not below it. `""` for the root itself.
///
/// A make started with `-C tools/objtool` names its targets relative to that
/// directory, and the tree has one namespace, so they are made relative to
/// the root before they are used as labels.
fn rel_dir() -> Option<String> {
    let root = makers_plugin::output_path("build-files")?;
    let root = resolve("/", &root);
    let cwd = resolve("/", &makers_plugin::session::working_directory());
    if cwd == root {
        return Some(String::new());
    }
    cwd.strip_prefix(&format!("{root}/")).map(str::to_string)
}

/// `name`, which this make knows relative to its own directory `rel`, as a
/// path from the tree root. `None` when it leaves the tree.
fn from_root(rel: &str, name: &str) -> Option<String> {
    if name.is_empty() || name.contains('%') {
        return None;
    }
    let name = norm(name);
    if name.starts_with('/') {
        return None;
    }
    let mut parts: Vec<&str> = Vec::new();
    let joined = format!("{rel}/{name}");
    for part in joined.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            p => parts.push(p),
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("/"))
    }
}

/// Whether the file is on disk. A dry run is no help: it gives every target
/// it would remake the placeholder timestamp `u64::MAX`, and a file that
/// `FORCE` makes it "remake" (Kbuild's `filechk` headers) is on disk all the
/// same. This needs `read-file-content`.
fn on_disk(name: &str) -> bool {
    std::fs::metadata(name).is_ok()
}

pub fn analyze(target: &Node) -> Result<(), Error> {
    let Some(recipe) = target.recipe() else {
        return Ok(());
    };
    let Some(rel) = rel_dir() else {
        return Ok(());
    };
    let local = norm(&target.name());
    // `.` is a target in Kbuild (the build directory) and has no path below
    // the root, but its recipe is the recursion that starts everything.
    let name = from_root(&rel, &local).unwrap_or_default();

    let mut cmds: Vec<(String, bool)> = Vec::new();
    let mut recursive = false;
    for line in &recipe.lines {
        let expanded = match target.expand(&line.text) {
            Ok(text) => text,
            Err(e) => {
                makers_plugin::warn(&format!("{name}: cannot expand `{}`: {}", line.text, e.message));
                return Ok(());
            }
        };
        if line.recursive {
            let lines = commands(&expanded);
            let stmts: Vec<String> = lines.iter().flat_map(|(c, _)| statements(c)).collect();
            let mixed = stmts.iter().any(|s| !starts_make(s) && !is_noise(s));
            if mixed {
                // A line that does something and then starts a make, such as
                // Kbuild's link, which runs `link-vmlinux.sh` and then the
                // make that strips relocations. The action runs all of it:
                // the make is part of how the file is produced, not a
                // separate file's rule.
                cmds.extend(lines);
                continue;
            }
            recursive = true;
            let cwd = makers_plugin::session::working_directory();
            for stmt in &stmts {
                if let Some(child) = child_identity(&cwd, stmt) {
                    RUN.with(|r| r.borrow_mut().children.insert(child.key()));
                }
            }
            continue;
        }
        cmds.extend(commands(&expanded));
    }
    // A recursive recipe is the edge to another make's output, a phony target
    // is not a file, and a target that exists is something to build from, not
    // to build.
    if name.is_empty() || target.phony() || recursive || cmds.is_empty() || on_disk(&local) {
        return Ok(());
    }

    // Targets one recipe makes together (`%.asn1.c %.asn1.h: %.asn1`): they
    // are not inputs of each other, and each is an output of an action that
    // runs the recipe.
    let siblings: Vec<String> = target
        .also_make()
        .to_list()
        .iter()
        .filter_map(|n| from_root(&rel, &norm(&n.name())))
        .filter(|n| *n != name)
        .collect();

    let mut srcs: Vec<String> = Vec::new();
    for edge in target.dep_edges() {
        if edge.flags.contains(DepFlags::ORDER_ONLY) || edge.target.phony() {
            continue;
        }
        // The name as written is empty for a prerequisite an implicit rule
        // supplied (`$(obj)/vdso-image-%.c: $(obj)/vdso%.so.dbg ...`); the
        // node it points at is the file.
        let written = if edge.name.is_empty() {
            edge.target.name()
        } else {
            edge.name.clone()
        };
        let Some(dep) = from_root(&rel, &written) else {
            continue;
        };
        if srcs.contains(&dep) || siblings.contains(&dep) {
            continue;
        }
        // Not on disk and no rule here: another make builds it (`vmlinux.a`
        // is made by the parent of the make that links it). If nobody does,
        // Bazel says so when it loads the label.
        srcs.push(dep);
    }

    let (cmds, ignore): (Vec<String>, Vec<bool>) = cmds.into_iter().unzip();
    let thin = is_thin_archive(&cmds);
    let infra = cmds.iter().any(|c| {
        c.contains(".sh") || c.split_whitespace().any(|w| w == "make" || w.ends_with("/make"))
    });
    // What the compile reads beyond the prerequisites make lists. Only
    // possible in a make that runs at the tree root, where paths in the
    // command mean the same as paths on disk.
    let mut maybe: Vec<String> = Vec::new();
    let mut maybe_names: Vec<String> = Vec::new();
    let mut maybe_dirs: Vec<String> = Vec::new();
    if rel.is_empty() && (reads_headers(&cmds) || srcs.iter().any(|s| is_c_like(s))) {
        let dirs: Vec<String> = include_dirs(&cmds).into_iter().collect();
        let mut start: Vec<String> = forced_includes(&cmds);
        start.extend(srcs.iter().filter(|s| on_disk(s)).cloned());
        let reach = RUN.with(|r| r.borrow_mut().scanner.reachable(&start, &dirs));
        for file in reach.files {
            if file != name && !srcs.contains(&file) {
                srcs.push(file);
            }
        }
        maybe = reach
            .missing_beside
            .into_iter()
            .filter(|f| *f != name && !srcs.contains(f))
            .collect();
        maybe_names = reach.missing_names.into_iter().collect();
        maybe_dirs = dirs;
    }
    // A source or header that does not exist yet (another action generates
    // it) cannot be scanned, so what it includes is unknown: such a compile
    // gets every header in the tree instead. So does one that includes a file
    // beside it that is not there, which is most likely generated too.
    let generated_source = rel.is_empty()
        && reads_headers(&cmds)
        && (srcs.iter().any(|s| is_c_like(s) && !on_disk(s)) || !maybe.is_empty());
    // Commands are relative to the make's own directory. The action starts
    // at the tree root, so a make that ran elsewhere goes there for them.
    let cmds: Vec<String> = cmds
        .into_iter()
        .zip(ignore)
        .map(|(c, ign)| {
            let c = if ign { format!("{c} || true") } else { c };
            if rel.is_empty() {
                c
            } else {
                format!("(cd '{rel}' && {c})")
            }
        })
        .collect();

    RUN.with(|r| {
        let mut run = r.borrow_mut();
        if infra && run.env.is_none() {
            match target.recipe_environment() {
                Ok(env) => run.env = Some(env),
                Err(e) => makers_plugin::warn(&format!(
                    "{name}: cannot read the recipe environment: {}",
                    e.message
                )),
            }
        }
        for sibling in siblings {
            run.rules.push(Rule {
                out: sibling,
                srcs: srcs.clone(),
                cmds: cmds.clone(),
                thin,
                env: infra,
                infra: infra || generated_source,
                maybe: maybe.clone(),
                maybe_names: maybe_names.clone(),
                maybe_dirs: maybe_dirs.clone(),
            });
        }
        run.rules.push(Rule {
            out: name,
            srcs,
            cmds,
            thin,
            env: infra,
            infra: infra || generated_source,
            maybe,
            maybe_names,
            maybe_dirs,
        });
    });
    Ok(())
}

fn write(logical: &str, path: &str, body: &str) -> Result<(), Error> {
    let mut out = makers_plugin::open_output_in(logical, path)?;
    out.write_all(body.as_bytes())
        .map_err(|e| makers_plugin::fail(e.to_string()))?;
    out.finish()
}

/// Where the generated files go: `out.build-files`, an absolute path.
fn out_root() -> String {
    super::setting("out.build-files").unwrap_or_else(|| ".".to_string())
}

/// The registry's text. The plugin only sees the working directory on disk,
/// so a path under it is opened relative to it.
fn read_registry() -> String {
    let out = out_root();
    let cwd = makers_plugin::session::working_directory();
    let rel = if out == cwd || out == "." {
        "kbuild/makes.list".to_string()
    } else if let Some(below) = out.strip_prefix(&format!("{cwd}/")) {
        format!("{below}/kbuild/makes.list")
    } else {
        format!("{out}/kbuild/makes.list")
    };
    std::fs::read_to_string(rel).unwrap_or_default()
}

/// Every make records itself, and the makes its recipes start, in
/// `kbuild/makes.list`, one line each: `key child child ...`. A make that a
/// script starts (Kbuild's link runs one for `init/version-timestamp.o`) is
/// named by no recipe, so it appears here and nowhere else; the root loads
/// such makes beside the ones it reaches. Makes run one after another in a
/// dry run, so appending is safe.
fn register(key: &str, children: &BTreeSet<String>) -> Result<(), Error> {
    let mut text = read_registry();
    text.push_str(key);
    for child in children {
        text.push(' ');
        text.push_str(child);
    }
    text.push('\n');
    write("build-files", "kbuild/makes.list", &text)
}

/// The makes nothing reaches from `root`: those a script started.
fn orphans(root: &str) -> Vec<String> {
    let text = read_registry();
    let mut edges: std::collections::BTreeMap<&str, BTreeSet<&str>> = Default::default();
    for line in text.lines() {
        let mut words = line.split_whitespace();
        if let Some(key) = words.next() {
            edges.entry(key).or_default().extend(words);
        }
    }
    let mut reached: BTreeSet<&str> = BTreeSet::new();
    let mut queue = vec![root];
    while let Some(k) = queue.pop() {
        if reached.insert(k) {
            if let Some(kids) = edges.get(k) {
                queue.extend(kids.iter().copied());
            }
        }
    }
    edges
        .keys()
        .filter(|k| !reached.contains(*k))
        .map(|k| k.to_string())
        .collect()
}

pub fn finish() -> Result<(), Error> {
    let mut run = RUN.with(|r| std::mem::take(&mut *r.borrow_mut()));
    let me = this_identity();
    let key = me.key();
    // The outer make Kbuild re-executes itself under is the same make as the
    // one it starts, so it names itself as its own child. It has nothing to
    // say, and its fragment would overwrite the one that does.
    let wrapper = run.rules.is_empty() && run.children.len() == 1 && run.children.contains(&key);
    run.children.remove(&key);
    if wrapper {
        return Ok(());
    }
    let func = format!("kbuild_{key}");

    let mut doc = String::from(
        "# Generated by the `bazel-export` makers plugin (mode=kbuild).\n\
         # Edit the makefiles, not this file.\n\n\
         load(\"//:kbuild/rules.bzl\", \"kbuild_spec\")\n",
    );
    for child in &run.children {
        doc.push_str(&format!(
            "load(\"//:kbuild/{child}.bzl\", \"kbuild_{child}\")\n"
        ));
    }
    if let Some(env) = &run.env {
        doc.push_str("\nKBUILD_ENV = {\n");
        for (k, v) in env {
            doc.push_str(&format!("    {}: {},\n", q(k), q(v)));
        }
        doc.push_str("}\n");
    }
    // Search directories repeat from action to action; name each set once.
    let mut dir_sets: Vec<&Vec<String>> = Vec::new();
    for rule in &run.rules {
        if !rule.maybe_names.is_empty() && !dir_sets.contains(&&rule.maybe_dirs) {
            dir_sets.push(&rule.maybe_dirs);
        }
    }
    for (i, dirs) in dir_sets.iter().enumerate() {
        doc.push_str(&format!("\nDIRS_{i} = [\n"));
        for d in dirs.iter() {
            doc.push_str(&format!("    {},\n", q(d)));
        }
        doc.push_str("]\n");
    }
    // A fragment returns its targets and its children's instead of declaring
    // them, so `kbuild_declare` sees all of them at once.
    doc.push_str(&format!("\ndef {func}():\n    specs = []\n"));
    for rule in &run.rules {
        doc.push_str("    specs.append(kbuild_spec(\n");
        doc.push_str(&format!("        name = {},\n", q(&rule_name(&rule.out))));
        doc.push_str(&format!("        out = {},\n", q(&rule.out)));
        doc.push_str("        srcs = [\n");
        for s in &rule.srcs {
            doc.push_str(&format!("            {},\n", q(s)));
        }
        doc.push_str("        ],\n        cmds = [\n");
        for c in &rule.cmds {
            doc.push_str(&format!("            {},\n", q(c)));
        }
        doc.push_str("        ],\n");
        if !rule.maybe.is_empty() {
            doc.push_str("        maybe = [\n");
            for m in &rule.maybe {
                doc.push_str(&format!("            {},\n", q(m)));
            }
            doc.push_str("        ],\n");
        }
        if !rule.maybe_names.is_empty() {
            doc.push_str("        maybe_names = [\n");
            for n in &rule.maybe_names {
                doc.push_str(&format!("            {},\n", q(n)));
            }
            doc.push_str("        ],\n");
            let i = dir_sets.iter().position(|d| **d == rule.maybe_dirs).unwrap_or(0);
            doc.push_str(&format!("        maybe_dirs = DIRS_{i},\n"));
        }
        if rule.env && run.env.is_some() {
            doc.push_str("        env = KBUILD_ENV,\n");
        }
        if rule.thin {
            doc.push_str("        thin = True,\n");
        }
        if rule.infra {
            doc.push_str("        infra = True,\n");
        }
        doc.push_str("    ))\n");
    }
    for child in &run.children {
        doc.push_str(&format!("    specs += kbuild_{child}()\n"));
    }
    doc.push_str("    return specs\n");
    write("build-files", &format!("kbuild/{key}.bzl"), &doc)?;
    write("build-files", "kbuild/rules.bzl", RULES_BZL)?;
    register(&key, &run.children)?;

    if me.is_root() {
        let outs: Vec<String> = super::setting("outs")
            .map(|o| o.split(',').map(|s| norm(s.trim())).filter(|s| !s.is_empty()).collect())
            .unwrap_or_default();
        if outs.is_empty() {
            return Err(makers_plugin::fail(
                "mode=kbuild needs `outs`: the files the build produces",
            ));
        }
        let name = super::setting("name").unwrap_or_else(|| "kernel".to_string());
        let orphans = orphans(&key);
        let mut loads = format!("load(\"//:kbuild/{key}.bzl\", \"kbuild_{key}\")\n");
        let mut calls = format!("kbuild_{key}()");
        for o in &orphans {
            loads.push_str(&format!("load(\"//:kbuild/{o}.bzl\", \"kbuild_{o}\")\n"));
            calls.push_str(&format!(" + kbuild_{o}()"));
        }
        let build = format!(
            "# Generated by the `bazel-export` makers plugin (mode=kbuild).\n\
             load(\"//:kbuild/rules.bzl\", \"kbuild_declare\")\n\
             {loads}\n\
             package(default_visibility = [\"//visibility:public\"])\n\n\
             kbuild_declare({calls})\n\n\
             filegroup(\n    name = {},\n    srcs = [{}],\n)\n",
            q(&name),
            outs.iter().map(|o| q(o)).collect::<Vec<_>>().join(", ")
        );
        write("build-files", "BUILD.bazel", &build)?;
        write(
            "build-files",
            "MODULE.bazel",
            &format!("module(name = {})\n", q(&ident(&name))),
        )?;
    }
    makers_plugin::note(&format!(
        "{key}: {} rule(s), {} child make(s)",
        run.rules.len(),
        run.children.len()
    ));
    Ok(())
}
