#ifndef QB_DEFS_H_DEFINED
#define QB_DEFS_H_DEFINED

#include <syslog.h>
#include <unistd.h>
#include "qb.h"

/* Boolean aliases */
#define QB_TRUE  1
#define QB_FALSE 0

/* LOG_TRACE: severity below LOG_DEBUG (8) */
#ifndef LOG_TRACE
#define LOG_TRACE 8
#endif

/* Time constants */
#define QB_TIME_NS_IN_MSEC 1000000ULL
#define QB_TIME_NS_IN_SEC  1000000000ULL
#define QB_TIME_NS_IN_USEC 1000ULL
#define QB_TIME_MS_IN_SEC  1000ULL
#define QB_TIME_US_IN_SEC  1000000ULL
#define QB_TIME_US_IN_MSEC 1000ULL

/* Min/max helpers */
#define QB_MAX(a, b) (((a) > (b)) ? (a) : (b))
#define QB_MIN(a, b) (((a) < (b)) ? (a) : (b))

/* Bit manipulation macros */
#define qb_bit_value(bit)        (1U << (bit))
#define qb_bit_set(barray, bit)  ((barray) |= qb_bit_value(bit))
#define qb_bit_clear(barray, bit) ((barray) &= ~(qb_bit_value(bit)))
#define qb_bit_is_set(barray, bit)   ((barray) & qb_bit_value(bit))
#define qb_bit_is_clear(barray, bit) (!((barray) & qb_bit_value(bit)))

/* IPC type aliases matching C libqb's qb_ipc_type_t values */
#define QB_IPC_NATIVE 0
#define QB_IPC_SHM    1
#define QB_IPC_SOCKET 2

/* Loop priority aliases (from qb_loop_priority enum: Low=0, Med=1, High=2) */
#define QB_LOOP_LOW  0
#define QB_LOOP_MED  1
#define QB_LOOP_HIGH 2

/* IPCS rate limit aliases (from qb_ipcs_rate_limit: Fast=0, Normal=1, Slow=2, Off=3, Off2=4) */
#define QB_IPCS_RATE_FAST   0
#define QB_IPCS_RATE_NORMAL 1
#define QB_IPCS_RATE_SLOW   2
#define QB_IPCS_RATE_OFF    3
#define QB_IPCS_RATE_OFF_   4
#define QB_IPCS_RATE_OFF_2  4

#endif /* QB_DEFS_H_DEFINED */
