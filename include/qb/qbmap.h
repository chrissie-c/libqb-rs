#ifndef QB_MAP_H_DEFINED
#define QB_MAP_H_DEFINED

#include "qb.h"

/* Both the struct tag and the _t typedef exist in the original libqb */
typedef struct qb_map_iter qb_map_iter_t;

/* Map notify event flag aliases (from QbMapNotifyFlags) */
#define QB_MAP_NOTIFY_DELETED   1
#define QB_MAP_NOTIFY_REPLACED  2
#define QB_MAP_NOTIFY_INSERTED  4
#define QB_MAP_NOTIFY_RECURSIVE 8
#define QB_MAP_NOTIFY_FREE      16


#endif /* QB_MAP_H_DEFINED */
