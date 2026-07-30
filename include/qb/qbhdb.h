#ifndef QB_HDB_H_DEFINED
#define QB_HDB_H_DEFINED

#include <inttypes.h>
#include "qb.h"
#include "qbarray.h"

/*
 * qb_hdb_create initializes a caller-allocated struct qb_hdb.
 * Declare databases at file scope with QB_HDB_DECLARE; the destructor stored
 * in the struct is picked up by qb_hdb_create at initialization time.
 */
#define QB_HDB_DECLARE(name, destructor) \
    struct qb_hdb name = { NULL, (void (*)(void *))(destructor) }

#define QB_HDB_D_FORMAT "%" PRIu64
#define QB_HDB_X_FORMAT "%" PRIx64

#endif /* QB_HDB_H_DEFINED */
