#ifndef QB_IPC_COMMON_H_DEFINED
#define QB_IPC_COMMON_H_DEFINED

#include <stdint.h>

/* Header prepended to every IPC request message (client to server). */
struct qb_ipc_request_header {
    int32_t id;
    int32_t size;
} __attribute__((aligned(8)));

/* Header prepended to every IPC response message (server to client). */
struct qb_ipc_response_header {
    int32_t id;
    int32_t size;
    int32_t error;
    int32_t _pad;
} __attribute__((aligned(8)));

#include "qb.h"

#endif /* QB_IPC_COMMON_H_DEFINED */
