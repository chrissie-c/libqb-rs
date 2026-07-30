#include "qbipc_common.h"

/* Function pointer typedefs used in qb_ipcs_poll_handlers */
typedef void (*qb_loop_job_dispatch_fn)(void *data);
typedef int32_t (*qb_ipcs_dispatch_fn_t)(int32_t fd, int32_t revents, void *data);
