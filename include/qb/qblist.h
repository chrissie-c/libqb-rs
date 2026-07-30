#ifndef QB_LIST_H_DEFINED
#define QB_LIST_H_DEFINED

#include <stddef.h>
#include <stdint.h>
#include "qb.h"

/* Declare a statically-initialized list head */
#define QB_LIST_DECLARE(name) \
    struct qb_list_head name = { &(name), &(name) }

static inline void qb_list_init(struct qb_list_head *head)
{
	head->next = head;
	head->prev = head;
}

static inline void qb_list_add(struct qb_list_head *element,
				struct qb_list_head *head)
{
	head->next->prev = element;
	element->next = head->next;
	element->prev = head;
	head->next = element;
}

static inline void qb_list_add_tail(struct qb_list_head *element,
				     struct qb_list_head *head)
{
	head->prev->next = element;
	element->next = head;
	element->prev = head->prev;
	head->prev = element;
}

static inline void qb_list_del(struct qb_list_head *_remove)
{
	_remove->next->prev = _remove->prev;
	_remove->prev->next = _remove->next;
}

static inline void qb_list_replace(struct qb_list_head *old_one,
				    struct qb_list_head *new_one)
{
	new_one->next = old_one->next;
	new_one->next->prev = new_one;
	new_one->prev = old_one->prev;
	new_one->prev->next = new_one;
}

static inline int qb_list_is_last(const struct qb_list_head *list,
				   const struct qb_list_head *head)
{
	return list->next == head;
}

static inline int32_t qb_list_empty(const struct qb_list_head *head)
{
	return head->next == head;
}

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
#define qb_list_entry(ptr, type, member) \
    ((type *)((char *)(ptr) - offsetof(type, member)))

/* Get the first entry from a non-empty list */
#define qb_list_first_entry(head, type, member) \
    qb_list_entry((head)->next, type, member)

/* Iterate over all nodes (not safe against removal) */
#define qb_list_for_each(pos, head) \
    for ((pos) = (head)->next; (pos) != (head); (pos) = (pos)->next)

/* Iterate safely (pos may be removed during iteration) */
#define qb_list_for_each_safe(pos, n, head) \
    for ((pos) = (head)->next, (n) = (pos)->next; \
         (pos) != (head); \
         (pos) = (n), (n) = (pos)->next)

/* Iterate over entries using container_of */
#define qb_list_for_each_entry(pos, head, member) \
    for ((pos) = qb_list_entry((head)->next, __typeof__(*(pos)), member); \
         &(pos)->member != (head); \
         (pos) = qb_list_entry((pos)->member.next, __typeof__(*(pos)), member))

/* Safe iteration over entries */
#define qb_list_for_each_entry_safe(pos, n, head, member) \
    for ((pos) = qb_list_entry((head)->next, __typeof__(*(pos)), member), \
         (n) = qb_list_entry((pos)->member.next, __typeof__(*(pos)), member); \
         &(pos)->member != (head); \
         (pos) = (n), (n) = qb_list_entry((n)->member.next, __typeof__(*(pos)), member))

/* Reverse iteration over entries */
#define qb_list_for_each_entry_reverse(pos, head, member) \
    for ((pos) = qb_list_entry((head)->prev, __typeof__(*(pos)), member); \
         &(pos)->member != (head); \
         (pos) = qb_list_entry((pos)->member.prev, __typeof__(*(pos)), member))

static inline int32_t qb_list_length(struct qb_list_head *head)
{
	struct qb_list_head *item;
	int32_t length = 0;

	qb_list_for_each(item, head)
		length++;

	return length;
}

#endif /* QB_LIST_H_DEFINED */
