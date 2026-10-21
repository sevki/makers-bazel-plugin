# Fixture for mode=kbuild, shaped like Kbuild.
#
# The top-level make recurses into one make per directory, and a command is
# built by make functions from `$@` and `$<`, so it is only a command line
# once expanded for its target. `rule_cc` ends the way Kbuild's does, with a
# `fixdep` step that records dependencies for the next run and which Bazel
# has no use for.
cmd = $(cmd_$(1))
cmd_cc = cc -DFLAG -c -o $@ $<
define rule_cc
	$(call cmd,cc); scripts/basic/fixdep $@.d $@ 'cmd_$@'; rm -f $@.d
endef
export KBUILD_FLAG := set

.PHONY: all sub
all: sub app

app: main.o sub/lib.a
	cc -o $@ main.o sub/lib.a

main.o: main.c shared.h
	$(call rule_cc)

# The archive is the sub-make's to build; here it only waits for it, as
# Kbuild's `$(obj)/%/built-in.a: $(obj)/% ;` does.
sub/lib.a: sub ;

# A line that does something and then starts a make, like Kbuild's link: the
# make is part of producing the file, not a rule of its own.
all: linked
linked: main.o
	+cc -o $@ main.o; $(MAKE) -f sub.mk obj=post

# A directory's own make, as Kbuild starts it.
sub:
	+$(MAKE) -f sub.mk obj=sub
