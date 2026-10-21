# makers-bazel-plugin

A [makers](https://github.com/sevki/makers) build plugin that generates Bazel
files from a resolved `make` graph. It is a WebAssembly component written
against the [`makers-plugin`](https://github.com/sevki/makers/tree/main/makers-plugin)
SDK; see the makers `docs/plugin-api.md` for the plugin interface.

It has two modes, in two builds.

## Per-target (default)

One `BUILD.bazel` per directory, one `genrule` per recipe-bearing file target.
Order-only prerequisites stay out of `srcs`, plain variables are substituted
in the target's scope, and `$@`/`$<`/`$^` are translated to Bazel's spelling.
This is right for a makefile whose whole graph lives in one `make` process.

## `mode=kbuild`: the Linux kernel, one Bazel action per file

Kbuild is one `make` per directory (`$(MAKE) -f scripts/Makefile.build
obj=kernel/sched`), so no single run sees the whole graph, and the command for
`kernel/fork.o` only exists after `$(call if_changed_rule,cc_o_c)` has been
expanded against `$@` and `$<`. Built with `--features kbuild`, the plugin asks
the makers host to expand each target's recipe in that target's scope
(`node.expand`, from [sevki/makers#687](https://github.com/sevki/makers/pull/687)),
runs in every make Kbuild starts, and writes the Bazel files itself:

- each directory's make writes `kbuild/<key>.bzl`, a macro returning one `kbuild_spec`
  per target it would build, and a call to the macro of each make it starts;
- the top-level make writes `BUILD.bazel`, `MODULE.bazel` and `kbuild/rules.bzl`, and loads every make no parent starts (a script's make, such as the link's) from the registry `kbuild/makes.list`;
- each action runs Kbuild's own command (cut before the `fixdep` step that
  records dependencies for Kbuild's next run), from the tree root. Its inputs
  are the prerequisites make lists plus what the compile reads: the plugin
  follows `#include` from the source through the `-I` directories, as `fixdep`
  does afterwards;
- a target that exists on disk, such as a generated header after `make prepare`,
  is an input, not a rule.

Settings (`--plugin-arg bazel:<key>=<value>`):

| key               | meaning                                                          |
|-------------------|------------------------------------------------------------------|
| `mode`            | `kbuild`                                                         |
| `goals`           | the top-level goals, as given to `make`; identifies the root run |
| `outs`            | files the goals produce, gathered by `//:kernel`                 |
| `name`            | name of that target (default `kernel`)                           |
| `out.build-files` | **absolute** path of the tree, so makes started with `-C` agree  |
| `fuel`            | raise it: following includes is more work than a plugin usually does |

Grant `MAKERS_PLUGIN_ALLOW=bazel:expand-variables,read-file-content`.

The tree must be configured and prepared first (`make defconfig prepare`), and
the plugin run is a dry run of the goals. A dry run still *executes* a recipe
line that starts a make, which includes the `vmlinux` link; see
[bazel-linux](https://github.com/sevki/bazel-linux) for how that is handled.

Verified on Linux 6.6.147: for `kernel/`, Bazel builds `kernel/built-in.a`
from the generated files and all 207 objects are byte-identical to Kbuild's.
The full `vmlinux`/`bzImage` run is not yet verified.

## Building and testing

```sh
cargo install cargo-component --locked
rustup target add wasm32-wasip1
cargo component build        # per-target mode; target/wasm32-wasip1/debug/makers_bazel_plugin.wasm
cargo component build --features kbuild --target-dir target-kbuild   # kbuild mode

# tests run the real makers `make`, built with the plugin host:
cargo install --git https://github.com/sevki/makers make-sys --features wasmtime --bin make --locked
MAKERS_MAKE=$(which make) cargo test   # or the path to that binary
```

Use `make` from makers (`--features wasmtime`) to load the component.
