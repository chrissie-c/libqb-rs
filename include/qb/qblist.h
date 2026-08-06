#ifndef QB_LIST_H_DEFINED
#define QB_LIST_H_DEFINED

#include <stddef.h>
#include <stdint.h>
#include "qb.h"

/**
 * @file qblist.h
 * @brief Kernel-style intrusive doubly-linked list.
 */

/**
 * @defgroup qb_list List API
 * @{
 *
 * Kernel-style intrusive doubly-linked list.  Each linkable struct embeds a
 * @c qb_list_head node; the qb_list_entry() macro recovers the containing
 * struct from a node pointer, and the qb_list_for_each*() family of macros
 * provides iteration.
 */

/* Declare a statically-initialized list head */
/**
 * @brief Declare and statically initialize a list head.
 *
 * @param name The name of the @c qb_list_head variable to declare.
 */
#define QB_LIST_DECLARE(name) \
    struct qb_list_head name = { &(name), &(name) }

/**
 * @brief Initialize a list head.
 *
 * Points the @c next and @c prev pointers back to @p head itself,
 * making the list empty.
 *
 * @param head Pointer to the list head to initialize.
 */
static inline void qb_list_init(struct qb_list_head *head)
{
	head->next = head;
	head->prev = head;
}

/**
 * @brief Insert an element at the front of the list.
 *
 * The new element is placed immediately after @p head (i.e. at the
 * beginning of the list as seen by forward iteration).
 *
 * @param element Pointer to the new list node to insert.
 * @param head    Pointer to the list head.
 */
static inline void qb_list_add(struct qb_list_head *element,
				struct qb_list_head *head)
{
	head->next->prev = element;
	element->next = head->next;
	element->prev = head;
	head->next = element;
}

/**
 * @brief Append an element at the tail of the list.
 *
 * The new element is placed immediately before @p head (i.e. at the
 * end of the list as seen by forward iteration).
 *
 * @param element Pointer to the new list node to insert.
 * @param head    Pointer to the list head.
 */
static inline void qb_list_add_tail(struct qb_list_head *element,
				     struct qb_list_head *head)
{
	head->prev->next = element;
	element->next = head;
	element->prev = head->prev;
	head->prev = element;
}

/**
 * @brief Remove an element from the list.
 *
 * Unlinks @p _remove from its neighbours.  The node's own pointers are
 * left unchanged; call qb_list_init() afterwards if a clean state is needed.
 *
 * @param _remove Pointer to the list node to remove.
 */
static inline void qb_list_del(struct qb_list_head *_remove)
{
	_remove->next->prev = _remove->prev;
	_remove->prev->next = _remove->next;
}

/**
 * @brief Replace one list node with another.
 *
 * Substitutes @p new_one for @p old_one in the list.  @p old_one is
 * left with stale pointers; call qb_list_init() on it if needed.
 *
 * @param old_one Pointer to the node being replaced.
 * @param new_one Pointer to the replacement node.
 */
static inline void qb_list_replace(struct qb_list_head *old_one,
				    struct qb_list_head *new_one)
{
	new_one->next = old_one->next;
	new_one->next->prev = new_one;
	new_one->prev = old_one->prev;
	new_one->prev->next = new_one;
}

/**
 * @brief Test whether a node is the last entry in a list.
 *
 * @param list Pointer to the node to test.
 * @param head Pointer to the list head.
 * @return Non-zero if @p list is the last node before @p head; zero otherwise.
 */
static inline int qb_list_is_last(const struct qb_list_head *list,
				   const struct qb_list_head *head)
{
	return list->next == head;
}

/**
 * @brief Test whether a list is empty.
 *
 * @param head Pointer to the list head.
 * @return Non-zero (true) if the list is empty; zero (false) otherwise.
 */
static inline int32_t qb_list_empty(const struct qb_list_head *head)
{
	return head->next == head;
}

/**
 * @brief Splice @p list into @p head at the front.
 *
 * Moves all nodes from @p list and inserts them at the beginning of
 * @p head, preserving their relative order.  Does nothing if @p list
 * is empty.
 *
 * @param list Pointer to the source list to be spliced in.
 * @param head Pointer to the destination list head.
 */
static inline void qb_list_splice(struct qb_list_head *list,
				   struct qb_list_head *head)
{
	struct qb_list_head *first = list->next;
	struct qb_list_head *last = list->prev;
	struct qb_list_head *at = head->next;

	if (first == list)
		return;

	first->prev = head;
	head->next = first;
	last->next = at;
	at->prev = last;
}

/**
 * @brief Splice @p list into @p head at the tail.
 *
 * Moves all nodes from @p list and appends them to the end of @p head,
 * preserving their relative order.  Does nothing if @p list is empty.
 *
 * @param list Pointer to the source list to be spliced in.
 * @param head Pointer to the destination list head.
 */
static inline void qb_list_splice_tail(struct qb_list_head *list,
					struct qb_list_head *head)
{
	struct qb_list_head *first = list->next;
	struct qb_list_head *last = list->prev;
	struct qb_list_head *at = head->prev;

	if (first == list)
		return;

	last->next = head;
	head->prev = last;
	first->prev = at;
	at->next = first;
}

/* Get the struct containing the list node */
/**
 * @brief Obtain a pointer to the enclosing struct from a list node.
 *
 * @param ptr    Pointer to the embedded @c qb_list_head member.
 * @param type   Type of the enclosing struct.
 * @param member Name of the @c qb_list_head field within @p type.
 * @return Pointer to the enclosing struct of @p type.
 */
#define qb_list_entry(ptr, type, member) \
    ((type *)((char *)(ptr) - offsetof(type, member)))

/* Get the first entry from a non-empty list */
/**
 * @brief Obtain the first entry in a non-empty list.
 *
 * @param head   Pointer to the list head.
 * @param type   Type of the enclosing struct.
 * @param member Name of the @c qb_list_head field within @p type.
 * @return Pointer to the first enclosing struct of @p type.
 */
#define qb_list_first_entry(head, type, member) \
    qb_list_entry((head)->next, type, member)

/* Iterate over all nodes (not safe against removal) */
/**
 * @brief Iterate over all nodes in a list.
 *
 * Not safe against removal of the current node during iteration.
 * Use qb_list_for_each_safe() if removal is needed.
 *
 * @param pos  Loop cursor (@c struct qb_list_head *).
 * @param head Pointer to the list head.
 */
#define qb_list_for_each(pos, head) \
    for ((pos) = (head)->next; (pos) != (head); (pos) = (pos)->next)

/* Iterate safely (pos may be removed during iteration) */
/**
 * @brief Iterate over all nodes in a list, safe against removal of the current node.
 *
 * @param pos  Loop cursor (@c struct qb_list_head *).
 * @param n    Temporary storage cursor (@c struct qb_list_head *).
 * @param head Pointer to the list head.
 */
#define qb_list_for_each_safe(pos, n, head) \
    for ((pos) = (head)->next, (n) = (pos)->next; \
         (pos) != (head); \
         (pos) = (n), (n) = (pos)->next)

/* Iterate over entries using container_of */
/**
 * @brief Iterate over entries of a given type embedded in the list.
 *
 * @param pos    Loop cursor (pointer to the enclosing struct type).
 * @param head   Pointer to the list head.
 * @param member Name of the @c qb_list_head field within the enclosing struct.
 */
#define qb_list_for_each_entry(pos, head, member) \
    for ((pos) = qb_list_entry((head)->next, __typeof__(*(pos)), member); \
         &(pos)->member != (head); \
         (pos) = qb_list_entry((pos)->member.next, __typeof__(*(pos)), member))

/* Safe iteration over entries */
/**
 * @brief Iterate over entries of a given type, safe against removal of the current entry.
 *
 * @param pos    Loop cursor (pointer to the enclosing struct type).
 * @param n      Temporary storage cursor (same pointer type as @p pos).
 * @param head   Pointer to the list head.
 * @param member Name of the @c qb_list_head field within the enclosing struct.
 */
#define qb_list_for_each_entry_safe(pos, n, head, member) \
    for ((pos) = qb_list_entry((head)->next, __typeof__(*(pos)), member), \
         (n) = qb_list_entry((pos)->member.next, __typeof__(*(pos)), member); \
         &(pos)->member != (head); \
         (pos) = (n), (n) = qb_list_entry((n)->member.next, __typeof__(*(pos)), member))

/* Reverse iteration over entries */
/**
 * @brief Iterate backwards over entries of a given type embedded in the list.
 *
 * @param pos    Loop cursor (pointer to the enclosing struct type).
 * @param head   Pointer to the list head.
 * @param member Name of the @c qb_list_head field within the enclosing struct.
 */
#define qb_list_for_each_entry_reverse(pos, head, member) \
    for ((pos) = qb_list_entry((head)->prev, __typeof__(*(pos)), member); \
         &(pos)->member != (head); \
         (pos) = qb_list_entry((pos)->member.prev, __typeof__(*(pos)), member))

/**
 * @brief Count the number of nodes in a list.
 *
 * Traverses the entire list; O(n) time complexity.
 *
 * @param head Pointer to the list head.
 * @return Number of nodes in the list.
 */
static inline int32_t qb_list_length(struct qb_list_head *head)
{
	struct qb_list_head *item;
	int32_t length = 0;

	qb_list_for_each(item, head)
		length++;

	return length;
}

/** @} */

#endif /* QB_LIST_H_DEFINED */
