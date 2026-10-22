# The make for one directory of the kbuild fixture.
cmd = $(cmd_$(1))
cmd_cc = cc -c -o $@ $<
define rule_cc
	$(call cmd,cc); scripts/basic/fixdep $@.d $@ 'cmd_$@'; rm -f $@.d
endef

sub/lib.a: sub/a.o
	rm -f $@; ar cDPrST $@ sub/a.o

sub/a.o: sub/a.c
	$(call rule_cc)
