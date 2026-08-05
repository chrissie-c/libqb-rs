#ifndef QB_HDB_H_DEFINED
#define QB_HDB_H_DEFINED

#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif

#ifdef __cplusplus
extern "C" {
#endif

#include <stdlib.h>
#include <stdint.h>
#include <inttypes.h>
#include <qb/qbarray.h>

#ifndef QB_FALSE
#define QB_FALSE 0
#endif
#ifndef QB_TRUE
#define QB_TRUE (!QB_FALSE)
#endif

typedef uint64_t qb_handle_t;

#define QB_HDB_D_FORMAT "%" PRIu64
#define QB_HDB_X_FORMAT "%" PRIx64

struct qb_hdb_handle {
	int32_t state;
	void *instance;
	int32_t check;
	int32_t ref_count;
};

/*
 * Layout (x86-64, verified with offsetof against installed libqb headers):
 *   uint32_t handle_count;      offset  0
 *   <padding>                   offset  4
 *   qb_array_t *handles;        offset  8
 *   uint32_t iterator;          offset 16
 *   <padding>                   offset 20
 *   void (*destructor)(void *); offset 24
 *   uint32_t first_run;         offset 32
 *   <padding>                   offset 36
 *   sizeof = 40
 *
 * Our Rust implementation stores a heap-allocated QbHdbInner* in the
 * 'handles' slot at offset 8.  All other field semantics are identical to
 * the C libqb implementation.
 */
struct qb_hdb {
	uint32_t handle_count;
	qb_array_t *handles;
	uint32_t iterator;
	void (*destructor)(void *);
	uint32_t first_run;
};

#define QB_HDB_DECLARE(database_name, destructor_function)		\
static struct qb_hdb (database_name) = {				\
	.handle_count	= 0,						\
	.handles	= NULL,						\
	.iterator	= 0,						\
	.destructor	= destructor_function,				\
	.first_run	= QB_TRUE					\
};

void     qb_hdb_create(struct qb_hdb *hdb);
void     qb_hdb_destroy(struct qb_hdb *hdb);

int32_t  qb_hdb_handle_create(struct qb_hdb *hdb, int32_t instance_size,
                               qb_handle_t *handle_id_out);
int32_t  qb_hdb_handle_get(struct qb_hdb *hdb, qb_handle_t handle_in,
                            void **instance);
int32_t  qb_hdb_handle_get_always(struct qb_hdb *hdb, qb_handle_t handle_in,
                                   void **instance);
int32_t  qb_hdb_handle_put(struct qb_hdb *hdb, qb_handle_t handle_in);
int32_t  qb_hdb_handle_destroy(struct qb_hdb *hdb, qb_handle_t handle_in);
int32_t  qb_hdb_handle_refcount_get(const struct qb_hdb *hdb, qb_handle_t handle_in);

void     qb_hdb_iterator_reset(struct qb_hdb *hdb);
int32_t  qb_hdb_iterator_next(struct qb_hdb *hdb, void **instance,
                               qb_handle_t *handle);

uint32_t qb_hdb_base_convert(qb_handle_t handle);
uint64_t qb_hdb_nocheck_convert(uint32_t handle);

#ifdef __cplusplus
}
#endif

#endif /* QB_HDB_H_DEFINED */
