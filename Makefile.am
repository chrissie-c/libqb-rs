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

# ── Man page generation ───────────────────────────────────────────────────────
#
# Mirrors the libqb docs/Makefile.am pattern: doxygen generates XML from the
# public headers, then doxygen2man converts each XML file to groff man pages.
# Both tools are detected by configure; the section is skipped when either is
# absent.  Run `make man` to regenerate explicitly.

if HAVE_DOXYGEN
if BUILD_MAN

doxygen2man_flags = \
	-c -g -m -P -q \
	-i qb/ \
	-o man3/ \
	-H "libqb Programmer's Manual" \
	-p LIBQB \
	-O $(abs_top_srcdir)/include/qb

man3_MANS = \
	docs/man3/qb_array_create_2.3 \
	docs/man3/qb_array_create.3 \
	docs/man3/qb_array_elems_per_bin_get.3 \
	docs/man3/qb_array_free.3 \
	docs/man3/qb_array_grow.3 \
	docs/man3/qbarray.h.3 \
	docs/man3/qb_array_index.3 \
	docs/man3/qb_array_new_bin_cb_set.3 \
	docs/man3/qb_array_num_bins_get.3 \
	docs/man3/qbatomic.h.3 \
	docs/man3/qb_atomic_init.3 \
	docs/man3/qb_atomic_int_add.3 \
	docs/man3/qb_atomic_int_compare_and_exchange.3 \
	docs/man3/qb_atomic_int_exchange_and_add.3 \
	docs/man3/qb_atomic_int_get.3 \
	docs/man3/qb_atomic_int_set.3 \
	docs/man3/qb_atomic_pointer_compare_and_exchange.3 \
	docs/man3/qb_atomic_pointer_get.3 \
	docs/man3/qb_atomic_pointer_set.3 \
	docs/man3/qbdefs.h.3 \
	docs/man3/qb_hashtable_create.3 \
	docs/man3/qb_hdb_base_convert.3 \
	docs/man3/qb_hdb_create.3 \
	docs/man3/qb_hdb_destroy.3 \
	docs/man3/qbhdb.h.3 \
	docs/man3/qb_hdb_handle_create.3 \
	docs/man3/qb_hdb_handle_destroy.3 \
	docs/man3/qb_hdb_handle_get.3 \
	docs/man3/qb_hdb_handle_get_always.3 \
	docs/man3/qb_hdb_handle_put.3 \
	docs/man3/qb_hdb_handle_refcount_get.3 \
	docs/man3/qb_hdb_iterator_next.3 \
	docs/man3/qb_hdb_iterator_reset.3 \
	docs/man3/qb_hdb_nocheck_convert.3 \
	docs/man3/qb_ipcc_auth_get.3 \
	docs/man3/qb_ipcc_connect.3 \
	docs/man3/qb_ipcc_connect_async.3 \
	docs/man3/qb_ipcc_connect_continue.3 \
	docs/man3/qb_ipcc_context_get.3 \
	docs/man3/qb_ipcc_context_set.3 \
	docs/man3/qb_ipcc_disconnect.3 \
	docs/man3/qb_ipcc_event_recv.3 \
	docs/man3/qb_ipcc_fc_enable_max_set.3 \
	docs/man3/qb_ipcc_fd_get.3 \
	docs/man3/qb_ipcc_get_buffer_size.3 \
	docs/man3/qbipcc.h.3 \
	docs/man3/qb_ipcc_is_connected.3 \
	docs/man3/qbipc_common.h.3 \
	docs/man3/qb_ipcc_recv.3 \
	docs/man3/qb_ipcc_send.3 \
	docs/man3/qb_ipcc_sendv.3 \
	docs/man3/qb_ipcc_sendv_recv.3 \
	docs/man3/qb_ipcc_verify_dgram_max_msg_size.3 \
	docs/man3/qb_ipcs_connection_auth_set.3 \
	docs/man3/qb_ipcs_connection_first_get.3 \
	docs/man3/qb_ipcs_connection_get_buffer_size.3 \
	docs/man3/qb_ipcs_connection_next_get.3 \
	docs/man3/qb_ipcs_connection_ref.3 \
	docs/man3/qb_ipcs_connection_service_context_get.3 \
	docs/man3/qb_ipcs_connection_stats_get_2.3 \
	docs/man3/qb_ipcs_connection_stats_get.3 \
	docs/man3/qb_ipcs_connection_unref.3 \
	docs/man3/qb_ipcs_context_get.3 \
	docs/man3/qb_ipcs_context_set.3 \
	docs/man3/qb_ipcs_create.3 \
	docs/man3/qb_ipcs_destroy.3 \
	docs/man3/qb_ipcs_disconnect.3 \
	docs/man3/qb_ipcs_enforce_buffer_size.3 \
	docs/man3/qb_ipcs_event_send.3 \
	docs/man3/qb_ipcs_event_sendv.3 \
	docs/man3/qbipcs.h.3 \
	docs/man3/qb_ipcs_poll_handlers_set.3 \
	docs/man3/qb_ipcs_ref.3 \
	docs/man3/qb_ipcs_request_rate_limit.3 \
	docs/man3/qb_ipcs_response_send.3 \
	docs/man3/qb_ipcs_response_sendv.3 \
	docs/man3/qb_ipcs_run.3 \
	docs/man3/qb_ipcs_service_context_get.3 \
	docs/man3/qb_ipcs_service_context_set.3 \
	docs/man3/qb_ipcs_service_id_get.3 \
	docs/man3/qb_ipcs_stats_get.3 \
	docs/man3/qb_ipcs_unref.3 \
	docs/man3/qb_list_add.3 \
	docs/man3/qb_list_add_tail.3 \
	docs/man3/qb_list_del.3 \
	docs/man3/qb_list_empty.3 \
	docs/man3/qblist.h.3 \
	docs/man3/qb_list_init.3 \
	docs/man3/qb_list_is_last.3 \
	docs/man3/qb_list_length.3 \
	docs/man3/qb_list_replace.3 \
	docs/man3/qb_list_splice.3 \
	docs/man3/qb_list_splice_tail.3 \
	docs/man3/qb_log_blackbox_print_from_file.3 \
	docs/man3/qb_log_blackbox_write_to_file.3 \
	docs/man3/qb_log_callsite_get.3 \
	docs/man3/qb_log_callsite_get2.3 \
	docs/man3/qb_log_callsites_dump.3 \
	docs/man3/qb_log_ctl2.3 \
	docs/man3/qb_log_ctl.3 \
	docs/man3/qb_log_custom_close.3 \
	docs/man3/qb_log_custom_open.3 \
	docs/man3/qb_log_facility2int.3 \
	docs/man3/qb_log_facility2str.3 \
	docs/man3/qb_log_file_close.3 \
	docs/man3/qb_log_file_open.3 \
	docs/man3/qb_log_file_reopen.3 \
	docs/man3/qb_log_filter_ctl2.3 \
	docs/man3/qb_log_filter_ctl.3 \
	docs/man3/qb_log_filter_fn_set.3 \
	docs/man3/qb_log_fini.3 \
	docs/man3/qb_log_format_set.3 \
	docs/man3/qblog.h.3 \
	docs/man3/qb_log_init.3 \
	docs/man3/qb_log_real_.3 \
	docs/man3/qb_log_tags_stringify_fn_set.3 \
	docs/man3/qb_log_target_format.3 \
	docs/man3/qb_log_thread_priority_set.3 \
	docs/man3/qb_log_thread_start.3 \
	docs/man3/qb_loop_create.3 \
	docs/man3/qb_loop_destroy.3 \
	docs/man3/qbloop.h.3 \
	docs/man3/qb_loop_job_add.3 \
	docs/man3/qb_loop_job_del.3 \
	docs/man3/qb_loop_poll_add.3 \
	docs/man3/qb_loop_poll_del.3 \
	docs/man3/qb_loop_poll_low_fds_event_set.3 \
	docs/man3/qb_loop_poll_mod.3 \
	docs/man3/qb_loop_run.3 \
	docs/man3/qb_loop_signal_add.3 \
	docs/man3/qb_loop_signal_del.3 \
	docs/man3/qb_loop_stop.3 \
	docs/man3/qb_loop_timer_add.3 \
	docs/man3/qb_loop_timer_del.3 \
	docs/man3/qb_loop_timer_expire_time_get.3 \
	docs/man3/qb_loop_timer_expire_time_remaining.3 \
	docs/man3/qb_loop_timer_is_running.3 \
	docs/man3/qb_map_count_get.3 \
	docs/man3/qb_map_destroy.3 \
	docs/man3/qb_map_foreach.3 \
	docs/man3/qb_map_get.3 \
	docs/man3/qbmap.h.3 \
	docs/man3/qb_map_iter_create.3 \
	docs/man3/qb_map_iter_free.3 \
	docs/man3/qb_map_iter_next.3 \
	docs/man3/qb_map_notify_add.3 \
	docs/man3/qb_map_notify_del_2.3 \
	docs/man3/qb_map_notify_del.3 \
	docs/man3/qb_map_pref_iter_create.3 \
	docs/man3/qb_map_put.3 \
	docs/man3/qb_map_rm.3 \
	docs/man3/qb_rb_chunk_peek.3 \
	docs/man3/qb_rb_chunk_read.3 \
	docs/man3/qb_rb_chunk_reclaim.3 \
	docs/man3/qb_rb_chunks_used.3 \
	docs/man3/qb_rb_chunk_write.3 \
	docs/man3/qb_rb_close.3 \
	docs/man3/qbrb.h.3 \
	docs/man3/qb_rb_open.3 \
	docs/man3/qb_rb_space_free.3 \
	docs/man3/qb_skiplist_create.3 \
	docs/man3/qb_strerror_r.3 \
	docs/man3/qb_thread_lock.3 \
	docs/man3/qb_thread_lock_create.3 \
	docs/man3/qb_thread_lock_destroy.3 \
	docs/man3/qb_thread_trylock.3 \
	docs/man3/qb_thread_unlock.3 \
	docs/man3/qb_timespec_add_ms.3 \
	docs/man3/qb_trie_create.3 \
	docs/man3/qbutil.h.3 \
	docs/man3/qb_util_nano_current_get.3 \
	docs/man3/qb_util_nano_from_epoch_get.3 \
	docs/man3/qb_util_nano_monotonic_hz.3 \
	docs/man3/qb_util_stopwatch_create.3 \
	docs/man3/qb_util_stopwatch_free.3 \
	docs/man3/qb_util_stopwatch_sec_elapsed_get.3 \
	docs/man3/qb_util_stopwatch_split.3 \
	docs/man3/qb_util_stopwatch_split_ctl.3 \
	docs/man3/qb_util_stopwatch_split_last.3 \
	docs/man3/qb_util_stopwatch_start.3 \
	docs/man3/qb_util_stopwatch_stop.3 \
	docs/man3/qb_util_stopwatch_time_split_get.3 \
	docs/man3/qb_util_stopwatch_us_elapsed_get.3 \
	docs/man3/qb_util_timespec_from_epoch_get.3 \
	docs/man3/qb_ver_str_get.3

# All man pages are built from a single stamp that runs doxygen then doxygen2man.
# The cargo build must complete first so that include/qb/qb.h is up to date.
$(man3_MANS): docs/man3/.stamp

docs/man3/.stamp: $(top_srcdir)/docs/man.dox $(RUST_LIB)
	$(MKDIR_P) $(top_srcdir)/docs/man3
	cd $(top_srcdir)/docs && $(DOXYGEN) man.dox
	cd $(top_srcdir)/docs && \
	    $(DOXYGEN2MAN) $(doxygen2man_flags) qb_8h.xml ; \
	    $(DOXYGEN2MAN) $(doxygen2man_flags) qbhdb_8h.xml ; \
	    $(DOXYGEN2MAN) $(doxygen2man_flags) qblist_8h.xml ; \
	    $(DOXYGEN2MAN) $(doxygen2man_flags) qblog_8h.xml ; \
	    $(DOXYGEN2MAN) $(doxygen2man_flags) qbarray_8h.xml ; \
	    $(DOXYGEN2MAN) $(doxygen2man_flags) qbatomic_8h.xml ; \
	    $(DOXYGEN2MAN) $(doxygen2man_flags) qbdefs_8h.xml ; \
	    $(DOXYGEN2MAN) $(doxygen2man_flags) qbipcc_8h.xml ; \
	    $(DOXYGEN2MAN) $(doxygen2man_flags) qbipc__common_8h.xml ; \
	    $(DOXYGEN2MAN) $(doxygen2man_flags) qbipcs_8h.xml ; \
	    $(DOXYGEN2MAN) $(doxygen2man_flags) qbloop_8h.xml ; \
	    $(DOXYGEN2MAN) $(doxygen2man_flags) qbmap_8h.xml ; \
	    $(DOXYGEN2MAN) $(doxygen2man_flags) qbrb_8h.xml ; \
	    $(DOXYGEN2MAN) $(doxygen2man_flags) qbutil_8h.xml ; \
	    $(DOXYGEN2MAN) $(doxygen2man_flags) group__qb__list.xml ; \
	    $(DOXYGEN2MAN) $(doxygen2man_flags) group__qb__hdb.xml ; \
	    $(DOXYGEN2MAN) $(doxygen2man_flags) group__qblog.xml || true
	touch $@

man: docs/man3/.stamp

endif # BUILD_MAN
endif # HAVE_DOXYGEN

# ── Cleaning ──────────────────────────────────────────────────────────────────

clean-local:
	-$(CARGO) clean
	-rm -rf $(CARGO_TARGET_DIR)
	-rm -rf $(top_srcdir)/docs/xml $(top_srcdir)/docs/man3

# ── Distribution tarball extras ───────────────────────────────────────────────

EXTRA_DIST = \
    src          \
    include      \
    test         \
    docs/man.dox \
    Cargo.toml   \
    Cargo.lock   \
    build.rs     \
    cbindgen.toml \
    libqb.pc
