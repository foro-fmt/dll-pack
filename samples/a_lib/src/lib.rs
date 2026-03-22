extern "C" {
    fn hello_world() -> *const u8;
}

#[no_mangle]
pub extern "C" fn call_hello_world() {
    unsafe {
        let c_str = hello_world();
        let message = std::ffi::CStr::from_ptr(c_str as *const i8)
            .to_str()
            .unwrap();
        println!("{}", message);
    }
}

#[no_mangle]
pub extern "C" fn adding(a: i32, b: i32) -> i32 {
    a + b
}
