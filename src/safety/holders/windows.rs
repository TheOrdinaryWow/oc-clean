//! Windows Restart Manager process-holder inspection.

use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use windows::Win32::Foundation::{ERROR_MORE_DATA, ERROR_SUCCESS, WIN32_ERROR};
use windows::Win32::System::RestartManager::{
    CCH_RM_SESSION_KEY, RM_PROCESS_INFO, RmEndSession, RmGetList, RmRegisterResources,
    RmStartSession,
};
use windows::core::{PCWSTR, PWSTR};

use super::{
    Completeness, HolderInfo, HolderInspector, Inspection, Verdict, database_related_paths,
};

/// Windows inspector backed by Restart Manager.
#[derive(Clone, Copy, Debug, Default)]
pub struct WindowsHolderInspector;

impl HolderInspector for WindowsHolderInspector {
    #[expect(
        unsafe_code,
        reason = "Restart Manager is a C API and holder detection has no safe Windows equivalent"
    )]
    fn inspect(&self, database_path: &Path) -> Inspection {
        let mut session_handle = 0;
        let mut session_key = vec![0_u16; CCH_RM_SESSION_KEY as usize + 1];
        // SAFETY: `session_handle` and `session_key` are writable for the sizes required by Restart Manager.
        let start = unsafe {
            RmStartSession(
                &raw mut session_handle,
                None,
                PWSTR(session_key.as_mut_ptr()),
            )
        };
        if start != ERROR_SUCCESS {
            return api_error("RmStartSession", start);
        }

        let inspection = inspect_session(session_handle, database_path);
        // SAFETY: `session_handle` was initialized by a successful `RmStartSession` call above.
        let end = unsafe { RmEndSession(session_handle) };
        if end != ERROR_SUCCESS {
            return api_error("RmEndSession", end);
        }
        inspection
    }
}

#[expect(
    unsafe_code,
    reason = "Restart Manager is a C API and holder detection has no safe Windows equivalent"
)]
fn inspect_session(session_handle: u32, database_path: &Path) -> Inspection {
    let targets = database_related_paths(database_path);
    let wide_paths = targets
        .iter()
        .map(|path| {
            path.as_os_str()
                .encode_wide()
                .chain(std::iter::once(0))
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    let path_pointers = wide_paths
        .iter()
        .map(|path| PCWSTR(path.as_ptr()))
        .collect::<Vec<_>>();
    // SAFETY: every `PCWSTR` points into `wide_paths`, which remains alive and NUL-terminated for this call.
    let register = unsafe { RmRegisterResources(session_handle, Some(&path_pointers), None, None) };
    if register != ERROR_SUCCESS {
        return api_error("RmRegisterResources", register);
    }

    let mut needed = 0;
    let mut count = 0;
    let mut reboot_reasons = 0;
    // SAFETY: all count and reason pointers are valid writable values; no output array is supplied for sizing.
    let first = unsafe {
        RmGetList(
            session_handle,
            &raw mut needed,
            &raw mut count,
            None,
            &raw mut reboot_reasons,
        )
    };
    if first == ERROR_SUCCESS && needed == 0 {
        return Inspection {
            verdict: Verdict::NotHeld,
            completeness: Completeness::CompleteForVisibleProcesses,
        };
    }
    if first != ERROR_MORE_DATA {
        return api_error("RmGetList sizing", first);
    }

    for _ in 0..3 {
        let mut process_info = vec![RM_PROCESS_INFO::default(); needed as usize];
        count = needed;
        // SAFETY: `process_info` has capacity for `count` initialized records and all scalar pointers are writable.
        let result = unsafe {
            RmGetList(
                session_handle,
                &raw mut needed,
                &raw mut count,
                Some(process_info.as_mut_ptr()),
                &raw mut reboot_reasons,
            )
        };
        if result == ERROR_MORE_DATA {
            continue;
        }
        if result != ERROR_SUCCESS {
            return api_error("RmGetList", result);
        }
        process_info.truncate(count as usize);
        let holders = process_info
            .into_iter()
            .map(|info| HolderInfo {
                pid: info.Process.dwProcessId,
                process_name: utf16_name(&info.strAppName),
                matched_paths: targets.to_vec(),
            })
            .collect::<Vec<_>>();
        return Inspection {
            verdict: if holders.is_empty() {
                Verdict::NotHeld
            } else {
                Verdict::Held(holders)
            },
            completeness: Completeness::CompleteForVisibleProcesses,
        };
    }
    Inspection {
        verdict: Verdict::CannotDetermine(
            "Restart Manager process list changed repeatedly during inspection".to_owned(),
        ),
        completeness: Completeness::PartialDueToPermissions,
    }
}

fn utf16_name(value: &[u16]) -> Option<String> {
    let end = value
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(value.len());
    let name = String::from_utf16_lossy(&value[..end]);
    (!name.is_empty()).then_some(name)
}

fn api_error(operation: &str, error: WIN32_ERROR) -> Inspection {
    Inspection {
        verdict: Verdict::CannotDetermine(format!(
            "{operation} failed with Windows error {}",
            error.0
        )),
        completeness: Completeness::PartialDueToPermissions,
    }
}

#[cfg(test)]
mod tests {
    use std::fs::File;

    use tempfile::NamedTempFile;

    use super::*;

    #[test]
    fn detects_current_process_holding_fixture() {
        let fixture = NamedTempFile::new().expect("fixture");
        let _held = File::open(fixture.path()).expect("open fixture");

        let inspection = WindowsHolderInspector.inspect(fixture.path());

        let Verdict::Held(holders) = inspection.verdict else {
            panic!("expected current process holder");
        };
        assert!(
            holders
                .iter()
                .any(|holder| holder.pid == std::process::id())
        );
    }
}
