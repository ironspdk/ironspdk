#pragma once

size_t u_json_object_len(const struct spdk_json_val *obj);

const struct spdk_json_val *u_json_val_name(const struct spdk_json_val *obj, size_t i);

const struct spdk_json_val *u_json_val_val(const struct spdk_json_val *obj, size_t i);

size_t u_json_val_len(const struct spdk_json_val *val);

const char *u_json_val_str_ptr(const struct spdk_json_val *val);

size_t u_json_val_str_len(const struct spdk_json_val *val);
