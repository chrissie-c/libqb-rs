# ── Rust library ─────────────────────────────────────────────────────────────
#
# Cargo builds into $(CARGO_TARGET_DIR)/debug/.  We set that to
# $(abs_builddir)/target so out-of-source builds work cleanly.

CARGO_TARGET_DIR = $(abs_builddir)/target
export CARGO_TARGET_DIR

RUST_LIB = $(CARGO_TARGET_DIR)/debug/libqb.so

# cargo handles its own incrementality; declare the rule phony so cargo always
# gets to decide whether a rebuild is needed.  The library file's mtime is only
# bumped by cargo when it actually recompiles, so test-program re-links happen
# only when the library genuinely changed.
.PHONY: cargo-build
cargo-build:
	cd $(top_srcdir) && CARGO_TARGET_DIR=$(CARGO_TARGET_DIR) $(CARGO) build

$(RUST_LIB): cargo-build

all-local: $(RUST_LIB)

# ── Test programs ─────────────────────────────────────────────────────────────
#
# Tests are compiled with subdir-objects: source lives under test/, object
# files are placed there too (or in the mirror build-dir for VPATH builds).
# Automake derives per-program variable names by replacing non-alphanumeric
# characters with underscores: test/check_array → test_check_array.

AM_CPPFLAGS = \
    -I$(top_builddir)          \
    -I$(top_srcdir)/include    \
    -I$(top_srcdir)/test       \
    -D_GNU_SOURCE              \
    -DHAVE_CONFIG_H            \
    $(CHECK_CFLAGS)

AM_CFLAGS   = -g -O0 -Wall -fmacro-prefix-map=test/=
AM_LDFLAGS  = -L$(CARGO_TARGET_DIR)/debug \
              -Wl,-rpath,$(CARGO_TARGET_DIR)/debug

# Libraries added to every test program link command.
LDADD = -lqb $(CHECK_LIBS) -lpthread -lm -lrt

check_PROGRAMS = \
    test/check_array  \
    test/check_ipc    \
    test/check_list   \
    test/check_log    \
    test/check_loop   \
    test/check_map    \
    test/check_rb     \
    test/check_tlist  \
    test/check_util

test_check_array_SOURCES  = test/check_array.c
test_check_ipc_SOURCES    = test/check_ipc.c test/_failure_injection.c
test_check_ipc_CPPFLAGS   = $(AM_CPPFLAGS) -DRUST_LIBQB
test_check_ipc_LDFLAGS    = $(AM_LDFLAGS) -ldl
test_check_list_SOURCES   = test/check_list.c
test_check_log_SOURCES    = test/check_log.c test/_syslog_override.c
test_check_loop_SOURCES   = test/check_loop.c
test_check_map_SOURCES    = test/check_map.c
test_check_rb_SOURCES     = test/check_rb.c
test_check_tlist_SOURCES  = test/check_tlist.c
test_check_util_SOURCES   = test/check_util.c

# Each test binary must wait for the Rust library to be ready.
$(check_PROGRAMS): $(RUST_LIB)

# ── Test runner ───────────────────────────────────────────────────────────────

TESTS = $(check_PROGRAMS)

# LD_LIBRARY_PATH lets the dynamic linker find libqb.so at runtime.
# libcheck 0.15 reads $TEMP (not $TMPDIR) when choosing where to create its
# IPC temp file.  If $TEMP is unset it falls back to the hardcoded "/tmp",
# which may be read-only.  We forward $$TMPDIR (the shell var, double-dollar
# so automake passes a literal "$TMPDIR" to the shell at test runtime) so that
# both libcheck and the test programs use the writable sandbox temp directory.
TESTS_ENVIRONMENT = \
    LD_LIBRARY_PATH=$(CARGO_TARGET_DIR)/debug \
    TEMP=$${TMPDIR:-/tmp}

# 'make test' is a user-friendly alias for automake's 'make check'.
.PHONY: test
test: check

# ── Cleaning ──────────────────────────────────────────────────────────────────

clean-local:
	-$(CARGO) clean
	-rm -rf $(CARGO_TARGET_DIR)

# ── Distribution tarball extras ───────────────────────────────────────────────

EXTRA_DIST = \
    src          \
    include      \
    test         \
    Cargo.toml   \
    Cargo.lock   \
    build.rs     \
    cbindgen.toml \
    libqb.pc
