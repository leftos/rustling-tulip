//! The security descriptor every tracer pipe is created with.
//!
//! An elevated tracer inherits its parent's full token, so a pipe left with
//! the default descriptor — which admits Everyone and ANONYMOUS read-only —
//! would let a Medium process of the same account talk to it. An ordinary
//! tracer's descriptor names only SYSTEM and the tracer's own user; an
//! elevated tracer's names SYSTEM and Administrators and carries a High
//! no-read-up mandatory label, so nothing below the daemon's
//! integrity can open the pipe (invariant 4, `docs/plans/elevated-daemon.md`).
//!
//! The token read is the tracer's own: the daemon crate's `integrity` module
//! does the same job for its connections, and the tracer cannot depend on it.

use anyhow::{Context as _, anyhow, ensure};
use interprocess::os::windows::security_descriptor::SecurityDescriptor;
use widestring::{U16CStr, U16CString};
use windows::Win32::Foundation::{CloseHandle, HANDLE, HLOCAL, LocalFree};
use windows::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows::Win32::Security::{
    GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, TOKEN_INFORMATION_CLASS,
    TOKEN_MANDATORY_LABEL, TOKEN_QUERY, TOKEN_USER, TokenIntegrityLevel, TokenUser,
};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows::core::PWSTR;

/// The integrity RID at or above which a token counts as elevated: High
/// (`0x3000`), System (`0x4000`) and Protected Process (`0x5000`).
const HIGH_INTEGRITY_RID: u32 = 0x3000;

/// The SDDL string a tracer pipe is created with. An ordinary tracer admits
/// SYSTEM and its own user, both full access; an elevated one admits SYSTEM
/// and Administrators and carries a High no-read-up label, so a process below
/// High cannot open it even as the same user.
#[must_use]
pub(crate) fn pipe_sddl(user_sid: &str, elevated: bool) -> String {
    if elevated {
        "D:P(A;;GA;;;SY)(A;;GA;;;BA)S:(ML;;NWNR;;;HI)".to_owned()
    } else {
        format!("D:P(A;;GA;;;SY)(A;;GA;;;{user_sid})")
    }
}

/// The descriptor every pipe instance is created with. Fails when the
/// tracer's own token or the descriptor string cannot be read, and the caller
/// then never binds with the default descriptor.
pub(crate) fn own_pipe_security_descriptor() -> anyhow::Result<SecurityDescriptor> {
    let token = read_own_token().context("reading the tracer's own token")?;
    let sddl = pipe_sddl(&token.user_sid, token.elevated);
    let wide = U16CString::from_str(&sddl)
        .map_err(|err| anyhow!("the pipe's security descriptor {sddl} is not encodable: {err}"))?;
    SecurityDescriptor::deserialize(&wide)
        .with_context(|| format!("parsing the pipe's security descriptor {sddl}"))
}

/// The tracer's own user SID and elevation.
struct OwnToken {
    user_sid: String,
    elevated: bool,
}

fn read_own_token() -> anyhow::Result<OwnToken> {
    let mut token = HANDLE::default();
    // SAFETY: the current process's pseudo-handle needs no closing; `token`
    // receives a handle closed by `OwnedHandle`.
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut token) }
        .context("opening its own process token")?;
    let token = OwnedHandle(token);
    Ok(OwnToken {
        user_sid: user_sid(token.0)?,
        elevated: integrity_rid(token.0)? >= HIGH_INTEGRITY_RID,
    })
}

/// The user SID in `token`, in the string form the descriptor names it by.
fn user_sid(token: HANDLE) -> anyhow::Result<String> {
    let user = token_information(token, TokenUser, "user")?;
    // SAFETY: the call filled `user` with a TOKEN_USER whose SID points into
    // the same buffer, which is alive and aligned.
    let sid = unsafe { (*user.as_ptr().cast::<TOKEN_USER>()).User.Sid };
    let mut wide = PWSTR::null();
    // SAFETY: `sid` is the valid user SID read above; `wide` receives a
    // LocalAlloc'd string freed below.
    unsafe { ConvertSidToStringSidW(sid, &raw mut wide) }.context("converting the user SID")?;
    let text = unsafe { U16CStr::from_ptr_str(wide.0.cast_const()) }.to_string()?;
    // SAFETY: `wide` is the string the conversion allocated.
    let _ = unsafe { LocalFree(Some(HLOCAL(wide.0.cast()))) };
    Ok(text)
}

/// The last sub-authority of `token`'s integrity label: the level's RID.
fn integrity_rid(token: HANDLE) -> anyhow::Result<u32> {
    let label = token_information(token, TokenIntegrityLevel, "integrity label")?;
    // SAFETY: the call filled `label` with a TOKEN_MANDATORY_LABEL whose SID
    // points into the same buffer, which is alive and aligned.
    let sid = unsafe { (*label.as_ptr().cast::<TOKEN_MANDATORY_LABEL>()).Label.Sid };
    // SAFETY: `sid` is the valid label SID read above.
    let count = unsafe { *GetSidSubAuthorityCount(sid) };
    ensure!(count > 0, "its integrity label has no sub-authority");
    // SAFETY: `count - 1` is below the SID's sub-authority count.
    Ok(unsafe { *GetSidSubAuthority(sid, u32::from(count - 1)) })
}

/// The token information `class` as `u64` words, aligned for the structures
/// the call returns. `what` names the information in a failure.
fn token_information(
    token: HANDLE,
    class: TOKEN_INFORMATION_CLASS,
    what: &str,
) -> anyhow::Result<Vec<u64>> {
    let mut needed = 0u32;
    // SAFETY: a size query with no buffer; it fails with
    // ERROR_INSUFFICIENT_BUFFER and sets `needed`.
    let _ = unsafe { GetTokenInformation(token, class, None, 0, &raw mut needed) };
    ensure!(needed > 0, "the token reported no {what} information");
    let mut buffer = vec![0u64; usize::try_from(needed)?.div_ceil(8)];
    // SAFETY: `buffer` holds at least `needed` bytes and outlives the call.
    unsafe {
        GetTokenInformation(
            token,
            class,
            Some(buffer.as_mut_ptr().cast()),
            needed,
            &raw mut needed,
        )
    }
    .with_context(|| format!("reading the token's {what} information"))?;
    Ok(buffer)
}

/// A handle closed when dropped.
struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        // SAFETY: the handle was opened by this module and is closed once.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

#[cfg(test)]
mod tests {
    use super::pipe_sddl;

    const USER_SID: &str = "S-1-5-21-1-2-3-1001";

    #[test]
    fn pipe_sddl_names_system_and_the_user_when_not_elevated() {
        assert_eq!(
            pipe_sddl(USER_SID, false),
            "D:P(A;;GA;;;SY)(A;;GA;;;S-1-5-21-1-2-3-1001)"
        );
    }

    #[test]
    fn pipe_sddl_names_system_and_administrators_with_a_high_label_when_elevated() {
        assert_eq!(
            pipe_sddl(USER_SID, true),
            "D:P(A;;GA;;;SY)(A;;GA;;;BA)S:(ML;;NWNR;;;HI)"
        );
    }
}
