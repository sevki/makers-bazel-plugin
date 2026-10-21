# makers-bazel-plugin

A [makers](https://github.com/sevki/makers) build plugin that generates Bazel
files from a resolved `make` graph. It is a WebAssembly component written
against the [`makers-plugin`](https://github.com/sevki/makers/tree/main/makers-plugin)
SDK; see the makers `docs/plugin-api.md` for the plugin interface.

It has two modes.

## Per-target (default)

One `BUILD.bazel` per directory, one `genrule` per recipe-bearing file target.
Order-only prerequisites stay out of `srcs`, plain variables are substituted
in the target's scope, and `$@`/`$<`/`$^` are translated to Bazel's spelling.
This is right for a makefile whose whole graph lives in one `make` process.

## `mode=wrap`: recursive makes, including the Linux kernel

Kbuild recurses (`$(MAKE) -f scripts/Makefile.build obj=<dir>`), so the
top-level graph is a fragment, its recipes (`$(call if_changed_rule,cc_o_c)`)
only mean something inside the sub-make, and its thin `built-in.a` archives
name `.o` files a per-target action would not have kept. `mode=wrap` leaves
Kbuild's graph to Kbuild and emits `MODULE.bazel` plus one hermetic `kernel`
genrule that copies the tree, runs `make`, and returns the declared outputs
(the same trade Android's Kleaf makes). It is not incremental: any source
change rebuilds the kernel.

Settings, as `--plugin-arg bazel:<key>=<value>`:

| key         | meaning                                                    |
|-------------|------------------------------------------------------------|
| `mode`      | `wrap` to enable                                           |
| `outs`      | comma-separated files the build produces and Bazel returns |
| `goals`     | space-separated make goals (default: the command line's)   |
| `configure` | make goals run first, e.g. `defconfig`                     |
| `make`      | the make binary (default `make`)                           |
| `make-args` | extra make arguments, e.g. `ARCH=x86_64 LLVM=1`            |
| `jobs`      | `-j` value (default: all cores)                            |
| `exclude`   | comma-separated extra globs left out of the action inputs  |
| `name`      | rule name (default `kernel`)                               |

Verified on Linux 6.6.147 with Bazel 9.3.0 (`vmlinux` and `arch/x86/boot/bzImage`):

```sh
cd linux-6.6.147
MAKERS_PLUGINS=bazel=/path/to/makers_bazel_plugin.wasm \
MAKERS_PLUGIN_ARGS='bazel:mode=wrap;bazel:outs=vmlinux,arch/x86/boot/bzImage;bazel:goals=vmlinux bzImage;bazel:configure=defconfig' \
  make -n help          # a goal that does not recurse; writes MODULE.bazel and BUILD.bazel
bazel build //:kernel --action_env=PATH
```

## Building and testing

```sh
cargo install cargo-component --locked
rustup target add wasm32-wasip1
cargo component build        # target/wasm32-wasip1/debug/makers_bazel_plugin.wasm

# tests run the real makers `make`, built with the plugin host:
cargo install --git https://github.com/sevki/makers make-sys --features wasmtime --bin make --locked
MAKERS_MAKE=$(which make) cargo test   # or the path to that binary
```

Use `make` from makers (`--features wasmtime`) to load the component.
