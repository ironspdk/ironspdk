#include <spdk/json.h>
#include "json.h"

size_t u_json_object_len(const struct spdk_json_val *obj)
{
    return obj->len;
}

const struct spdk_json_val *u_json_val_name(const struct spdk_json_val *obj, size_t i)
{
    return &obj[i + 1];
}

const struct spdk_json_val *u_json_val_val(const struct spdk_json_val *obj, size_t i)
{
    return &obj[i + 2];
}

size_t u_json_val_len(const struct spdk_json_val *val)
{
    return spdk_json_val_len(val);
}

const char *u_json_val_str_ptr(const struct spdk_json_val *val)
{
    return (const char *)val->start;
}

size_t u_json_val_str_len(const struct spdk_json_val *val)
{
    return val->len;
}
