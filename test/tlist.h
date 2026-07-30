/*
 * Internal timer list used by libqb tests.
 * Min-heap of timers, ordered by absolute expiry time (CLOCK_MONOTONIC ns).
 * Supports O(log n) add, delete, and expire.
 */
#ifndef QB_TLIST_H_DEFINED
#define QB_TLIST_H_DEFINED

#include <stdint.h>
#include <stdlib.h>
#include <stdbool.h>
#include <time.h>

/* Internal per-timer node */
struct timer_entry {
    uint64_t expire_time_ns; /* absolute CLOCK_MONOTONIC expiry */
    void (*fn)(void *data);
    void *data;
    int heap_idx;            /* position in heap array, updated on moves */
};

typedef struct timer_entry *timer_handle;

struct timerlist {
    struct timer_entry **heap; /* min-heap array of pointers */
    int size;
    int capacity;
};

static inline uint64_t tlist_now_ns_(void)
{
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (uint64_t)ts.tv_sec * UINT64_C(1000000000) + (uint64_t)ts.tv_nsec;
}

static inline void timerlist_init(struct timerlist *tlist)
{
    tlist->heap = NULL;
    tlist->size = 0;
    tlist->capacity = 0;
}

static inline void timerlist_destroy(struct timerlist *tlist)
{
    int i;
    for (i = 0; i < tlist->size; i++) {
        free(tlist->heap[i]);
    }
    free(tlist->heap);
    timerlist_init(tlist);
}

static inline void tlist_heap_swap_(struct timerlist *tlist, int a, int b)
{
    struct timer_entry *tmp = tlist->heap[a];
    tlist->heap[a] = tlist->heap[b];
    tlist->heap[b] = tmp;
    tlist->heap[a]->heap_idx = a;
    tlist->heap[b]->heap_idx = b;
}

static inline void tlist_sift_up_(struct timerlist *tlist, int i)
{
    while (i > 0) {
        int parent = (i - 1) / 2;
        if (tlist->heap[parent]->expire_time_ns > tlist->heap[i]->expire_time_ns) {
            tlist_heap_swap_(tlist, parent, i);
            i = parent;
        } else {
            break;
        }
    }
}

static inline void tlist_sift_down_(struct timerlist *tlist, int i)
{
    for (;;) {
        int smallest = i;
        int left  = 2 * i + 1;
        int right = 2 * i + 2;
        if (left  < tlist->size &&
            tlist->heap[left]->expire_time_ns  < tlist->heap[smallest]->expire_time_ns)
            smallest = left;
        if (right < tlist->size &&
            tlist->heap[right]->expire_time_ns < tlist->heap[smallest]->expire_time_ns)
            smallest = right;
        if (smallest != i) {
            tlist_heap_swap_(tlist, smallest, i);
            i = smallest;
        } else {
            break;
        }
    }
}

static inline int timerlist_add_duration(struct timerlist *tlist,
                                         void (*fn)(void *data),
                                         void *data,
                                         uint64_t duration_ns,
                                         timer_handle *handle)
{
    struct timer_entry *entry;

    if (tlist->size == tlist->capacity) {
        int new_cap = tlist->capacity == 0 ? 16 : tlist->capacity * 2;
        struct timer_entry **new_heap = (struct timer_entry **)
            realloc(tlist->heap, (size_t)new_cap * sizeof(*new_heap));
        if (!new_heap) return -1;
        tlist->heap = new_heap;
        tlist->capacity = new_cap;
    }

    entry = (struct timer_entry *)malloc(sizeof(*entry));
    if (!entry) return -1;

    entry->expire_time_ns = tlist_now_ns_() + duration_ns;
    entry->fn             = fn;
    entry->data           = data;
    entry->heap_idx       = tlist->size;

    tlist->heap[tlist->size] = entry;
    tlist->size++;

    tlist_sift_up_(tlist, entry->heap_idx);

    *handle = entry;
    return 0;
}

static inline void timerlist_del(struct timerlist *tlist, timer_handle handle)
{
    struct timer_entry *entry = handle;
    int idx = entry->heap_idx;
    struct timer_entry *moved;

    tlist->size--;
    if (idx == tlist->size) {
        /* Removing the last element */
        tlist->heap[tlist->size] = NULL;
    } else {
        moved = tlist->heap[tlist->size];
        tlist->heap[tlist->size] = NULL;
        tlist->heap[idx] = moved;
        moved->heap_idx = idx;
        tlist_sift_up_(tlist, idx);
        tlist_sift_down_(tlist, moved->heap_idx);
    }
    free(entry);
}

static inline uint64_t timerlist_msec_duration_to_expire(struct timerlist *tlist)
{
    uint64_t now, diff;

    if (tlist->size == 0) return (uint64_t)-1;

    now = tlist_now_ns_();
    if (tlist->heap[0]->expire_time_ns <= now) return 0;

    diff = tlist->heap[0]->expire_time_ns - now;
    return (diff + UINT64_C(999999)) / UINT64_C(1000000); /* ceil ns→ms */
}

static inline void timerlist_expire(struct timerlist *tlist)
{
    uint64_t now = tlist_now_ns_();

    while (tlist->size > 0 && tlist->heap[0]->expire_time_ns <= now) {
        struct timer_entry *entry = tlist->heap[0];
        void (*fn)(void *) = entry->fn;
        void *data         = entry->data;
        timerlist_del(tlist, entry);
        fn(data);
        /* Refresh now in case the callback took time */
        now = tlist_now_ns_();
    }
}

static inline bool timerlist_debug_is_valid_heap(struct timerlist *tlist)
{
    int i;
    for (i = 0; i < tlist->size; i++) {
        if (tlist->heap[i]->heap_idx != i) return false;
    }
    for (i = 1; i < tlist->size; i++) {
        int parent = (i - 1) / 2;
        if (tlist->heap[parent]->expire_time_ns > tlist->heap[i]->expire_time_ns)
            return false;
    }
    return true;
}

#endif /* QB_TLIST_H_DEFINED */
