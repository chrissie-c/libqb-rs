#include <signal.h>
#include "qb.h"

/* Job dispatch function type alias */
typedef void (*qb_loop_job_dispatch_fn)(void *data);

/* Compatibility alias: original libqb uses qb_loop_signal_handle = void *,
 * libqb-rs uses QbLoopSignalHandle = uint64_t. Alias so test code compiles. */
typedef QbLoopSignalHandle qb_loop_signal_handle;
