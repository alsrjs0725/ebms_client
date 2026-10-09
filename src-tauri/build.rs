include!("../build-support/winfsp_delayload.rs");

fn main() {
    winfsp_delayload();
    tauri_build::build()
}
