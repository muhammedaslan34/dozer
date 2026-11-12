//! Small Windows helpers: job objects, memory status, process killing.

use std::os::windows::process::CommandExt;
use std::process::Command;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject, TerminateJobObject,
};
use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};

pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// A Windows job object. Every process put in it, and every process those
/// start (npx -> node -> ...), is killed together when the job is dropped.
pub struct Job(HANDLE);

// The handle is only used through thread-safe kernel calls.
unsafe impl Send for Job {}
unsafe impl Sync for Job {}

impl Job {
    pub fn new() -> anyhow::Result<Self> {
        unsafe {
            let handle = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if handle.is_null() {
                anyhow::bail!("CreateJobObjectW failed");
            }
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let ok = SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const _,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            );
            if ok == 0 {
                CloseHandle(handle);
                anyhow::bail!("SetInformationJobObject failed");
            }
            Ok(Self(handle))
        }
    }

    pub fn assign(&self, child: &std::process::Child) -> anyhow::Result<()> {
        use std::os::windows::io::AsRawHandle;
        let ok = unsafe { AssignProcessToJobObject(self.0, child.as_raw_handle() as HANDLE) };
        if ok == 0 {
            anyhow::bail!("AssignProcessToJobObject failed");
        }
        Ok(())
    }

    pub fn terminate(&self) {
        unsafe {
            TerminateJobObject(self.0, 1);
        }
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        unsafe {
            TerminateJobObject(self.0, 1);
            CloseHandle(self.0);
        }
    }
}

#[derive(Clone, Copy, Default, Debug)]
pub struct MemoryStatus {
    pub total: u64,
    pub available: u64,
    pub commit_limit: u64,
    pub commit_available: u64,
}

pub fn memory_status() -> MemoryStatus {
    unsafe {
        let mut m: MEMORYSTATUSEX = std::mem::zeroed();
        m.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
        if GlobalMemoryStatusEx(&mut m) == 0 {
            return MemoryStatus::default();
        }
        MemoryStatus {
            total: m.ullTotalPhys,
            available: m.ullAvailPhys,
            commit_limit: m.ullTotalPageFile,
            commit_available: m.ullAvailPageFile,
        }
    }
}

/// Ends a process tree. `force: false` asks windows to close (like clicking X),
/// so apps can save; console programs ignore that and need `force: true`.
pub fn end_tree(pid: u32, force: bool) -> bool {
    let pid = pid.to_string();
    let mut args = vec!["/PID", pid.as_str(), "/T"];
    if force {
        args.push("/F");
    }
    Command::new("taskkill")
        .args(&args)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Runs a command without flashing a console window; returns (ok, stdout+stderr).
pub fn run_hidden(program: &str, args: &[&str]) -> (bool, String) {
    match Command::new(program).args(args).creation_flags(CREATE_NO_WINDOW).output() {
        Ok(o) => {
            let mut text = String::from_utf8_lossy(&o.stdout).into_owned();
            text.push_str(&String::from_utf8_lossy(&o.stderr));
            (o.status.success(), text.trim().to_string())
        }
        Err(err) => (false, err.to_string()),
    }
}
