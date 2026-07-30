/*
 * C shims for libqb log functions that require variadic args or
 * specific function-pointer types that cannot be expressed in stable Rust.
 *
 * This file implements:
 *   qb_log_real_         – variadic dispatch via callsite
 *   qb_log_real_va_      – va_list version of the above
 *   qb_log_from_external_source   – variadic printf-style log entry
 *   qb_log_from_external_source_va – va_list version
 *   qb_log_from_external_source_va2 – va_list + message_id
 *   qb_log_from_external_source_msg_id – variadic + message_id
 *   qb_log_custom_open   – accepts proper C function pointer types
 *   qb_vsnprintf_serialize  – format printf args into buffer (= vsnprintf)
 *   qb_vsnprintf_deserialize – copy serialised text to output buffer
 */

#include <stdarg.h>
#include <stddef.h>
#include <string.h>
#include <stdio.h>
#include <stdint.h>
#include <time.h>

/* ── callsite struct (matches Rust layout) ─────────────────────── */

struct qb_log_callsite {
    const char *function;
    const char *filename;
    const char *format;
    uint8_t     priority;
    uint8_t     _pad[3];
    uint32_t    lineno;
    uint32_t    targets;
    uint32_t    tags;
    const char *message_id;
};

#define QB_LOG_ABSOLUTE_MAX_LEN 4096

/* ── Rust-exported symbols called by these shims ─────────────────── */

/*
 * Dispatch a pre-formatted log message to all enabled targets.
 * Defined in src/log.rs as qb_log_dispatch_raw.
 */
extern void qb_log_dispatch_raw(struct qb_log_callsite *cs,
                                 struct timespec *ts,
                                 const char *msg);

/*
 * Get or create a callsite (cached by function/filename/format/priority/lineno).
 * Defined in src/log.rs as qb_log_callsite_get2.
 */
extern struct qb_log_callsite *qb_log_callsite_get2(const char *message_id,
                                                     const char *function,
                                                     const char *filename,
                                                     const char *format,
                                                     uint8_t priority,
                                                     uint32_t lineno,
                                                     uint32_t tags);

/*
 * Internal custom-open that accepts raw function pointers as void*.
 * Defined in src/log.rs.
 */
extern int32_t qb_log_custom_open_raw(void (*logger)(int32_t,
                                                       struct qb_log_callsite *,
                                                       struct timespec *,
                                                       const char *),
                                        void (*close_fn)(int32_t),
                                        void (*reload_fn)(int32_t),
                                        void *user_data);

/* ── qb_log_real_va_ ────────────────────────────────────────────── */

void qb_log_real_va_(struct qb_log_callsite *cs, va_list ap)
{
    char msg[QB_LOG_ABSOLUTE_MAX_LEN];
    struct timespec ts = {0, 0};

    if (!cs || cs->targets == 0) return;

    vsnprintf(msg, sizeof(msg), cs->format, ap);
    clock_gettime(CLOCK_REALTIME, &ts);
    qb_log_dispatch_raw(cs, &ts, msg);
}

/* ── qb_log_real_ ───────────────────────────────────────────────── */

void qb_log_real_(struct qb_log_callsite *cs, ...)
{
    va_list ap;
    va_start(ap, cs);
    qb_log_real_va_(cs, ap);
    va_end(ap);
}

/* ── qb_log_from_external_source_va2 ───────────────────────────── */

void qb_log_from_external_source_va2(const char *message_id,
                                      const char *function,
                                      const char *filename,
                                      const char *format,
                                      uint8_t priority,
                                      uint32_t lineno,
                                      uint32_t tags,
                                      va_list ap)
{
    struct qb_log_callsite *cs;
    char msg[QB_LOG_ABSOLUTE_MAX_LEN];
    struct timespec ts = {0, 0};

    cs = qb_log_callsite_get2(message_id, function, filename, format,
                               priority, lineno, tags);
    if (!cs || cs->targets == 0) return;

    vsnprintf(msg, sizeof(msg), format, ap);
    clock_gettime(CLOCK_REALTIME, &ts);
    qb_log_dispatch_raw(cs, &ts, msg);

}

/* ── qb_log_from_external_source_va ─────────────────────────────── */

void qb_log_from_external_source_va(const char *function,
                                     const char *filename,
                                     const char *format,
                                     uint8_t priority,
                                     uint32_t lineno,
                                     uint32_t tags,
                                     va_list ap)
{
    qb_log_from_external_source_va2(NULL, function, filename, format,
                                    priority, lineno, tags, ap);
}

/* ── qb_log_from_external_source ────────────────────────────────── */

void qb_log_from_external_source(const char *function,
                                  const char *filename,
                                  const char *format,
                                  uint8_t priority,
                                  uint32_t lineno,
                                  uint32_t tags,
                                  ...)
{
    va_list ap;
    va_start(ap, tags);
    qb_log_from_external_source_va2(NULL, function, filename, format,
                                    priority, lineno, tags, ap);
    va_end(ap);
}

/* ── qb_log_from_external_source_msg_id ─────────────────────────── */

void qb_log_from_external_source_msg_id(const char *function,
                                         const char *filename,
                                         const char *format,
                                         uint8_t priority,
                                         uint32_t lineno,
                                         uint32_t tags,
                                         const char *message_id,
                                         ...)
{
    va_list ap;
    va_start(ap, message_id);
    qb_log_from_external_source_va2(message_id, function, filename, format,
                                    priority, lineno, tags, ap);
    va_end(ap);
}

/* ── qb_log_custom_open ─────────────────────────────────────────── */

int32_t qb_log_custom_open(void (*logger)(int32_t,
                                           struct qb_log_callsite *,
                                           struct timespec *,
                                           const char *),
                             void (*close_fn)(int32_t),
                             void (*reload_fn)(int32_t),
                             void *user_data)
{
    return qb_log_custom_open_raw(logger, close_fn, reload_fn, user_data);
}

/* ── qb_vsnprintf_serialize ─────────────────────────────────────── */

size_t qb_vsnprintf_serialize(char *serialize, size_t max_len,
                               const char *fmt, va_list ap)
{
    int ret;
    if (!serialize || max_len == 0) return 0;
    ret = vsnprintf(serialize, max_len, fmt, ap);
    if (ret < 0) {
        serialize[0] = '\0';
        return 0;
    }
    return (size_t)(ret < (int)max_len ? ret : (int)max_len - 1);
}

/* ── qb_vsnprintf_deserialize ───────────────────────────────────── */

size_t qb_vsnprintf_deserialize(char *string, size_t strlen_,
                                 const char *buf)
{
    size_t len;
    if (!string || strlen_ == 0 || !buf) return 0;
    len = strlen(buf);
    if (len >= strlen_) len = strlen_ - 1;
    memcpy(string, buf, len);
    string[len] = '\0';
    return len;
}
