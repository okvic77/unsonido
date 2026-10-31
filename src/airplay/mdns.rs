//! Service advertisement through the system DNS-SD daemon: mDNSResponder on macOS (part of
//! libSystem), and Avahi's libdns_sd compatibility library on Linux.

use std::ffi::{CString, c_char, c_void};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};

#[repr(C)]
struct DnsServiceOpaque {
    _private: [u8; 0],
}
type DnsServiceRef = *mut DnsServiceOpaque;

#[cfg_attr(target_os = "linux", link(name = "dns_sd"))]
unsafe extern "C" {
    fn DNSServiceRegister(
        sd_ref: *mut DnsServiceRef,
        flags: u32,
        interface_index: u32,
        name: *const c_char,
        regtype: *const c_char,
        domain: *const c_char,
        host: *const c_char,
        port: u16,
        txt_len: u16,
        txt_record: *const c_void,
        callback: *const c_void,
        context: *mut c_void,
    ) -> i32;
    fn DNSServiceRefDeallocate(sd_ref: DnsServiceRef);
    fn DNSServiceResolve(
        sd_ref: *mut DnsServiceRef,
        flags: u32,
        interface_index: u32,
        name: *const c_char,
        regtype: *const c_char,
        domain: *const c_char,
        callback: ResolveReply,
        context: *mut c_void,
    ) -> i32;
    fn DNSServiceRefSockFD(sd_ref: DnsServiceRef) -> i32;
    fn DNSServiceProcessResult(sd_ref: DnsServiceRef) -> i32;
}

type ResolveReply = extern "C" fn(
    sd_ref: DnsServiceRef,
    flags: u32,
    interface_index: u32,
    error: i32,
    fullname: *const c_char,
    host_target: *const c_char,
    port: u16,
    txt_len: u16,
    txt_record: *const u8,
    context: *mut c_void,
);

extern "C" fn on_resolve(
    _sd_ref: DnsServiceRef,
    _flags: u32,
    _interface_index: u32,
    error: i32,
    _fullname: *const c_char,
    _host_target: *const c_char,
    port: u16,
    _txt_len: u16,
    _txt_record: *const u8,
    context: *mut c_void,
) {
    if error == 0 {
        // SAFETY: `context` points at the `Option<u16>` owned by `resolve_port`, which outlives
        // the resolve handle.
        unsafe { *(context as *mut Option<u16>) = Some(u16::from_be(port)) };
    }
}

/// Resolve a service instance in `local.` to its port, waiting up to `timeout`.
pub fn resolve_port(name: &str, service_type: &str, timeout: Duration) -> Result<u16> {
    let name_c = CString::new(name)?;
    let type_c = CString::new(service_type)?;
    let domain = CString::new("local.")?;
    // The callback writes here; it is only ever accessed through this raw pointer.
    let slot: *mut Option<u16> = Box::into_raw(Box::new(None));
    let result = resolve_into(&name_c, &type_c, &domain, slot, timeout);
    // SAFETY: `resolve_into` has freed its handle, so the callback can no longer run.
    let port = unsafe { *Box::from_raw(slot) };
    result?;
    port.ok_or_else(|| anyhow!("could not resolve {name}.{service_type}"))
}

fn resolve_into(
    name: &CString,
    service_type: &CString,
    domain: &CString,
    slot: *mut Option<u16>,
    timeout: Duration,
) -> Result<()> {
    let mut handle: DnsServiceRef = std::ptr::null_mut();
    // SAFETY: valid strings and out-pointer; `slot` outlives the handle created here.
    let err = unsafe {
        DNSServiceResolve(
            &mut handle,
            0,
            0,
            name.as_ptr(),
            service_type.as_ptr(),
            domain.as_ptr(),
            on_resolve,
            slot.cast(),
        )
    };
    if err != 0 || handle.is_null() {
        return Err(anyhow!("DNSServiceResolve failed with error {err}"));
    }
    let handle = Registration(handle);
    // SAFETY: the handle is valid until `handle` is dropped at the end of this function.
    let fd = unsafe { DNSServiceRefSockFD(handle.0) };
    let deadline = Instant::now() + timeout;
    // SAFETY: only this thread touches `slot`, and the callback runs inside ProcessResult below.
    while unsafe { (*slot).is_none() } {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        let mut poll = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one valid pollfd.
        let ready =
            unsafe { libc::poll(&mut poll, 1, remaining.as_millis().clamp(1, 1000) as i32) };
        if ready > 0 {
            // SAFETY: data is ready on the handle's socket; this runs the callback.
            unsafe { DNSServiceProcessResult(handle.0) };
        }
    }
    Ok(())
}

/// A registered service; unregistered when dropped.
pub struct Registration(DnsServiceRef);

// SAFETY: the handle is only used again to deallocate it, which the API allows from any thread.
unsafe impl Send for Registration {}
unsafe impl Sync for Registration {}

impl Drop for Registration {
    fn drop(&mut self) {
        // SAFETY: the handle came from a successful DNSServiceRegister and is freed only here.
        unsafe { DNSServiceRefDeallocate(self.0) };
    }
}

pub fn register(name: &str, service_type: &str, port: u16, txt: &[String]) -> Result<Registration> {
    let mut record = Vec::new();
    for entry in txt {
        let len = u8::try_from(entry.len()).map_err(|_| anyhow!("TXT entry too long: {entry}"))?;
        record.push(len);
        record.extend_from_slice(entry.as_bytes());
    }
    let name = CString::new(name)?;
    let service_type = CString::new(service_type)?;
    let mut handle: DnsServiceRef = std::ptr::null_mut();
    // SAFETY: all pointers are valid for the duration of the call; a null callback is allowed
    // and means registration errors are not reported asynchronously.
    let err = unsafe {
        DNSServiceRegister(
            &mut handle,
            0,
            0,
            name.as_ptr(),
            service_type.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            port.to_be(),
            record.len() as u16,
            record.as_ptr().cast(),
            std::ptr::null(),
            std::ptr::null_mut(),
        )
    };
    if err != 0 || handle.is_null() {
        return Err(anyhow!("DNSServiceRegister failed with error {err}"));
    }
    Ok(Registration(handle))
}
