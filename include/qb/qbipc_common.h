#ifndef QB_IPC_COMMON_H_DEFINED
#define QB_IPC_COMMON_H_DEFINED

#include <stdint.h>

/*
 * Header prepended to every IPC request message (client to server).
 *
 * Matches C libqb's per-field __attribute__((aligned(8))) layout:
 *   int32_t id   __attribute__((aligned(8)));  // offset  0, pad  4
 *   int32_t size __attribute__((aligned(8)));  // offset  8, pad  4
 * sizeof = 16
 */
struct qb_ipc_request_header {
    int32_t id   __attribute__((aligned(8)));
    int32_t size __attribute__((aligned(8)));
} __attribute__((aligned(8)));

/*
 * Header prepended to every IPC response message (server to client).
 *
 * Matches C libqb's per-field __attribute__((aligned(8))) layout:
 *   int32_t id    __attribute__((aligned(8)));  // offset  0, pad  4
 *   int32_t size  __attribute__((aligned(8)));  // offset  8, pad  4
 *   int32_t error __attribute__((aligned(8)));  // offset 16, pad  4
 * sizeof = 24
 */
struct qb_ipc_response_header {
    int32_t id    __attribute__((aligned(8)));
    int32_t size  __attribute__((aligned(8)));
    int32_t error __attribute__((aligned(8)));
} __attribute__((aligned(8)));

#include "qb.h"

#endif /* QB_IPC_COMMON_H_DEFINED */
