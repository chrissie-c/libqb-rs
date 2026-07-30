#ifndef QB_LOG_H_DEFINED
#define QB_LOG_H_DEFINED

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
#define QB_LOG_TAG_LIBQB_MSG_BIT  0
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

int32_t qb_log_custom_open(qb_log_logger_fn log_fn,
                             qb_log_close_fn close_fn,
                             qb_log_reload_fn reload_fn,
                             void *user_data);

/* ── Low-level dispatch: qb_log_real_() ────────────────────────── */
/*
 * Format and dispatch a log message using a pre-resolved callsite.
 * The variadic args are printf args for cs->format.
 */
void qb_log_real_(struct qb_log_callsite *cs, ...);
void qb_log_real_va_(struct qb_log_callsite *cs, va_list ap);

/* ── Callsite get/register ─────────────────────────────────────── */
struct qb_log_callsite *qb_log_callsite_get(const char *function,
                                             const char *filename,
                                             const char *format,
                                             uint8_t priority,
                                             uint32_t lineno,
                                             uint32_t tags);

struct qb_log_callsite *qb_log_callsite_get2(const char *message_id,
                                              const char *function,
                                              const char *filename,
                                              const char *format,
                                              uint8_t priority,
                                              uint32_t lineno,
                                              uint32_t tags);

/* ── qb_log_from_external_source ──────────────────────────────── */
void qb_log_from_external_source(const char *function,
                                  const char *filename,
                                  const char *format,
                                  uint8_t priority,
                                  uint32_t lineno,
                                  uint32_t tags,
                                  ...)
    __attribute__((format(printf, 3, 7)));

void qb_log_from_external_source_va(const char *function,
                                     const char *filename,
                                     const char *format,
                                     uint8_t priority,
                                     uint32_t lineno,
                                     uint32_t tags,
                                     va_list ap)
    __attribute__((format(printf, 3, 0)));

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
#define qb_logt2(message_id, priority, tags, fmt, args...) do { \
    struct qb_log_callsite *_cs_ = \
        qb_log_callsite_get2((message_id), __func__, __FILE__, (fmt), \
                              (uint8_t)(priority), __LINE__, (tags)); \
    if (_cs_) qb_log_real_(_cs_, ##args); \
} while (0)

#define qb_logt(priority, tags, fmt, args...) \
    qb_logt2(NULL, (priority), (tags), (fmt), ##args)

#define qb_log(priority, fmt, args...) \
    qb_logt((priority), 0, (fmt), ##args)

#define qb_log2(message_id, priority, fmt, args...) \
    qb_logt2((message_id), (priority), 0, (fmt), ##args)

/* ── qb_enter / qb_leave ──────────────────────────────────────── */
#define qb_enter() do {} while (0)
#define qb_leave() do {} while (0)

/* ── qb_perror ────────────────────────────────────────────────── */
#define qb_perror(priority, fmt, args...) do { \
    char _perr_buf_[QB_LOG_STRERROR_MAX_LEN]; \
    const char *_perr_str_ = qb_strerror_r(errno, _perr_buf_, sizeof(_perr_buf_)); \
    qb_logt((priority), 0, fmt ": %s (%d)", ##args, _perr_str_, errno); \
} while (0)

#endif /* QB_LOG_H_DEFINED */
