use crate::c;
use std::env;
use std::ffi::CString;
use std::os::raw::c_void;
use std::ptr;

use thiserror::Error;

static mut SHUTDOWN_CX: *mut c_void = std::ptr::null_mut();

#[derive(Debug, Error)]
pub enum Error {
    #[error("No start callback (.on_start()) defined")]
    NoStartCbDefined,
    #[error("Failed to parse arguments: {0}")]
    ParseArgv(i32),
    #[error("SPDK application start failed: {0}")]
    Start(i32),
}

pub struct SpdkApp {
    name: CString,
    start_cb: Option<Box<dyn FnOnce()>>,
    shutdown_cb: Option<Box<dyn FnOnce()>>,
}

impl SpdkApp {
    pub fn new(name: &str) -> Self {
        Self {
            name: CString::new(name).expect("Error parsing name"),
            start_cb: None,
            shutdown_cb: None,
        }
    }

    pub fn on_start<F>(&mut self, f: F)
    where
        F: FnOnce() + 'static,
    {
        self.start_cb = Some(Box::new(f));
    }

    pub fn on_shutdown<F>(&mut self, f: F)
    where
        F: FnOnce() + 'static,
    {
        self.shutdown_cb = Some(Box::new(f));
    }

    pub fn run(self) -> Result<(), Error> {
        unsafe { self.run_spdk_app() }
    }

    unsafe fn run_spdk_app(self) -> Result<(), Error> {
        let mut opts_buf = vec![0u8; unsafe { c::u_spdk_app_opts_size() }];
        let opts = opts_buf.as_mut_ptr() as *mut c::spdk_app_opts;
        unsafe { c::u_spdk_app_opts_init(opts, self.name.as_ptr()) };

        let args: Vec<CString> = env::args().map(|arg| CString::new(arg).unwrap()).collect();
        let mut argv: Vec<*mut i8> = args.iter().map(|arg| arg.as_ptr() as *mut i8).collect();
        let argc = argv.len() as i32;

        let rc = unsafe { c::u_spdk_app_parse_args(argc, argv.as_mut_ptr(), opts) };
        if rc == 0 {
            // help command line argument
            return Ok(());
        }
        if rc != 1 {
            // error while parsing arguments
            return Err(Error::ParseArgv(rc));
        }

        if self.start_cb.is_none() {
            return Err(Error::NoStartCbDefined);
        }
        let start_cx = self
            .start_cb
            .map(|cb| Box::into_raw(Box::new(cb)) as *mut c_void)
            .unwrap();
        unsafe {
            SHUTDOWN_CX = match self.shutdown_cb {
                Some(cb) => Box::into_raw(Box::new(cb)) as *mut c_void,
                None => ptr::null_mut(),
            };
            c::u_spdk_app_set_shutdown_cb(opts, unsafe_spdk_app_shutdown);
        }

        let rc = unsafe { c::u_spdk_app_start(opts, unsafe_spdk_app_start, start_cx) };
        if rc != 0 {
            return Err(Error::Start(rc));
        }

        Ok(())
    }
}

/// Unsafe shutdown routine. Performs unsafe shutdown operations and calls shutdown()
extern "C" fn unsafe_spdk_app_shutdown() {
    if !(unsafe { SHUTDOWN_CX.is_null() }) {
        let cb = unsafe { Box::from_raw(SHUTDOWN_CX as *mut Box<dyn FnOnce()>) };
        cb();
    }
    unsafe { c::u_spdk_app_stop(0) };
}

extern "C" fn unsafe_spdk_app_start(cx: *mut c_void) {
    assert!(!cx.is_null());
    let cb = unsafe { Box::from_raw(cx as *mut Box<dyn FnOnce()>) };
    cb();
}
