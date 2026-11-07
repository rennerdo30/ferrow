//! `ferrow-update --version <x.y.z[-pre]>`: the elevated half of an
//! in-app update (Windows). The app starts it with administrator consent
//! and passes only the version; this program then:
//!
//! 1. creates a fresh administrator-only `%ProgramFiles%\Ferrow-update-<random>`
//!    folder, every ancestor checked without following reparse points;
//! 2. downloads `SHA256SUMS.txt` and the setup itself from the fixed GitHub
//!    release URL of that version (Windows' own `curl.exe`, HTTPS only,
//!    bounded time and size);
//! 3. hashes the setup through a handle that denies writes and deletes,
//!    and keeps that handle while setup runs;
//! 4. runs `setup /S` only when the hash matches, waits at most 3 minutes;
//! 5. reopens Ferrow unelevated as the desktop's user.
//!
//! It takes no URL, path or checksum from the caller. Exit codes are in
//! [`ferrow_update::exit`].

use std::process::ExitCode;

fn main() -> ExitCode {
    #[cfg(windows)]
    {
        let args: Vec<String> = std::env::args().skip(1).collect();
        match win::run(&args) {
            Ok(()) => ExitCode::SUCCESS,
            Err((code, message)) => {
                eprintln!("ferrow-update: {message}");
                ExitCode::from(code)
            }
        }
    }
    #[cfg(not(windows))]
    {
        eprintln!(
            "ferrow-update: in-app updates are Windows-only; update through your package manager"
        );
        ExitCode::from(ferrow_update::exit::USAGE)
    }
}

#[cfg(windows)]
mod win {
    use ferrow_update::{SUMS, Version, asset_url, exit, expected_hash, setup_name};
    use std::io::Read;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::time::{Duration, Instant};
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ,
    };

    type Failure = (u8, String);

    const SETUP_LIMIT: u64 = 256 * 1024 * 1024;
    const SUMS_LIMIT: usize = 64 * 1024;
    const SETUP_TIMEOUT: Duration = Duration::from_secs(180);

    pub fn run(args: &[String]) -> Result<(), Failure> {
        let version = match args {
            [flag, v] if flag == "--version" => Version::parse(v),
            _ => None,
        }
        .ok_or((
            exit::USAGE,
            "usage: ferrow-update --version <x.y.z[-pre]>".into(),
        ))?;
        let stage = ferrowd::protection::update_stage("Ferrow-update-")
            .map_err(|e| (exit::STAGE, format!("update folder: {e}")))?;
        let name = setup_name(&version);
        let setup = stage.join(&name);
        let result = install(&version, &name, &setup);
        // Only what this run created: the setup file and the empty folder.
        let _ = std::fs::remove_file(&setup);
        let _ = std::fs::remove_dir(&stage);
        result?;
        reopen_app();
        Ok(())
    }

    fn install(version: &Version, name: &str, setup: &Path) -> Result<(), Failure> {
        let sums = curl(
            &["--max-filesize", &SUMS_LIMIT.to_string()],
            &asset_url(version, SUMS),
            None,
        )
        .map_err(|e| (exit::DOWNLOAD, e))?;
        if sums.len() > SUMS_LIMIT {
            return Err((exit::DOWNLOAD, format!("{SUMS} is too large")));
        }
        let expected = expected_hash(&String::from_utf8_lossy(&sums), name)
            .map_err(|e| (exit::CHECKSUM, e))?;
        curl(
            &["--max-filesize", &SETUP_LIMIT.to_string()],
            &asset_url(version, name),
            Some(setup),
        )
        .map_err(|e| (exit::DOWNLOAD, e))?;

        // Pin the file: no one may write or delete it from here on.
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(setup)
            .map_err(|e| (exit::STAGE, format!("open setup: {e}")))?;
        let meta = file.metadata().map_err(|e| (exit::STAGE, e.to_string()))?;
        if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
            || !meta.is_file()
            || meta.len() > SETUP_LIMIT
        {
            return Err((
                exit::STAGE,
                "the downloaded setup is not a plain file".into(),
            ));
        }
        ferrowd::protection::validate_protected_file(setup)
            .map_err(|e| (exit::STAGE, format!("setup file: {e}")))?;
        let actual = sha256(&mut file).map_err(|e| (exit::STAGE, format!("hash: {e}")))?;
        if actual != expected {
            return Err((
                exit::CHECKSUM,
                format!("checksum mismatch: expected {expected}, got {actual}"),
            ));
        }

        let mut child = Command::new(setup)
            .arg("/S")
            .spawn()
            .map_err(|e| (exit::SETUP_FAILED, format!("start setup: {e}")))?;
        let deadline = Instant::now() + SETUP_TIMEOUT;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(250))
                }
                // Never kill setup half-way; it finishes or rolls back itself.
                Ok(None) => {
                    return Err((exit::SETUP_TIMEOUT, "setup did not finish in time".into()));
                }
                Err(e) => return Err((exit::SETUP_FAILED, e.to_string())),
            }
        };
        drop(file);
        if !status.success() {
            return Err((exit::SETUP_FAILED, format!("setup exited with {status}")));
        }
        Ok(())
    }

    /// Windows' own curl (System32, never PATH): HTTPS only, redirects
    /// only to HTTPS, fail on HTTP errors, bounded time.
    fn curl(extra: &[&str], url: &str, output: Option<&Path>) -> Result<Vec<u8>, String> {
        let mut command = Command::new(system32("curl.exe"));
        command.args([
            "--proto",
            "=https",
            "--proto-redir",
            "=https",
            "--tlsv1.2",
            "--fail",
            "--silent",
            "--show-error",
            "--location",
            "--connect-timeout",
            "20",
            "--max-time",
            "300",
            "--user-agent",
            "ferrow-update",
        ]);
        command.args(extra);
        if let Some(path) = output {
            command.arg("--output").arg(path);
        }
        command.arg(url);
        let out = command.output().map_err(|e| format!("curl: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "download of {url} failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        Ok(out.stdout)
    }

    fn system32(file: &str) -> PathBuf {
        use windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW;
        let mut buf = [0u16; 260];
        // SAFETY: the buffer and its length match; the API writes at most that many units.
        let n = unsafe { GetSystemDirectoryW(buf.as_mut_ptr(), buf.len() as u32) } as usize;
        let dir = if n > 0 && n < buf.len() {
            PathBuf::from(String::from_utf16_lossy(&buf[..n]))
        } else {
            PathBuf::from(r"C:\Windows\System32")
        };
        dir.join(file)
    }

    /// SHA-256 through Windows CNG, lowercase hex.
    fn sha256(file: &mut std::fs::File) -> std::io::Result<String> {
        use windows_sys::Win32::Security::Cryptography::{
            BCRYPT_HASH_HANDLE, BCRYPT_SHA256_ALG_HANDLE, BCryptCreateHash, BCryptDestroyHash,
            BCryptFinishHash, BCryptHashData,
        };
        let check = |status: i32| {
            if status < 0 {
                Err(std::io::Error::other(format!("CNG error {status:#x}")))
            } else {
                Ok(())
            }
        };
        let mut hash: BCRYPT_HASH_HANDLE = std::ptr::null_mut();
        // SAFETY: pseudo algorithm handle (Windows 10+), no object buffer, no secret.
        check(unsafe {
            BCryptCreateHash(
                BCRYPT_SHA256_ALG_HANDLE,
                &raw mut hash,
                std::ptr::null_mut(),
                0,
                std::ptr::null(),
                0,
                0,
            )
        })?;
        let result = (|| {
            let mut buf = vec![0u8; 1 << 16];
            loop {
                let n = file.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                // SAFETY: live hash handle; buffer slice of n initialized bytes.
                check(unsafe { BCryptHashData(hash, buf.as_ptr(), n as u32, 0) })?;
            }
            let mut digest = [0u8; 32];
            // SAFETY: live hash handle; SHA-256 output is exactly 32 bytes.
            check(unsafe { BCryptFinishHash(hash, digest.as_mut_ptr(), 32, 0) })?;
            Ok(digest.iter().map(|b| format!("{b:02x}")).collect())
        })();
        // SAFETY: the handle was created above and is destroyed exactly once.
        unsafe { BCryptDestroyHash(hash) };
        result
    }

    /// Start the app as the user who owns the desktop (the shell's
    /// token), never with this process's elevated token. Falls back to
    /// explorer.exe, which hands the start to the running shell.
    fn reopen_app() {
        let Ok(exe) = std::env::current_exe().map(|p| p.with_file_name("ferrow-gui.exe")) else {
            return;
        };
        if !exe.is_file() {
            return;
        }
        if start_as_shell_user(&exe) {
            return;
        }
        let windir =
            std::env::var_os("WINDIR").map_or_else(|| PathBuf::from(r"C:\Windows"), PathBuf::from);
        let _ = Command::new(windir.join("explorer.exe")).arg(&exe).spawn();
    }

    fn start_as_shell_user(exe: &Path) -> bool {
        use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
        use windows_sys::Win32::Security::{
            DuplicateTokenEx, SecurityImpersonation, TOKEN_ADJUST_DEFAULT, TOKEN_ADJUST_SESSIONID,
            TOKEN_ASSIGN_PRIMARY, TOKEN_DUPLICATE, TOKEN_QUERY, TokenPrimary,
        };
        use windows_sys::Win32::System::Threading::{
            CreateProcessWithTokenW, OpenProcess, OpenProcessToken, PROCESS_INFORMATION,
            PROCESS_QUERY_LIMITED_INFORMATION, STARTUPINFOW,
        };
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            GetShellWindow, GetWindowThreadProcessId,
        };

        struct Owned(HANDLE);
        impl Drop for Owned {
            fn drop(&mut self) {
                if !self.0.is_null() {
                    // SAFETY: a handle this function opened, closed once.
                    unsafe { CloseHandle(self.0) };
                }
            }
        }
        let wide = |s: &std::ffi::OsStr| -> Vec<u16> { s.encode_wide().chain(Some(0)).collect() };
        // SAFETY: plain Win32 calls with live out-pointers; every handle is
        // owned by an `Owned` guard and closed on all paths.
        unsafe {
            let shell = GetShellWindow();
            if shell.is_null() {
                return false;
            }
            let mut pid = 0u32;
            GetWindowThreadProcessId(shell, &raw mut pid);
            if pid == 0 {
                return false;
            }
            let process = Owned(OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid));
            if process.0.is_null() {
                return false;
            }
            let mut token = Owned(std::ptr::null_mut());
            if OpenProcessToken(process.0, TOKEN_DUPLICATE, &raw mut token.0) == 0 {
                return false;
            }
            let mut primary = Owned(std::ptr::null_mut());
            if DuplicateTokenEx(
                token.0,
                TOKEN_QUERY
                    | TOKEN_DUPLICATE
                    | TOKEN_ASSIGN_PRIMARY
                    | TOKEN_ADJUST_DEFAULT
                    | TOKEN_ADJUST_SESSIONID,
                std::ptr::null(),
                SecurityImpersonation,
                TokenPrimary,
                &raw mut primary.0,
            ) == 0
            {
                return false;
            }
            let app = wide(exe.as_os_str());
            let mut command_line = wide(std::ffi::OsStr::new(&format!("\"{}\"", exe.display())));
            let dir = exe.parent().map(|d| wide(d.as_os_str()));
            let mut startup: STARTUPINFOW = std::mem::zeroed();
            startup.cb = size_of::<STARTUPINFOW>() as u32;
            let mut info: PROCESS_INFORMATION = std::mem::zeroed();
            let ok = CreateProcessWithTokenW(
                primary.0,
                0,
                app.as_ptr(),
                command_line.as_mut_ptr(),
                0,
                std::ptr::null(),
                dir.as_ref().map_or(std::ptr::null(), |d| d.as_ptr()),
                &startup,
                &raw mut info,
            ) != 0;
            if ok {
                drop(Owned(info.hThread));
                drop(Owned(info.hProcess));
            }
            ok
        }
    }
}
