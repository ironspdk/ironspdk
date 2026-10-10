pub mod ffi {
    #![allow(non_camel_case_types)]
    #![allow(non_snake_case)]
    #![allow(non_upper_case_globals)]
    #![allow(unnecessary_transmutes)]
    #![allow(clippy::all)]

    include!(concat!(env!("OUT_DIR"), "/ffi.rs"));

    impl Default for SpdkBdevOptsC {
        fn default() -> Self {
            unsafe { std::mem::zeroed() }
        }
    }
}

pub mod build_utils;
