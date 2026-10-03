//! Minimal Linux-PAM binding, for the daemon shell.
//!
//! The shell authenticates every session against the host's own accounts: the
//! browser sends a username and password, the daemon runs the PAM stack named
//! by its service file and, only if that succeeds, starts a shell as that
//! user. Nothing is cached, and the password is never stored — a new shell is
//! a new login.
//!
//! Bound by hand rather than through a crate: this needs exactly four calls
//! (`pam_start`, `pam_authenticate`, `pam_acct_mgmt`, `pam_end`) plus
//! `pam_strerror` and one conversation function. `daemon/install.sh` installs
//! the service file these calls name. The daemon is the privileged process, so
//! it is the only part of the system that should ever see a host password.

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::path::Path;

const PAM_SUCCESS: c_int = 0;
const PAM_PROMPT_ECHO_OFF: c_int = 1;
const PAM_PROMPT_ECHO_ON: c_int = 2;
const PAM_ERROR_MSG: c_int = 3;
const PAM_TEXT_INFO: c_int = 4;
const PAM_CONV_ERR: c_int = 19;

/// The service file `install.sh` writes. A source checkout that has not run
/// the installer falls back to `login`, so the host's own account policy still
/// decides; a build from a package always has the dedicated file.
pub const SERVICE: &str = "virt-man-daemon";

#[repr(C)]
struct PamMessage {
    msg_style: c_int,
    msg: *const c_char,
}

#[repr(C)]
struct PamResponse {
    resp: *mut c_char,
    resp_retcode: c_int,
}

type ConvFn = unsafe extern "C" fn(
    num_msg: c_int,
    msg: *mut *const PamMessage,
    resp: *mut *mut PamResponse,
    appdata: *mut c_void,
) -> c_int;

#[repr(C)]
struct PamConv {
    conv: Option<ConvFn>,
    appdata: *mut c_void,
}

#[link(name = "pam")]
unsafe extern "C" {
    fn pam_start(
        service: *const c_char,
        user: *const c_char,
        conv: *const PamConv,
        handle: *mut *mut c_void,
    ) -> c_int;
    fn pam_authenticate(handle: *mut c_void, flags: c_int) -> c_int;
    fn pam_acct_mgmt(handle: *mut c_void, flags: c_int) -> c_int;
    fn pam_end(handle: *mut c_void, status: c_int) -> c_int;
    fn pam_strerror(handle: *mut c_void, errnum: c_int) -> *const c_char;
}

/// What the PAM stack sees when it asks something. Borrowed for the duration
/// of one synchronous `pam_authenticate` call, so the pointer stays valid.
struct Cred {
    user: String,
    password: String,
}

unsafe extern "C" fn converse(
    num_msg: c_int,
    msg: *mut *const PamMessage,
    resp: *mut *mut PamResponse,
    appdata: *mut c_void,
) -> c_int {
    if num_msg <= 0 || msg.is_null() || resp.is_null() || appdata.is_null() {
        return PAM_CONV_ERR;
    }
    // SAFETY: PAM hands back the `appdata` pointer passed in `authenticate`,
    // which outlives this call, and an array of `num_msg` messages.
    let cred = unsafe { &*(appdata as *const Cred) };
    // A NUL in a credential cannot be sent over C strings; refuse the
    // conversation rather than truncating the password.
    let Ok(user) = CString::new(cred.user.as_str()) else {
        return PAM_CONV_ERR;
    };
    let Ok(password) = CString::new(cred.password.as_str()) else {
        return PAM_CONV_ERR;
    };
    // SAFETY: calloc zeroes the block, which is what PAM expects for an
    // unanswered slot; PAM frees every `resp` and the array itself.
    let arr = unsafe { libc::calloc(num_msg as usize, std::mem::size_of::<PamResponse>()) }
        as *mut PamResponse;
    if arr.is_null() {
        return PAM_CONV_ERR;
    }
    for i in 0..num_msg as isize {
        // SAFETY: i < num_msg, so both pointers are in range.
        let style = unsafe { (**msg.offset(i)).msg_style };
        let slot = unsafe { &mut *arr.offset(i) };
        slot.resp_retcode = 0;
        slot.resp = match style {
            // ECHO_OFF is the password prompt (or a token/code); ECHO_ON is a
            // username prompt, which only happens when pam_start was given a
            // name PAM could not use.
            PAM_PROMPT_ECHO_OFF => unsafe { libc::strdup(password.as_ptr()) },
            PAM_PROMPT_ECHO_ON => unsafe { libc::strdup(user.as_ptr()) },
            // PAM_ERROR_MSG / PAM_TEXT_INFO carry no answer, and a null
            // response is how the conversation says it read them.
            PAM_ERROR_MSG | PAM_TEXT_INFO => std::ptr::null_mut(),
            _ => std::ptr::null_mut(),
        };
    }
    unsafe { *resp = arr };
    PAM_SUCCESS
}

/// Verify `user`/`password` against the host's PAM.
///
/// A wrong password and an unavailable account give different messages, so the
/// UI can say which happened. The wording is PAM's own.
pub fn authenticate(user: &str, password: &str) -> Result<(), String> {
    let service = if Path::new(&format!("/etc/pam.d/{SERVICE}")).exists() {
        SERVICE
    } else {
        "login"
    };
    authenticate_with(service, user, password)
}

fn authenticate_with(service: &str, user: &str, password: &str) -> Result<(), String> {
    let service_c = CString::new(service).map_err(|_| "invalid PAM service name".to_string())?;
    let user_c = CString::new(user).map_err(|_| "invalid username".to_string())?;
    let cred = Cred {
        user: user.to_string(),
        password: password.to_string(),
    };
    let conv = PamConv {
        conv: Some(converse),
        appdata: &cred as *const Cred as *mut c_void,
    };
    let mut handle: *mut c_void = std::ptr::null_mut();
    // SAFETY: the service, user and conversation all outlive every call below;
    // PAM writes only through the out-pointers we give it.
    let rc = unsafe { pam_start(service_c.as_ptr(), user_c.as_ptr(), &conv, &mut handle) };
    if rc != PAM_SUCCESS {
        // No usable handle to ask for the reason; the status is all there is.
        return Err(format!("PAM is not configured ({})", strerr(handle, rc)));
    }
    let auth = unsafe { pam_authenticate(handle, 0) };
    if auth != PAM_SUCCESS {
        let msg = strerr(handle, auth);
        unsafe { pam_end(handle, auth) };
        return Err(format!("authentication failed: {msg}"));
    }
    let acct = unsafe { pam_acct_mgmt(handle, 0) };
    let outcome = if acct == PAM_SUCCESS {
        Ok(())
    } else {
        Err(format!("this account cannot log in: {}", strerr(handle, acct)))
    };
    unsafe { pam_end(handle, acct) };
    outcome
}

fn strerr(handle: *mut c_void, rc: c_int) -> String {
    // SAFETY: pam_strerror takes any handle, including null.
    let p = unsafe { pam_strerror(handle, rc) };
    if p.is_null() {
        format!("PAM error {rc}")
    } else {
        unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The conversation is the one piece of real logic here — the rest is
    /// straight pass-through. It must answer a password prompt with the
    /// password and allocate exactly what PAM frees.
    #[test]
    fn a_password_prompt_is_answered_and_the_reply_is_owned_by_pam() {
        let cred = Cred {
            user: "alice".into(),
            password: "s3cret".into(),
        };
        let msg = PamMessage {
            msg_style: PAM_PROMPT_ECHO_OFF,
            msg: std::ptr::null(),
        };
        let mut list: [*const PamMessage; 1] = [&msg];
        let mut resp: *mut PamResponse = std::ptr::null_mut();
        let rc = unsafe {
            converse(
                1,
                list.as_mut_ptr(),
                &mut resp,
                &cred as *const Cred as *mut c_void,
            )
        };
        assert_eq!(rc, PAM_SUCCESS);
        assert!(!resp.is_null());
        let answer = unsafe { CStr::from_ptr((*resp).resp) }
            .to_string_lossy()
            .into_owned();
        assert_eq!(answer, "s3cret");
        // Use-after-free would be invisible in a leak; free the way PAM does.
        unsafe {
            libc::free((*resp).resp as *mut c_void);
            libc::free(resp as *mut c_void);
        }
    }

    #[test]
    fn an_informational_message_gets_no_reply() {
        let cred = Cred {
            user: "alice".into(),
            password: "x".into(),
        };
        let msg = PamMessage {
            msg_style: PAM_TEXT_INFO,
            msg: std::ptr::null(),
        };
        let mut list: [*const PamMessage; 1] = [&msg];
        let mut resp: *mut PamResponse = std::ptr::null_mut();
        let rc = unsafe {
            converse(
                1,
                list.as_mut_ptr(),
                &mut resp,
                &cred as *const Cred as *mut c_void,
            )
        };
        assert_eq!(rc, PAM_SUCCESS);
        assert!(unsafe { (*resp).resp.is_null() }, "no answer for text/info");
        unsafe { libc::free(resp as *mut c_void) };
    }

    /// A credential with a NUL cannot be spoken over C strings; the
    /// conversation must refuse rather than authenticate with a truncated one.
    #[test]
    fn a_password_with_a_nul_is_refused() {
        let cred = Cred {
            user: "alice".into(),
            password: "a\0b".into(),
        };
        let msg = PamMessage {
            msg_style: PAM_PROMPT_ECHO_OFF,
            msg: std::ptr::null(),
        };
        let mut list: [*const PamMessage; 1] = [&msg];
        let mut resp: *mut PamResponse = std::ptr::null_mut();
        let rc = unsafe {
            converse(
                1,
                list.as_mut_ptr(),
                &mut resp,
                &cred as *const Cred as *mut c_void,
            )
        };
        assert_eq!(rc, PAM_CONV_ERR);
    }
}
