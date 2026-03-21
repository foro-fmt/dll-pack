use std::thread;

#[no_mangle]
pub extern "C" fn adding_with_sleep(a: i32, b: i32) -> i32 {
    thread::sleep(std::time::Duration::from_secs(1));
    a + b
}
