//! Prints what Windows reports about this machine's monitors.
use hk_platform::Platform;

fn main() {
    let platform = hk_platform::native().expect("this operating system isn't supported");
    println!("model: {:?}", platform.model());
    println!("park point: {:?}", platform.park_point());
    for monitor in platform.monitors() {
        println!("{monitor:#?}");
    }
}
