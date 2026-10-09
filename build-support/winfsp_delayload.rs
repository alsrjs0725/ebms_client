// build.rs에서 `include!`로 쓰는 공용 코드.

/// WinFsp DLL을 지연 로드하도록 링크한다. WinFsp가 설치되지 않은 PC에서도 실행 파일이 뜨고,
/// 마운트할 때 설치 위치에서 DLL을 찾는다 (`ebms-vfs-winfsp`). Windows(MSVC)가 아니면 아무것도 하지 않는다.
#[allow(dead_code)]
fn winfsp_delayload() {
    let var = |k: &str| std::env::var(k).unwrap_or_default();
    if var("CARGO_CFG_TARGET_OS") != "windows" || var("CARGO_CFG_TARGET_ENV") != "msvc" {
        return;
    }
    let dll = match var("CARGO_CFG_TARGET_ARCH").as_str() {
        "x86_64" => "winfsp-x64.dll",
        "x86" => "winfsp-x86.dll",
        "aarch64" => "winfsp-a64.dll",
        _ => return,
    };
    println!("cargo:rustc-link-lib=dylib=delayimp");
    println!("cargo:rustc-link-arg=/DELAYLOAD:{dll}");
}
