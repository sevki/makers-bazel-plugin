# Fixture for the `read-environment` gate on variable reads.
#
# Deliberately leaves `EXTRA_CFLAGS` undefined: a name the makefile only
# references is the one a plugin can be made to resolve from the environment,
# because a makefile assignment would win over the environment anyway (and so
# would carry origin `file`, which the digest already covers).
CC := cc

.PHONY: all
all: probe.o

probe.o: probe.c
	$(CC) -c -o $@ $< $(EXTRA_CFLAGS)
