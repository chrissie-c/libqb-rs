AC_PREREQ([2.69])
AC_INIT([libqb-rs], [0.1.0], [])
AC_CONFIG_SRCDIR([src/lib.rs])
AC_CONFIG_AUX_DIR([build-aux])
AC_CONFIG_MACRO_DIR([m4])
AM_INIT_AUTOMAKE([foreign -Wall subdir-objects])
AC_CONFIG_HEADERS([config.h])

# ── C compiler ────────────────────────────────────────────────────────────────
AC_PROG_CC
AC_USE_SYSTEM_EXTENSIONS   # defines _GNU_SOURCE etc. in config.h

# ── Standard headers (needed by os_base.h guards) ────────────────────────────
AC_CHECK_HEADERS([errno.h fcntl.h inttypes.h stddef.h stdint.h stdlib.h \
                  string.h strings.h syslog.h sys/socket.h sys/stat.h    \
                  sys/time.h sys/types.h sys/uio.h time.h unistd.h])

# ── POSIX / Linux functions ───────────────────────────────────────────────────
AC_CHECK_FUNCS([clock_gettime epoll_create1 sem_timedwait semtimedop  \
                strchrnul strlcat strlcpy                             \
                pthread_condattr_setpshared pthread_mutexattr_setpshared \
                pthread_setschedparam pthread_spin_lock])

# ── pkg-config + libcheck ─────────────────────────────────────────────────────
PKG_PROG_PKG_CONFIG
PKG_CHECK_MODULES([CHECK], [check >= 0.9.10],
  [],
  [AC_MSG_ERROR([libcheck (>= 0.9.10) not found. Install it with: dnf install check-devel])])

# ── Rust / cargo ──────────────────────────────────────────────────────────────
AC_PATH_PROG([CARGO], [cargo])
AS_IF([test -z "$CARGO"],
  [AC_MSG_ERROR([cargo not found. Install Rust from https://rustup.rs])])

# ── Documentation / man pages ─────────────────────────────────────────────────
AC_PATH_PROG([DOXYGEN], [doxygen])
AM_CONDITIONAL([HAVE_DOXYGEN], [test -n "$DOXYGEN"])

AC_PATH_PROG([DOXYGEN2MAN], [doxygen2man])
AM_CONDITIONAL([BUILD_MAN], [test -n "$DOXYGEN2MAN"])

# ── Output ────────────────────────────────────────────────────────────────────
AC_CONFIG_FILES([Makefile])
AC_OUTPUT

AC_MSG_NOTICE([

  libqb-rs $VERSION configured.

    cargo:        $CARGO
    CC:           $CC
    CFLAGS:       $CFLAGS
    CHECK_CFLAGS: $CHECK_CFLAGS
    doxygen:      ${DOXYGEN:-not found}
    doxygen2man:  ${DOXYGEN2MAN:-not found}

  Build:    make
  Test:     make test
  Man pages: make man  (requires doxygen + doxygen2man)
  Clean:    make clean
])
