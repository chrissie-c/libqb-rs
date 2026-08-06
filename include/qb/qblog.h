#ifndef QB_LOG_H_DEFINED
#define QB_LOG_H_DEFINED

/**
 * @defgroup qblog Logging API
 * @{
 *
 * The qblog module provides a flexible, multi-target logging subsystem
 * modelled on the libqb C library.  Log messages are routed through
 * per-callsite descriptors (struct qb_log_callsite) so that filter
 * decisions are made once at registration time rather than on every
 * message.  Targets include syslog, files, custom callbacks, and the
 * systemd journal, and are identified by integer handles.
 *
 * Typical usage:
 * @code
 *   qb_log(LOG_INFO, "hello from %s", __func__);
 *   qb_logt(LOG_WARNING, MY_TAG, "tagged message");
 * @endcode
 */

#include <stdarg.h>
#include <stdio.h>
#include "qb.h"
#include "qbdefs.h"

/* ── Buffer size constants ─────────────────────────────────────── */
#define QB_LOG_MAX_LEN            512
#define QB_LOG_STRERROR_MAX_LEN   128

/* ── QB_LOG_CONF_* ─────────────────────────────────────────────── */
#define QB_LOG_CONF_ENABLED       0
#define QB_LOG_CONF_FACILITY      1
#define QB_LOG_CONF_DEBUG         2
#define QB_LOG_CONF_SIZE          3
#define QB_LOG_CONF_THREADED      4
#define QB_LOG_CONF_PRIORITY_BUMP 5
#define QB_LOG_CONF_STATE_GET     6
#define QB_LOG_CONF_FILE_SYNC     7
#define QB_LOG_CONF_EXTENDED      8
#define QB_LOG_CONF_IDENT         9
#define QB_LOG_CONF_MAX_LINE_LEN  10
#define QB_LOG_CONF_ELLIPSIS      11
#define QB_LOG_CONF_USE_JOURNAL   12

/* ── QB_LOG_STATE_* ────────────────────────────────────────────── */
#define QB_LOG_STATE_UNUSED   0
#define QB_LOG_STATE_DISABLED 1
#define QB_LOG_STATE_ENABLED  2

/* ── QB_LOG_FILTER_* ───────────────────────────────────────────── */
#define QB_LOG_FILTER_ADD       0
#define QB_LOG_FILTER_REMOVE    1
#define QB_LOG_FILTER_CLEAR_ALL 2
#define QB_LOG_TAG_SET          3
#define QB_LOG_TAG_CLEAR        4
#define QB_LOG_TAG_CLEAR_ALL    5

#define QB_LOG_FILTER_FILE             0
#define QB_LOG_FILTER_FUNCTION         1
#define QB_LOG_FILTER_FORMAT           2
#define QB_LOG_FILTER_FILE_REGEX       3
#define QB_LOG_FILTER_FUNCTION_REGEX   4
#define QB_LOG_FILTER_FORMAT_REGEX     5

/* ── Tag bitmask ───────────────────────────────────────────────── */
#define QB_LOG_TAG_LIBQB_MSG_BIT  31
#define QB_LOG_TAG_LIBQB_MSG      (1u << QB_LOG_TAG_LIBQB_MSG_BIT)

/* ── QB_XS / QB_XC extended-info separator ────────────────────── */
#define QB_XC '\a'
#define QB_XS "\a"

/* ── qb_log_ctl2 argument type ─────────────────────────────────── */
typedef union QbLogCtl2Arg qb_log_ctl2_arg_t;
#ifdef __cplusplus
#define QB_LOG_CTL2_I32(a)  (qb_log_ctl2_arg_t{ .i32 = (a) })
#define QB_LOG_CTL2_S(a)    (qb_log_ctl2_arg_t{ .s   = (a) })
#else
#define QB_LOG_CTL2_I32(a)  ((qb_log_ctl2_arg_t){ .i32 = (a) })
#define QB_LOG_CTL2_S(a)    ((qb_log_ctl2_arg_t){ .s   = (a) })
#endif

/* ── QB_LOG_INIT_DATA ──────────────────────────────────────────── */
#define QB_LOG_INIT_DATA(name) /* no-op */

/* ── Logger callback types ────────────────────────────────────── */

typedef void (*qb_log_logger_fn)(int32_t t,
                                  struct qb_log_callsite *cs,
                                  struct timespec *timestamp,
                                  const char *msg);
typedef void (*qb_log_close_fn)(int32_t t);
typedef void (*qb_log_reload_fn)(int32_t t);

/**
 * @brief Open a custom log target backed by caller-supplied callbacks.
 *
 * Registers a new dynamic log target.  The returned handle falls in the
 * inclusive range QB_LOG_TARGET_DYNAMIC_START to QB_LOG_TARGET_DYNAMIC_END
 * and may be passed to other qb_log_*() functions (e.g. qb_log_ctl()) to
 * configure the target.
 *
 * @param log_fn    Callback invoked for each log message delivered to this
 *                  target; must not be NULL.
 * @param close_fn  Callback invoked when the target is closed; may be NULL.
 * @param reload_fn Callback invoked on log reload (e.g. SIGHUP); may be NULL.
 * @param user_data Opaque pointer passed through to the callbacks unchanged.
 * @return A positive target handle on success (QB_LOG_TARGET_DYNAMIC_START
 *         to QB_LOG_TARGET_DYNAMIC_END), or -errno on failure.
 */
int32_t qb_log_custom_open(qb_log_logger_fn log_fn,
                             qb_log_close_fn close_fn,
                             qb_log_reload_fn reload_fn,
                             void *user_data);

/* ── Low-level dispatch: qb_log_real_() ────────────────────────── */
/*
 * Format and dispatch a log message using a pre-resolved callsite.
 * The variadic args are printf args for cs->format.
 */
/**
 * @brief Format and dispatch a log message using a pre-resolved callsite.
 *
 * This is an internal function; prefer the qb_log() or qb_logt() macros
 * for ordinary use.  The variadic arguments are interpreted as printf-style
 * arguments for the format string stored in @p cs.
 *
 * @param cs Pointer to the callsite descriptor previously obtained from
 *           qb_log_callsite_get() or qb_log_callsite_get2().
 * @param ... printf-style format arguments matching @p cs->format.
 */
void qb_log_real_(struct qb_log_callsite *cs, ...);
/**
 * @brief va_list variant of qb_log_real_().
 *
 * Identical to qb_log_real_() but accepts a @c va_list instead of a
 * variadic argument list, making it suitable for wrapper functions that
 * themselves accept @c ....
 *
 * @param cs Pointer to the callsite descriptor previously obtained from
 *           qb_log_callsite_get() or qb_log_callsite_get2().
 * @param ap Initialized @c va_list of printf-style arguments matching
 *           @p cs->format.
 */
void qb_log_real_va_(struct qb_log_callsite *cs, va_list ap);

/* ── Callsite get/register ─────────────────────────────────────── */
/**
 * @brief Get or create a callsite descriptor for a given source location.
 *
 * Looks up an existing callsite matching all supplied parameters, or
 * allocates and registers a new one.  The returned pointer is stable for
 * the lifetime of the process and may be cached by the caller.  Pass it
 * directly to qb_log_real_() to emit log messages without re-paying the
 * lookup cost.
 *
 * @param function Name of the originating function (typically __func__).
 * @param filename Name of the originating source file (typically __FILE__).
 * @param format   printf-style format string that will be associated with
 *                 this callsite.
 * @param priority Syslog-compatible priority level (e.g. LOG_INFO).
 * @param lineno   Source line number (typically __LINE__).
 * @param tags     Bitmask of application-defined tags; use 0 for none.
 * @return Pointer to the callsite descriptor, or NULL on allocation failure.
 */
struct qb_log_callsite *qb_log_callsite_get(const char *function,
                                             const char *filename,
                                             const char *format,
                                             uint8_t priority,
                                             uint32_t lineno,
                                             uint32_t tags);

/**
 * @brief Get or create a callsite descriptor, including a message ID.
 *
 * Identical to qb_log_callsite_get() but additionally associates a
 * @p message_id with the callsite, which is used when logging to targets
 * that support structured message identifiers (e.g. the systemd journal).
 *
 * @param message_id Structured message identifier (e.g. a systemd catalog
 *                   UUID string), or NULL.
 * @param function   Name of the originating function (typically __func__).
 * @param filename   Name of the originating source file (typically __FILE__).
 * @param format     printf-style format string associated with this callsite.
 * @param priority   Syslog-compatible priority level (e.g. LOG_DEBUG).
 * @param lineno     Source line number (typically __LINE__).
 * @param tags       Bitmask of application-defined tags; use 0 for none.
 * @return Pointer to the callsite descriptor, or NULL on allocation failure.
 */
struct qb_log_callsite *qb_log_callsite_get2(const char *message_id,
                                              const char *function,
                                              const char *filename,
                                              const char *format,
                                              uint8_t priority,
                                              uint32_t lineno,
                                              uint32_t tags);

/* ── qb_log_from_external_source ──────────────────────────────── */
/**
 * @brief Inject a log message originating from an external source.
 *
 * Use this function to import log records produced by third-party libraries
 * or other code that supplies a logging callback.  Because filtering is
 * applied on every call rather than being cached in a callsite, the
 * performance of this function is lower than qb_log() / qb_logt().
 *
 * @param function Name of the originating function in the external source.
 * @param filename Name of the originating file in the external source.
 * @param format   printf-style format string for the message body.
 * @param priority Syslog-compatible priority level (e.g. LOG_WARNING).
 * @param lineno   Line number within the originating file.
 * @param tags     Bitmask of application-defined tags usable with
 *                 qb_log_tags_stringify_fn_set(); use 0 for none.
 * @param ...      printf-style arguments matching @p format.
 */
void qb_log_from_external_source(const char *function,
                                  const char *filename,
                                  const char *format,
                                  uint8_t priority,
                                  uint32_t lineno,
                                  uint32_t tags,
                                  ...)
    __attribute__((format(printf, 3, 7)));

/**
 * @brief va_list variant of qb_log_from_external_source().
 *
 * Accepts a @c va_list instead of variadic arguments so that it can be
 * called from functions that are themselves variadic.  See
 * qb_log_from_external_source() for full parameter documentation.
 *
 * @param function Name of the originating function in the external source.
 * @param filename Name of the originating file in the external source.
 * @param format   printf-style format string for the message body.
 * @param priority Syslog-compatible priority level (e.g. LOG_WARNING).
 * @param lineno   Line number within the originating file.
 * @param tags     Bitmask of application-defined tags; use 0 for none.
 * @param ap       Initialized @c va_list of printf-style arguments matching
 *                 @p format.
 */
void qb_log_from_external_source_va(const char *function,
                                     const char *filename,
                                     const char *format,
                                     uint8_t priority,
                                     uint32_t lineno,
                                     uint32_t tags,
                                     va_list ap)
    __attribute__((format(printf, 3, 0)));

/**
 * @brief Inject a log message from an external source, including a message ID.
 *
 * Combines the functionality of qb_log_from_external_source() with the
 * structured @p message_id parameter of qb_log_callsite_get2(), allowing
 * external log records to carry a catalog identifier (e.g. a systemd journal
 * MESSAGE_ID).
 *
 * @param function   Name of the originating function in the external source.
 * @param filename   Name of the originating file in the external source.
 * @param format     printf-style format string for the message body.
 * @param priority   Syslog-compatible priority level (e.g. LOG_ERR).
 * @param lineno     Line number within the originating file.
 * @param tags       Bitmask of application-defined tags; use 0 for none.
 * @param message_id Structured message identifier (e.g. a systemd catalog
 *                   UUID string), or NULL.
 * @param ...        printf-style arguments matching @p format.
 */
void qb_log_from_external_source_msg_id(const char *function,
                                         const char *filename,
                                         const char *format,
                                         uint8_t priority,
                                         uint32_t lineno,
                                         uint32_t tags,
                                         const char *message_id,
                                         ...)
    __attribute__((format(printf, 3, 8)));

/* ── qb_log / qb_logt / qb_logt2 / qb_log2 macros ─────────────── */
/**
 * @brief Emit a tagged log message with an explicit message ID (macro).
 *
 * Resolves or creates a callsite on the first call for the given source
 * location, then dispatches the formatted message.  This is the lowest-level
 * logging macro; prefer qb_log() or qb_logt() for everyday use.
 *
 * @param message_id Structured message identifier for the systemd catalog,
 *                   or NULL.
 * @param priority   Syslog-compatible priority level (e.g. LOG_DEBUG).
 * @param tags       uint32_t bitmask usable with
 *                   qb_log_tags_stringify_fn_set(); use @c %g in the format
 *                   string to print the tag value.
 * @param fmt        printf-style format string.
 * @param args       printf-style format arguments.
 */
#define qb_logt2(message_id, priority, tags, fmt, args...) do { \
    struct qb_log_callsite *_cs_ = \
        qb_log_callsite_get2((message_id), __func__, __FILE__, (fmt), \
                              (uint8_t)(priority), __LINE__, (tags)); \
    if (_cs_) qb_log_real_(_cs_, ##args); \
} while (0)

/**
 * @brief Emit a tagged log message (macro).
 *
 * Resolves or creates a callsite for the current source location, then
 * dispatches the formatted message to all enabled targets whose filters
 * match.  Use the @p tags argument with qb_log_tags_stringify_fn_set() to
 * attach application-defined metadata; the tag value may be printed with
 * @c %g in the format string.
 *
 * @param priority Syslog-compatible priority level (e.g. LOG_WARNING).
 * @param tags     uint32_t bitmask of application-defined tags; use 0 for
 *                 none.
 * @param fmt      printf-style format string.
 * @param args     printf-style format arguments.
 */
#define qb_logt(priority, tags, fmt, args...) \
    qb_logt2(NULL, (priority), (tags), (fmt), ##args)

/**
 * @brief Emit a log message (macro).
 *
 * The primary logging macro for ordinary use.  Equivalent to qb_logt() with
 * a tag value of 0.
 *
 * @param priority Syslog-compatible priority level (e.g. LOG_INFO).
 * @param fmt      printf-style format string.
 * @param args     printf-style format arguments.
 */
#define qb_log(priority, fmt, args...) \
    qb_logt((priority), 0, (fmt), ##args)

/**
 * @brief Emit a log message with a structured message ID (macro).
 *
 * Equivalent to qb_logt2() with a tag value of 0.  Useful when logging to
 * the systemd journal and a catalog MESSAGE_ID is desired.
 *
 * @param message_id Structured message identifier for the systemd catalog,
 *                   or NULL.
 * @param priority   Syslog-compatible priority level (e.g. LOG_ERR).
 * @param fmt        printf-style format string.
 * @param args       printf-style format arguments.
 */
#define qb_log2(message_id, priority, fmt, args...) \
    qb_logt2((message_id), (priority), 0, (fmt), ##args)

/* ── qb_enter / qb_leave ──────────────────────────────────────── */
/**
 * @brief Trace macro emitted on function entry (no-op in this implementation).
 */
#define qb_enter() do {} while (0)
/**
 * @brief Trace macro emitted on function exit (no-op in this implementation).
 */
#define qb_leave() do {} while (0)

/* ── qb_perror ────────────────────────────────────────────────── */
/**
 * @brief Emit a log message that appends the current errno description (macro).
 *
 * Formats the caller-supplied message followed by the string representation
 * of the current @c errno value and its numeric code, then dispatches via
 * qb_logt().
 *
 * @param priority Syslog-compatible priority level (e.g. LOG_ERR).
 * @param fmt      printf-style format string for the primary message text.
 * @param args     printf-style format arguments.
 */
#define qb_perror(priority, fmt, args...) do { \
    char _perr_buf_[QB_LOG_STRERROR_MAX_LEN]; \
    const char *_perr_str_ = qb_strerror_r(errno, _perr_buf_, sizeof(_perr_buf_)); \
    qb_logt((priority), 0, fmt ": %s (%d)", ##args, _perr_str_, errno); \
} while (0)

/** @} */ /* end of qblog group */

#endif /* QB_LOG_H_DEFINED */
