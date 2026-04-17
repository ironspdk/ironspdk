#pragma once

extern char *rs_handle_rpc_cmd(const char *cmd, const struct spdk_json_val *params);

#include <spdk/rpc.h>
#include "json.h"

#define RUST_SPDK_RPC(name) \
static \
void __rust_spdk_rpc_##name(struct spdk_jsonrpc_request *request, \
                  const struct spdk_json_val *params) \
{ \
    struct spdk_json_write_ctx *w; \
    char *out; \
\
    out = rs_handle_rpc_cmd(#name, params); \
    w = spdk_jsonrpc_begin_result(request); \
    spdk_json_write_string(w, out); \
    free(out); \
    spdk_jsonrpc_end_result(request, w); \
} \
SPDK_RPC_REGISTER(#name, __rust_spdk_rpc_##name, SPDK_RPC_RUNTIME)
