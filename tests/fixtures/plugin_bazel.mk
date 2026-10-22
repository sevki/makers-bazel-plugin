# Fixture for the Bazel export plugin.
#
# Targets in two directories (so more than one BUILD.bazel is emitted), an
# order-only prerequisite (which is an ordering constraint, not an input),
# phony targets (which are not files Bazel can produce), and recipes
# exercising each `$` form the two tools disagree about.
CC := cc

.PHONY: all clean
all: prog stamp

prog: main.o src/util.o
	$(CC) -o $@ $^

main.o: main.c | build
	$(CC) -c -o $@ $<

src/util.o: src/util.c | build
	$(CC) -c -o $@ $< $(call extra_flags,util)

stamp: prog
	echo "built at $$(date -u)" > $@

build:
	mkdir -p build

clean:
	rm -f prog main.o src/util.o stamp
