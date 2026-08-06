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

/**
 * @file qbhdb.h
 * @brief Handle database for reference-counted objects.
 *
 * The handle database provides a way to allocate, look up, and reference-count
 * opaque objects via stable 64-bit handles.
 */

/**
 * @defgroup qb_hdb Handle Database
 *
 * Reference-counted object store accessed through opaque 64-bit handles.
 * @{
 */

#ifndef QB_FALSE
#define QB_FALSE 0
#endif
#ifndef QB_TRUE
#define QB_TRUE (!QB_FALSE)
#endif

/**
 * Generic handle type is 64 bits.
 */
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

/**
 * @brief Declare a file-scoped handle database with a static initializer.
 *
 * Convenience macro that defines a file-scoped @c struct @c qb_hdb variable
 * named @p database_name and initializes it with @p destructor_function as
 * the per-object destructor.
 *
 * @code
 * QB_HDB_DECLARE(my_handle_database, NULL);
 * @endcode
 *
 * @param database_name       Name of the @c struct @c qb_hdb variable to declare.
 * @param destructor_function Pointer to the destructor called when an object's
 *                            refcount reaches zero, or @c NULL for none.
 */
#define QB_HDB_DECLARE(database_name, destructor_function)		\
static struct qb_hdb (database_name) = {				\
	.handle_count	= 0,						\
	.handles	= NULL,						\
	.iterator	= 0,						\
	.destructor	= destructor_function,				\
	.first_run	= QB_TRUE					\
};

/**
 * @brief Create (initialize) a handle database.
 *
 * @param hdb Pointer to the database to initialize.
 */
void     qb_hdb_create(struct qb_hdb *hdb);

/**
 * @brief Destroy a handle database and free all associated resources.
 *
 * @param hdb Pointer to the database to destroy.
 */
void     qb_hdb_destroy(struct qb_hdb *hdb);

/**
 * @brief Allocate a new handle and its backing object.
 *
 * Allocates @p instance_size bytes for the new object, inserts it into @p hdb,
 * and writes the resulting handle value to @p handle_id_out.
 *
 * @param hdb            Pointer to the database instance.
 * @param instance_size  Size in bytes of the object to allocate.
 * @param handle_id_out  Output parameter that receives the new handle.
 * @return 0 on success, or a negative errno value on failure.
 */
int32_t  qb_hdb_handle_create(struct qb_hdb *hdb, int32_t instance_size,
                               qb_handle_t *handle_id_out);

/**
 * @brief Look up a handle and increment its reference count.
 *
 * On success, writes a pointer to the object into @p instance and increments
 * the object's reference count.  The caller must call qb_hdb_handle_put()
 * when done.
 *
 * @param hdb        Pointer to the database instance.
 * @param handle_in  Handle to look up.
 * @param instance   Output parameter that receives a pointer to the object.
 * @return 0 on success, or a negative errno value on failure.
 */
int32_t  qb_hdb_handle_get(struct qb_hdb *hdb, qb_handle_t handle_in,
                            void **instance);

/**
 * @brief Look up a handle and increment its reference count, even if destroy was requested.
 *
 * Identical to qb_hdb_handle_get() except that it also returns objects that
 * have been marked for destruction but not yet freed.
 *
 * @param hdb        Pointer to the database instance.
 * @param handle_in  Handle to look up.
 * @param instance   Output parameter that receives a pointer to the object.
 * @return 0 on success, or a negative errno value on failure.
 */
int32_t  qb_hdb_handle_get_always(struct qb_hdb *hdb, qb_handle_t handle_in,
                                   void **instance);

/**
 * @brief Decrement the reference count of a handle.
 *
 * Decrements the reference count that was incremented by qb_hdb_handle_get()
 * or qb_hdb_handle_get_always().  If the count reaches zero and the handle
 * has been marked for destruction, the backing object is freed.
 *
 * @param hdb        Pointer to the database instance.
 * @param handle_in  Handle whose reference count should be decremented.
 * @return 0 on success, or a negative errno value on failure.
 */
int32_t  qb_hdb_handle_put(struct qb_hdb *hdb, qb_handle_t handle_in);

/**
 * @brief Request the destruction of a handle.
 *
 * Marks the handle for destruction.  The backing object is freed when its
 * reference count reaches zero.
 *
 * @param hdb        Pointer to the database instance.
 * @param handle_in  Handle to destroy.
 * @return 0 on success, or a negative errno value on failure.
 */
int32_t  qb_hdb_handle_destroy(struct qb_hdb *hdb, qb_handle_t handle_in);

/**
 * @brief Return the current reference count of a handle.
 *
 * @param hdb        Pointer to the database instance (const).
 * @param handle_in  Handle to query.
 * @return The current reference count (>= 0) on success, or a negative errno value on failure.
 */
int32_t  qb_hdb_handle_refcount_get(const struct qb_hdb *hdb, qb_handle_t handle_in);

/**
 * @brief Reset the database iterator to the beginning.
 *
 * @param hdb Pointer to the database instance.
 */
void     qb_hdb_iterator_reset(struct qb_hdb *hdb);

/**
 * @brief Advance the iterator and retrieve the next object.
 *
 * Writes the next object's pointer into @p instance and its handle into
 * @p handle, then increments the object's reference count.  The caller must
 * call qb_hdb_handle_put() when done with the object.
 *
 * @param hdb       Pointer to the database instance.
 * @param instance  Output parameter that receives a pointer to the object.
 * @param handle    Output parameter that receives the object's handle.
 * @return 0 on success, or a negative errno value when no more objects remain.
 */
int32_t  qb_hdb_iterator_next(struct qb_hdb *hdb, void **instance,
                               qb_handle_t *handle);

/**
 * @brief Extract the 32-bit base index from a 64-bit handle.
 *
 * @param handle The 64-bit handle to convert.
 * @return The lower 32-bit index component of the handle.
 */
uint32_t qb_hdb_base_convert(qb_handle_t handle);

/**
 * @brief Convert a 32-bit index to a 64-bit handle without a validity check.
 *
 * @param handle The 32-bit index to promote.
 * @return A 64-bit handle value corresponding to @p handle.
 */
uint64_t qb_hdb_nocheck_convert(uint32_t handle);

/** @} */

#ifdef __cplusplus
}
#endif

#endif /* QB_HDB_H_DEFINED */
