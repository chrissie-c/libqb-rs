LIBQB_SO  := ../target/debug/libqb.so
LIBQB_DIR := ../target/debug
INCDIR    := ../include

CC      := gcc
CFLAGS  := -g -O0 -Wall -I$(INCDIR) -I. -D_GNU_SOURCE -DHAVE_CONFIG_H \
           $(shell pkg-config --cflags check)
LDFLAGS := -L$(LIBQB_DIR) -Wl,-rpath,$(abspath $(LIBQB_DIR))
LDLIBS  := -lqb $(shell pkg-config --libs check) -lpthread -lm -lrt

TESTS := check_array check_list check_log check_loop check_map check_rb check_tlist check_util

.PHONY: all run clean

all: $(TESTS)

check_array: check_array.c $(LIBQB_SO)
	$(CC) $(CFLAGS) -o $@ $< $(LDFLAGS) $(LDLIBS)

check_list: check_list.c $(LIBQB_SO)
	$(CC) $(CFLAGS) -o $@ $< $(LDFLAGS) $(LDLIBS)

check_log: check_log.c _syslog_override.c $(LIBQB_SO)
	$(CC) $(CFLAGS) -o $@ check_log.c _syslog_override.c $(LDFLAGS) $(LDLIBS)

check_loop: check_loop.c $(LIBQB_SO)
	$(CC) $(CFLAGS) -o $@ $< $(LDFLAGS) $(LDLIBS)

check_map: check_map.c $(LIBQB_SO)
	$(CC) $(CFLAGS) -o $@ $< $(LDFLAGS) $(LDLIBS)

check_rb: check_rb.c $(LIBQB_SO)
	$(CC) $(CFLAGS) -o $@ $< $(LDFLAGS) $(LDLIBS)

check_tlist: check_tlist.c $(LIBQB_SO)
	$(CC) $(CFLAGS) -o $@ $< $(LDFLAGS) $(LDLIBS)

check_util: check_util.c $(LIBQB_SO)
	$(CC) $(CFLAGS) -o $@ $< $(LDFLAGS) $(LDLIBS)

check_ipc: check_ipc.c $(LIBQB_SO)
	$(CC) $(CFLAGS) -Wno-incompatible-function-pointer-types -o $@ $< _failure_injection.c $(LDFLAGS) $(LDLIBS) -DIPC_TEST_NAME_FILE="\"ipc-test-name-sock\"" -DRUST_LIBQB -DSOCKETDIR="\"/var/run\""

run: all
	@for t in $(TESTS); do \
		echo "=== $$t ==="; \
		LD_LIBRARY_PATH=$(LIBQB_DIR) ./$$t || true; \
	done

clean:
	rm -f $(TESTS)
