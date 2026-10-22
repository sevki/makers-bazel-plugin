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
# Read by a script, and not exported, so the recipe's environment lacks it.
script_only := yes

.PHONY: all sub
all: sub app gen.h img-out.gen grp.c1 grp.h1 gen2.o plain.o miss.o ord.o grp.o listed.out flask.h flask2.h other.h hid.h used.out tools/mk disk.out inc.o gen.bin

# A header the compile includes but does not list as a prerequisite, as in
# Kbuild, where building it first is a matter of ordering.
gen.h:
	echo '#define GEN 1' > $@

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

# A prerequisite an implicit rule supplies has no name as written in the
# makefile. As in the vdso Makefile, the stem sits inside a word of the
# prerequisite (`vdso%.so.dbg`), which another rule generates.
img-%.gen: pre%.x
	cp $< $@

preout.x:
	echo pre > $@

# One recipe makes two files, as Kbuild's `%.asn1.c %.asn1.h: %.asn1` does.
%.c1 %.h1: %.src
	echo gen $*

grp.src:
	echo src > $@

# A compile of a source another action generates: what it includes cannot be
# read until it exists.
gen2.o: gen2.c
	cc -c -I. -o $@ $<

gen2.c:
	echo > $@

# A compile of a source that is there and includes nothing missing.
plain.o: plain.c
	cc -c -I. -o $@ $<

# A compile of a source that includes a file beside it that is not there.
miss.o: miss.c
	cc -c -I. -o $@ $<

# Two search directories that both have the header: the compiler takes the
# first one on the command line, which is not the first alphabetically.
ord.o: ord.c
	cc -c -Izdir -Iadir -o $@ $<

# A consumer of one output of a group is a consumer of the group: the source
# includes the header its recipe made beside it.
grp.o: grp.c1
	cc -c -o $@ $<

# A script handed a file by name, which make does not list as a prerequisite.
listed.out:
	sh ./run.sh ./list.txt > $@

# One command makes two headers, and make is told of only one, as Kbuild's
# `genheaders flask.h av_permissions.h` does.
flask.h:
	echo gen flask.h perms.h

# A command that names a header with a rule of its own, as `bzImage`'s names
# `zoffset.h`: that header is not an output of this action, or whoever needs
# it would need this too.
flask2.h:
	echo gen flask2.h other.h

other.h:
	echo other > $@

# `filechk` writes a hidden temporary beside its target and moves it into
# place; that is not an output.
hid.h:
	echo gen hid.h .tmp_hid.h

# A command runs a tool that another rule builds, which is not a
# prerequisite, as `realmode.relocs` runs `arch/x86/tools/relocs`.
used.out:
	tools/mk used > $@

tools/mk:
	echo tool > $@

# A tool that is already built, as `make prepare` leaves arch/x86/tools/relocs:
# it is on disk, so no rule makes it, and the command still needs it.
disk.out:
	tools/ondisk > $@

# Assembly that embeds files by name, as arch/x86/realmode/rmpiggy.S does.
inc.o: inc.S
	cc -c -I. -o $@ $<

gen.bin:
	echo bin > $@
