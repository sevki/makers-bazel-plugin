"""Rules for a Bazel workspace generated from a recursive make (Kbuild).

Written by the `bazel-export` makers plugin in `mode=kbuild`; edit the
makefiles, not this file.

Every action runs the command make would have run for one target, from the
workspace root, so the paths in it (`kernel/fork.c`, `-I./include`) mean what
they meant to make. Bazel keeps sources and outputs in different trees, so an
action first links each generated input to its workspace-relative path.
"""

KbuildInfo = provider(
    doc = "What a thin archive needs beside itself to be linked.",
    fields = {"members": "depset of the files a thin archive refers to by path"},
)

def _link_lines(f):
    # Sources are already where the command expects them; only generated
    # inputs need a symlink at their workspace-relative path.
    if f.is_source:
        return None
    return [f.path, f.short_path]

def _quote(s):
    return "'" + s.replace("'", "'\\''") + "'"

def _kbuild_action_impl(ctx):
    out = ctx.outputs.out
    members = [d[KbuildInfo].members for d in ctx.attr.srcs if KbuildInfo in d]
    inputs = depset(ctx.files.srcs, transitive = members)

    links = ctx.actions.declare_file(ctx.label.name + ".links")
    args = ctx.actions.args()
    args.add_all(inputs, map_each = _link_lines)
    ctx.actions.write(links, args)

    script = ctx.actions.declare_file(ctx.label.name + ".sh")
    lines = [
        "set -e",
        'while IFS= read -r p && IFS= read -r l; do mkdir -p "$(dirname "$l")"; ln -sf "$PWD/$p" "$l"; done < ' + _quote(links.path),
        "mkdir -p " + _quote("/".join(ctx.attr.out.split("/")[:-1]) or "."),
    ]
    # The compiler is told to treat a missing -I directory as an error, and a
    # directory exists here only if some input is in it.
    for d in ctx.attr.dirs:
        lines.append("mkdir -p " + _quote(d))
    for k, v in sorted(ctx.attr.env.items()):
        lines.append("export %s=%s" % (k, '"$PWD"' if v == "$PWD" else _quote(v)))
    lines += ctx.attr.cmds
    lines.append("cp " + _quote(ctx.attr.out) + " " + _quote(out.path))
    ctx.actions.write(script, "\n".join(lines) + "\n")

    ctx.actions.run_shell(
        inputs = depset([links, script], transitive = [inputs]),
        outputs = [out],
        command = "sh " + _quote(script.path),
        mnemonic = "Kbuild",
        progress_message = "KBUILD %s" % ctx.attr.out,
        use_default_shell_env = True,
    )
    providers = [DefaultInfo(files = depset([out]))]
    if ctx.attr.thin:
        providers.append(KbuildInfo(members = depset(ctx.files.srcs, transitive = members)))
    return providers

_kbuild_action = rule(
    implementation = _kbuild_action_impl,
    # Predeclared, so that other targets can name the output by its path.
    outputs = {"out": "%{out}"},
    attrs = {
        "srcs": attr.label_list(allow_files = True),
        "out": attr.string(mandatory = True),
        "cmds": attr.string_list(),
        "env": attr.string_dict(),
        "thin": attr.bool(default = False),
        "dirs": attr.string_list(),
    },
)

def kbuild_spec(name, out, srcs = [], cmds = [], env = {}, thin = False, infra = False, maybe = [], maybe_names = [], maybe_dirs = [], group = []):
    """One target of the build: run `cmds` to produce `out` from `srcs`.

    This only describes the target; `kbuild_declare` creates it, once it has
    every target to hand.

    Args:
      name: rule name; a second target of the same name is ignored, since a
        make may be invoked more than once for the same directory.
      out: the file the commands produce, relative to the workspace root.
      srcs: prerequisites, as labels.
      cmds: the shell commands make would run, one per element.
      env: variables make exported to the recipe.
      thin: the commands create a thin archive, which names its members by
        path, so they must be present when it is used.
      infra: the commands run build scripts that read the rest of the build
        system (`scripts/`, the top-level makefiles, the configuration).
      maybe: files the sources include that are not on disk, as paths. Each
        that another target produces is also a source.
      maybe_names: include names that were not found in any of `maybe_dirs`;
        `maybe_dirs/name` is also a source where another target produces it.
      group: every output of the recipe, when it makes several; whoever
        uses one of them gets them all as inputs.
      maybe_dirs: the include search directories, relative to the root; the
        action creates them, since the compiler fails on one that is missing.
    """
    return struct(
        name = name,
        out = out,
        srcs = srcs,
        cmds = cmds,
        env = env,
        thin = thin,
        infra = infra,
        maybe = maybe,
        maybe_names = maybe_names,
        maybe_dirs = maybe_dirs,
        group = group,
    )

def kbuild_declare(specs):
    """Declares the targets `kbuild_spec` described.

    A header that a compile includes may be generated by a target in a
    different make, which the compile's own make cannot see; it becomes a
    source here, where every target's output is known.
    """
    outs = {s.out: True for s in specs}
    groups = {s.out: s.group for s in specs if s.group}
    seen = {}
    infra = False
    for s in specs:
        if s.name in seen:
            continue
        seen[s.name] = True
        srcs = list(s.srcs)
        have = {x: True for x in srcs}
        for m in s.maybe:
            if m in outs and m != s.out and m not in have:
                srcs.append(m)
                have[m] = True
        for n in s.maybe_names:
            for d in s.maybe_dirs:
                m = d + "/" + n
                if m in outs and m != s.out and m not in have:
                    srcs.append(m)
                    have[m] = True
        for x in list(srcs):
            for g in groups.get(x, []):
                if g not in have and g != s.out:
                    srcs.append(g)
                    have[g] = True
        if s.infra:
            infra = True
            srcs.append(":kbuild_infra")
        _kbuild_action(
            name = s.name,
            out = s.out,
            srcs = srcs,
            cmds = s.cmds,
            env = s.env,
            thin = s.thin,
            dirs = s.maybe_dirs,
        )
    if infra:
        native.filegroup(
            name = "kbuild_infra",
            srcs = native.glob([
                "scripts/**",
                "Makefile",
                "Kbuild",
                "arch/*/Makefile*",
                "arch/*/*.mk",
                "include/**",
                "arch/*/include/**",
                "init/*",
                ".config",
                ".version",
            ], allow_empty = True),
        )
