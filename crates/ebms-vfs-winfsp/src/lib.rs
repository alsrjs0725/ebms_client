//! Windows WinFsp 백엔드. [`ebms_core::fs::ReadOnlyFs`]를 읽기 전용 드라이브(예: `E:`)로 마운트한다.
//! 보통 서버별 최상위 폴더로 합친 [`ebms_core::drive::Drive`]를 마운트한다.
//!
//! 파일을 연 프로세스를 열 때 기록해 두고, 그 프로세스가 구동기일 때만 곡 전체 다운로드(티켓 1개)를
//! 일으킬 수 있게 한다. 읽기는 WinFsp 비동기 인터페이스로 받아, 다운로드가 필요하면 디스패처 스레드를
//! 막지 않고 끝난 뒤 응답한다.
//!
//! WinFsp DLL은 실행 파일에 지연 로드로 링크하고(`build-support/winfsp_delayload.rs`), 마운트할 때
//! 설치 위치에서 찾는다. WinFsp가 없으면 마운트만 실패하고 앱은 그대로 돈다.
#![cfg(windows)]

mod process;

use std::ffi::c_void;
use std::path::Path;
use std::sync::{Arc, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use ebms_core::Error;
use ebms_core::fs::{Attr, Kind, ReadOnlyFs};
use ebms_core::tree::{Ino, ROOT};
use tokio::sync::{oneshot, watch};
use tracing::warn;
use windows::Win32::Foundation::{
    HLOCAL, LocalFree, STATUS_ACCESS_DENIED, STATUS_BUFFER_OVERFLOW, STATUS_CANCELLED,
    STATUS_END_OF_FILE, STATUS_NOT_A_DIRECTORY, STATUS_OBJECT_NAME_NOT_FOUND,
    STATUS_UNEXPECTED_IO_ERROR,
};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::PSECURITY_DESCRIPTOR;
use windows::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_READONLY};
use windows::core::w;
use winfsp::filesystem::{
    AsyncFileSystemContext, DirBuffer, DirInfo, DirMarker, FileInfo, FileSecurity,
    FileSystemContext, OpenFileInfo, VolumeInfo, WideNameInfo,
};
use winfsp::host::{FileSystemHost, FineGuard, VolumeParams};
use winfsp::{FspError, U16CStr};

use crate::process::ProcessCaller;

/// 트리는 동기화 때만 바뀌므로 파일 정보 캐시를 FUSE와 같이 60초 둔다.
const INFO_TIMEOUT_MS: u32 = 60_000;
/// 다운로드를 기다리는 읽기가 끊기지 않도록 WinFsp가 허용하는 최대(10분).
const IRP_TIMEOUT_MS: u32 = 600_000;
const SECTOR: u64 = 4096;
/// 시스템·관리자는 모든 권한, 나머지는 읽기·실행만. 볼륨이 읽기 전용이라 쓰기는 어차피 막힌다.
const SDDL: windows::core::PCWSTR = w!("O:BAG:BAD:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;FRFX;;;WD)");

struct WinFs {
    fs: Arc<dyn ReadOnlyFs>,
    /// 곡 전체를 받을 수 있는 프로그램 이름 ([`ebms_core::players`])
    players: Arc<[String]>,
    rt: tokio::runtime::Handle,
    /// 모든 파일·폴더에 쓰는 보안 설명자
    security: Vec<u8>,
    /// 마운트한 시각 (FILETIME). 파일 시각으로 보여준다.
    time: u64,
    /// 언마운트할 때 true. 다운로드를 기다리는 읽기를 바로 끝낸다.
    cancel: watch::Receiver<bool>,
}

/// 열린 파일·폴더 핸들.
struct Handle {
    ino: Ino,
    kind: Kind,
    /// 연 프로세스. WinFsp는 열 때만 알려준다.
    pid: u32,
    /// 폴더 목록 (폴더를 처음 읽을 때 채운다)
    dir: DirBuffer,
}

impl WinFs {
    fn resolve(&self, name: &U16CStr) -> winfsp::Result<Attr> {
        self.fs
            .resolve(&name.to_string_lossy())
            .ok_or_else(|| STATUS_OBJECT_NAME_NOT_FOUND.into())
    }

    fn fill(&self, info: &mut FileInfo, a: Attr) {
        info.file_attributes = match a.kind {
            // 폴더의 읽기 전용 속성은 탐색기가 "사용자 지정 폴더"로 해석하므로 붙이지 않는다.
            Kind::Dir => FILE_ATTRIBUTE_DIRECTORY.0,
            Kind::File => FILE_ATTRIBUTE_READONLY.0,
        };
        info.reparse_tag = 0;
        info.file_size = a.size;
        info.allocation_size = a.size.div_ceil(SECTOR) * SECTOR;
        info.creation_time = self.time;
        info.last_access_time = self.time;
        info.last_write_time = self.time;
        info.change_time = self.time;
        info.index_number = a.ino;
        info.hard_links = 0;
        info.ea_size = 0;
    }

    /// 보안 설명자를 `buf`에 복사한다. 작으면 [`STATUS_BUFFER_OVERFLOW`].
    fn copy_security(&self, buf: Option<&mut [c_void]>) -> winfsp::Result<u64> {
        if let Some(buf) = buf {
            if buf.len() < self.security.len() {
                return Err(STATUS_BUFFER_OVERFLOW.into());
            }
            // SAFETY: buf는 security.len() 바이트 이상 쓸 수 있다.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    self.security.as_ptr(),
                    buf.as_mut_ptr().cast::<u8>(),
                    self.security.len(),
                );
            }
        }
        Ok(self.security.len() as u64)
    }

    /// 폴더 목록을 WinFsp 폴더 버퍼에 채운다. `.`과 `..`은 루트에만 없다.
    fn fill_dir(&self, h: &Handle) -> winfsp::Result<()> {
        let lock = h.dir.acquire(true, None)?;
        let mut entries = Vec::new();
        if h.ino != ROOT {
            entries.push((".".to_string(), Some(h.ino)));
            entries.push(("..".to_string(), self.fs.parent(h.ino)));
        }
        // 콜백 안에서 다시 FS를 부르지 않도록 이름과 ino만 모은다.
        let is_dir = self.fs.readdir_from(h.ino, 0, &mut |e| {
            entries.push((e.name, Some(e.ino)));
            false
        });
        if !is_dir {
            return Err(STATUS_NOT_A_DIRECTORY.into());
        }
        let mut info: DirInfo<512> = DirInfo::new();
        for (name, ino) in entries {
            let Some(attr) = ino.and_then(|ino| self.fs.getattr(ino)) else {
                continue;
            };
            info.reset();
            // 이름은 트리에서 이미 255자 안으로 줄였다. 그래도 넘치면 그 항목만 뺀다.
            // `set_name`은 끝의 NUL까지 이름 길이에 넣는다. 그러면 이어 읽기 marker("a.wav")보다
            // 버퍼 속 이름("a.wav\0")이 커서 앞 페이지의 마지막 항목을 다시 주고, 한 응답에 다
            // 담기지 않는 큰 폴더는 나열이 끝나지 않는다.
            let wide: Vec<u16> = name.encode_utf16().collect();
            if info.set_name_raw(wide.as_slice()).is_err() {
                continue;
            }
            self.fill(info.file_info_mut(), attr);
            lock.write(&mut info)?;
        }
        Ok(())
    }
}

impl FileSystemContext for WinFs {
    type FileContext = Handle;

    fn get_security_by_name(
        &self,
        file_name: &U16CStr,
        security_descriptor: Option<&mut [c_void]>,
        _reparse_point_resolver: impl FnOnce(&U16CStr) -> Option<FileSecurity>,
    ) -> winfsp::Result<FileSecurity> {
        let attr = self.resolve(file_name)?;
        let mut info = FileInfo::default();
        self.fill(&mut info, attr);
        Ok(FileSecurity {
            reparse: false,
            sz_security_descriptor: self.copy_security(security_descriptor)?,
            attributes: info.file_attributes,
        })
    }

    fn open(
        &self,
        file_name: &U16CStr,
        _create_options: u32,
        _granted_access: winfsp_sys::FILE_ACCESS_RIGHTS,
        file_info: &mut OpenFileInfo,
    ) -> winfsp::Result<Handle> {
        let attr = self.resolve(file_name)?;
        self.fill(file_info.as_mut(), attr);
        // SAFETY: Open 요청을 처리하는 중이다 (요청 정보는 이 스레드에 있다).
        let pid = unsafe { winfsp_sys::FspFileSystemOperationProcessIdF() };
        Ok(Handle {
            ino: attr.ino,
            kind: attr.kind,
            pid,
            dir: DirBuffer::new(),
        })
    }

    fn close(&self, _context: Handle) {}

    fn get_file_info(&self, context: &Handle, file_info: &mut FileInfo) -> winfsp::Result<()> {
        let attr = self
            .fs
            .getattr(context.ino)
            .ok_or(FspError::from(STATUS_OBJECT_NAME_NOT_FOUND))?;
        self.fill(file_info, attr);
        Ok(())
    }

    fn get_security(
        &self,
        _context: &Handle,
        security_descriptor: Option<&mut [c_void]>,
    ) -> winfsp::Result<u64> {
        self.copy_security(security_descriptor)
    }

    fn read_directory(
        &self,
        context: &Handle,
        _pattern: Option<&U16CStr>,
        marker: DirMarker,
        buffer: &mut [u8],
    ) -> winfsp::Result<u32> {
        if context.kind != Kind::Dir {
            return Err(STATUS_NOT_A_DIRECTORY.into());
        }
        // 처음부터 읽을 때만 다시 채운다. 이어 읽기는 채워 둔 버퍼에서 marker 다음부터.
        if marker.is_none() {
            self.fill_dir(context)?;
        }
        Ok(context.dir.read(marker, buffer))
    }

    fn get_volume_info(&self, out_volume_info: &mut VolumeInfo) -> winfsp::Result<()> {
        out_volume_info.total_size = 0;
        out_volume_info.free_size = 0;
        out_volume_info.set_volume_label("EBMS");
        Ok(())
    }
}

impl AsyncFileSystemContext for WinFs {
    fn read_async(
        &self,
        context: &Handle,
        buffer: &mut [u8],
        offset: u64,
    ) -> impl Future<Output = winfsp::Result<u32>> + Send {
        let (tx, rx) = oneshot::channel();
        let caller = Box::new(ProcessCaller::new(context.pid, self.players.clone()));
        let size = u32::try_from(buffer.len()).unwrap_or(u32::MAX);
        let ino = context.ino;
        let mut cancel = self.cancel.clone();
        async move {
            // 캐시에 있으면 바로, 다운로드가 필요하면 끝난 뒤 런타임 스레드에서 tx로 온다.
            self.fs.read_async(
                ino,
                offset,
                size,
                caller,
                Box::new(move |result| {
                    let _ = tx.send(result);
                }),
            );
            let result = tokio::select! {
                result = rx => result.unwrap_or_else(|_| Err(Error::Other("read dropped".into()))),
                _ = cancel.wait_for(|c| *c) => return Err(STATUS_CANCELLED.into()),
            };
            match result {
                Ok(data) if data.is_empty() && !buffer.is_empty() => Err(STATUS_END_OF_FILE.into()),
                Ok(data) => {
                    let n = data.len().min(buffer.len());
                    buffer[..n].copy_from_slice(&data[..n]);
                    Ok(n as u32)
                }
                // 이미 로그를 남겼다. 다른 프로그램에는 권한 없음으로 보인다.
                Err(Error::NotPlayer { .. }) => Err(STATUS_ACCESS_DENIED.into()),
                Err(e) => {
                    warn!(ino, %e, "read failed");
                    Err(STATUS_UNEXPECTED_IO_ERROR.into())
                }
            }
        }
    }

    fn spawn_task(&self, future: impl Future<Output = ()> + Send + 'static) {
        self.rt.spawn(future);
    }
}

/// 마운트된 드라이브. drop하면 언마운트된다.
pub struct Mounted {
    cancel: watch::Sender<bool>,
    host: Option<FileSystemHost<WinFs, FineGuard>>,
}

impl Drop for Mounted {
    fn drop(&mut self) {
        // 호스트는 drop할 때 진행 중인 읽기가 모두 끝나기를 기다리므로, 다운로드 대기를 먼저 끊는다.
        let _ = self.cancel.send(true);
        drop(self.host.take());
    }
}

/// 마운트한다. `mountpoint`는 드라이브 문자(`E:`)나 아직 없는 폴더 경로.
/// `players`([`ebms_core::config::Config::players`])에 해당하는 프로세스만 받지 않은 플레이 파일을
/// 읽어 곡 전체를 받을 수 있다. 읽기 응답은 `rt`에서 돈다.
pub fn spawn_mount(
    fs: Arc<dyn ReadOnlyFs>,
    mountpoint: &Path,
    players: Vec<String>,
    rt: tokio::runtime::Handle,
) -> std::io::Result<Mounted> {
    init()?;
    let mut params = VolumeParams::new();
    params
        .filesystem_name("EBMS")
        .sector_size(SECTOR as u16)
        .sectors_per_allocation_unit(1)
        .max_component_length(255)
        .case_sensitive_search(false)
        .case_preserved_names(true)
        .unicode_on_disk(true)
        .persistent_acls(true)
        .read_only_volume(true)
        .post_cleanup_when_modified_only(true)
        .file_info_timeout(INFO_TIMEOUT_MS)
        .irp_timeout(IRP_TIMEOUT_MS);
    let (cancel_tx, cancel) = watch::channel(false);
    let context = WinFs {
        fs,
        players: players.into(),
        rt,
        security: security_descriptor()?,
        time: filetime_now(),
        cancel,
    };
    let mut host: FileSystemHost<WinFs, FineGuard> = FileSystemHost::new_async(params, context)?;
    host.mount(mountpoint.as_os_str())?;
    host.start()?;
    Ok(Mounted {
        cancel: cancel_tx,
        host: Some(host),
    })
}

/// 쓸 수 있는 드라이브 문자 중 가장 뒤의 것 (예: `Z:`). 없으면 None.
pub fn free_drive_letter() -> Option<String> {
    // SAFETY: 인자가 없는 조회 함수다.
    let used = unsafe { windows::Win32::Storage::FileSystem::GetLogicalDrives() };
    // A·B는 플로피 자리라 피하고, 뒤에서부터 고른다.
    (2..26u32)
        .rev()
        .find(|i| used & (1 << i) == 0)
        .map(|i| format!("{}:", (b'A' + i as u8) as char))
}

/// WinFsp DLL을 한 번 불러온다. 설치 폴더(레지스트리)를 먼저 보고, 없으면 기본 검색 경로.
fn init() -> std::io::Result<()> {
    static INIT: OnceLock<Result<(), String>> = OnceLock::new();
    INIT.get_or_init(|| {
        load_installed_dll();
        winfsp::winfsp_init()
            .map(|_| ())
            .map_err(|_| "WinFsp is not installed (https://winfsp.dev/rel/)".to_string())
    })
    .clone()
    .map_err(std::io::Error::other)
}

/// 레지스트리의 WinFsp 설치 폴더에서 DLL을 불러 둔다. 이후 지연 로드는 이미 불러온 DLL을 쓴다.
fn load_installed_dll() {
    use windows::Win32::System::LibraryLoader::LoadLibraryW;
    use windows::Win32::System::Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RegGetValueW};

    let dll = if cfg!(target_arch = "x86_64") {
        "winfsp-x64.dll"
    } else if cfg!(target_arch = "aarch64") {
        "winfsp-a64.dll"
    } else {
        "winfsp-x86.dll"
    };
    for key in [w!("SOFTWARE\\WOW6432Node\\WinFsp"), w!("SOFTWARE\\WinFsp")] {
        let mut buf = [0u16; 1024];
        let mut size = std::mem::size_of_val(&buf) as u32;
        // SAFETY: buf와 size는 호출 동안 유효하다.
        let status = unsafe {
            RegGetValueW(
                HKEY_LOCAL_MACHINE,
                key,
                w!("InstallDir"),
                RRF_RT_REG_SZ,
                None,
                Some(buf.as_mut_ptr().cast()),
                Some(&mut size),
            )
        };
        if status.is_err() {
            continue;
        }
        let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        let dir = String::from_utf16_lossy(&buf[..len]);
        let path = Path::new(&dir).join("bin").join(dll);
        // SAFETY: 경로 문자열은 호출 동안 유효하다.
        if unsafe { LoadLibraryW(&windows::core::HSTRING::from(path.as_os_str())) }.is_ok() {
            return;
        }
    }
}

/// [`SDDL`]을 이진 보안 설명자로 바꾼다.
fn security_descriptor() -> std::io::Result<Vec<u8>> {
    let mut sd = PSECURITY_DESCRIPTOR::default();
    let mut len = 0u32;
    // SAFETY: 출력 포인터는 호출 동안 유효하고, 결과는 LocalFree로 해제한다.
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            SDDL,
            SDDL_REVISION_1,
            &mut sd,
            Some(&mut len),
        )?;
        let bytes = std::slice::from_raw_parts(sd.0.cast::<u8>(), len as usize).to_vec();
        let _ = LocalFree(Some(HLOCAL(sd.0)));
        Ok(bytes)
    }
}

/// 지금 시각을 FILETIME(1601년부터 100ns 단위)으로.
fn filetime_now() -> u64 {
    const UNIX_TO_1601_SECS: u64 = 11_644_473_600;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    (now.as_secs() + UNIX_TO_1601_SECS) * 10_000_000 + u64::from(now.subsec_nanos() / 100)
}
