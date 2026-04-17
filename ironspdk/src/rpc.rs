// JSON-RPC handling
use crate::Error;
use crate::c;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::ffi::CStr;
use std::os::raw::c_char;
use std::sync::OnceLock;

#[macro_export]
macro_rules! rpc_register {
    ($cmd:expr, $handler:ident) => {
        paste! {
            #[ctor]
            #[allow(non_snake_case)]
            fn [<__rpc_register_ $handler >]() {
                $crate::rpc::register_rpc_cmd_handler($cmd, $handler);
            }
        }
    };
}

pub type RpcCmd = String;
pub type RpcCmdArgs = HashMap<RpcCmd, String>;
pub type RpcCmdResult = Result<String, Error>;

type RpcHandler = fn(HashMap<String, String>) -> RpcCmdResult;

static DISPATCH_TABLE: OnceLock<Mutex<HashMap<&'static str, RpcHandler>>> = OnceLock::new();

fn dispatch_table() -> &'static Mutex<HashMap<&'static str, RpcHandler>> {
    DISPATCH_TABLE.get_or_init(|| Mutex::new(HashMap::new()))
}

struct JsonIter {
    obj: *const c::spdk_json_val,
    idx: usize,
}

type JsonVal = (String, String);

impl JsonIter {
    pub fn new(obj: *const c::spdk_json_val) -> Self {
        Self { obj, idx: 0 }
    }
}

fn json_val_to_string(val: *const c::spdk_json_val) -> String {
    let ptr = unsafe { c::u_json_val_str_ptr(val) };
    let len = unsafe { c::u_json_val_str_len(val) };
    let slice = unsafe { std::slice::from_raw_parts(ptr as *const u8, len) };
    String::from_utf8_lossy(slice).to_string()
}

impl Iterator for JsonIter {
    type Item = JsonVal;

    fn next(&mut self) -> Option<JsonVal> {
        if self.idx < unsafe { c::u_json_object_len(self.obj) } {
            let name = unsafe { c::u_json_val_name(self.obj, self.idx) };
            let val = unsafe { c::u_json_val_val(self.obj, self.idx) };
            let name_str = json_val_to_string(name);
            let val_str = json_val_to_string(val);
            self.idx += 1 + unsafe { c::u_json_val_len(val) };
            Some((name_str, val_str))
        } else {
            None
        }
    }
}

pub fn register_rpc_cmd_handler(cmd: &'static str, handler: RpcHandler) {
    dispatch_table().lock().insert(cmd, handler);
}

fn handle_rpc_cmd(cmd: &str, args: RpcCmdArgs) -> RpcCmdResult {
    dispatch_table()
        .lock()
        .get(cmd)
        .map(|handler| handler(args))
        .unwrap_or_else(|| Err(Error::RpcCmdUnknown(cmd.to_string())))
}

/// # SAFETY
/// Caller must provide valid `cmd_c_ptr` and `params_c_ptr` arguments
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rs_handle_rpc_cmd(
    cmd_c_ptr: *const c_char,
    params_c_ptr: *const c::spdk_json_val,
) -> *mut c_char {
    let args: HashMap<String, String> = JsonIter::new(params_c_ptr).collect();
    let cmd_c_str = unsafe { CStr::from_ptr(cmd_c_ptr) };
    let cmd = cmd_c_str
        .to_str()
        .expect("Invalid RPC command name")
        .to_string();
    let out = match handle_rpc_cmd(cmd.as_str(), args) {
        Ok(result) => result,
        Err(err) => format!("Error: {}", err),
    };
    std::ffi::CString::new(out).unwrap().into_raw()
}
