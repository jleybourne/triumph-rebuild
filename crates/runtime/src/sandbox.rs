//! Network containment for the offline online-mode sandbox.
//!
//! When `REBUILD_OFFLINE_SANDBOX` is set, the GDK stubs report an online-capable console so the title
//! enters its online / content-loading code (`rebuild_gdk::sandbox`). That is only allowed to happen
//! because this module makes it **impossible for the title to reach anything off the machine**: every
//! outbound socket and HTTP call the title makes is routed through a wrapper here that refuses any
//! destination that is not loopback (and, with an explicit opt-in, the private LAN). The refusal is in
//! code and unconditional while the sandbox is on — it is not a setting the title can turn off.
//!
//! What is intercepted (bound as `OVERRIDES` in `lib.rs`, so the name alone routes here whichever DLL the
//! title imports it from):
//!
//! ```text
//! ws2_32   connect, WSAConnect   non-loopback peer -> fail WSAENETUNREACH (10051), counted
//!          sendto, WSASendTo     non-loopback peer -> fail WSAENETUNREACH, counted (UDP multiplayer)
//!          getaddrinfo,          a non-loopback host -> WSAHOST_NOT_FOUND (11001); public names are
//!          GetAddrInfoW          never resolved
//! winhttp  WinHttpConnect        a non-loopback host -> fail (NULL)
//!          WinHttpSendRequest    fail closed: the connect handle's destination can't be re-checked here,
//!                                so in sandbox mode every send is refused
//! ```
//!
//! An allowed (loopback / opted-in LAN) call is forwarded to the host's own function unchanged, so a
//! local test server still works. Every refusal is counted; the first few are logged, and the run report
//! prints the totals by kind.
//!
//! The decision — [`ip_allowed_with`], [`sockaddr_ip`], [`host_allowed_with`] — is pure and unit-tested
//! directly, without real sockets.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

/// Whether the sandbox is on (`REBUILD_OFFLINE_SANDBOX`). Read once: the flag is fixed for the run.
pub fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("REBUILD_OFFLINE_SANDBOX").is_some())
}

/// Whether the private LAN is allowed out too (`REBUILD_OFFLINE_SANDBOX_LAN`, opt-in). Default: loopback
/// only. Public addresses are never allowed, whatever this is.
pub fn allow_lan() -> bool {
    static LAN: OnceLock<bool> = OnceLock::new();
    *LAN.get_or_init(|| std::env::var_os("REBUILD_OFFLINE_SANDBOX_LAN").is_some())
}

/// Whether a destination IP may be reached: loopback always; private / link-local only with `lan`; every
/// public address refused. IPv4-mapped IPv6 is judged as the IPv4 address it carries.
pub fn ip_allowed_with(ip: IpAddr, lan: bool) -> bool {
    match ip {
        IpAddr::V4(a) => a.is_loopback() || (lan && (a.is_private() || a.is_link_local())),
        IpAddr::V6(a) => {
            if let Some(v4) = a.to_ipv4_mapped() {
                return ip_allowed_with(IpAddr::V4(v4), lan);
            }
            let seg0 = a.segments()[0];
            // fe80::/10 link-local, fc00::/7 unique-local
            a.is_loopback() || (lan && ((seg0 & 0xffc0) == 0xfe80 || (seg0 & 0xfe00) == 0xfc00))
        }
    }
}

/// The current rule for an IP (using [`allow_lan`]).
pub fn ip_allowed(ip: IpAddr) -> bool {
    ip_allowed_with(ip, allow_lan())
}

/// The IP in a `SOCKADDR` (`AF_INET` = 2, `AF_INET6` = 23). `None` for any other / truncated address —
/// treated as not-allowed by the callers, since it cannot be proven to be loopback.
pub fn sockaddr_ip(bytes: &[u8]) -> Option<IpAddr> {
    if bytes.len() < 2 {
        return None;
    }
    match u16::from_ne_bytes([bytes[0], bytes[1]]) {
        2 if bytes.len() >= 8 => Some(IpAddr::V4(Ipv4Addr::new(bytes[4], bytes[5], bytes[6], bytes[7]))),
        23 if bytes.len() >= 24 => {
            let mut o = [0u8; 16];
            o.copy_from_slice(&bytes[8..24]);
            Some(IpAddr::V6(Ipv6Addr::from(o)))
        }
        _ => None,
    }
}

/// Whether a host name may be resolved / connected to: `localhost` and loopback (or opted-in LAN) IP
/// literals only. A name we would have to resolve on the network is refused (we do not resolve public
/// names). Bracketed IPv6 literals (`[::1]`) are accepted.
pub fn host_allowed_with(name: &str, lan: bool) -> bool {
    let n = name.trim();
    let n = n.strip_prefix('[').and_then(|s| s.strip_suffix(']')).unwrap_or(n);
    if n.eq_ignore_ascii_case("localhost") {
        return true;
    }
    match n.parse::<IpAddr>() {
        Ok(ip) => ip_allowed_with(ip, lan),
        Err(_) => false,
    }
}

pub fn host_allowed(name: &str) -> bool {
    host_allowed_with(name, allow_lan())
}

// --------------------------------------------------------------------------------- counters

#[derive(Clone, Copy)]
pub enum Kind {
    Connect,
    Send,
    Resolve,
    WinHttp,
}

static BLOCKED_CONNECT: AtomicU64 = AtomicU64::new(0);
static BLOCKED_SEND: AtomicU64 = AtomicU64::new(0);
static BLOCKED_RESOLVE: AtomicU64 = AtomicU64::new(0);
static BLOCKED_WINHTTP: AtomicU64 = AtomicU64::new(0);

fn counter(kind: Kind) -> &'static AtomicU64 {
    match kind {
        Kind::Connect => &BLOCKED_CONNECT,
        Kind::Send => &BLOCKED_SEND,
        Kind::Resolve => &BLOCKED_RESOLVE,
        Kind::WinHttp => &BLOCKED_WINHTTP,
    }
}

fn kind_name(kind: Kind) -> &'static str {
    match kind {
        Kind::Connect => "connect",
        Kind::Send => "sendto",
        Kind::Resolve => "resolve",
        Kind::WinHttp => "winhttp",
    }
}

/// Count a refused call, and log the first few so a run shows what the title reached for.
fn record(kind: Kind, dest: &str) {
    let n = counter(kind).fetch_add(1, Ordering::Relaxed);
    if n < 8 {
        let line = format!("SANDBOX blocked {} to {dest}", kind_name(kind));
        eprintln!("{line}");
        rebuild_gdk::discovery::note(line);
    }
}

/// The counts so far: (connect, sendto, resolve, winhttp).
pub fn counts() -> (u64, u64, u64, u64) {
    (
        BLOCKED_CONNECT.load(Ordering::Relaxed),
        BLOCKED_SEND.load(Ordering::Relaxed),
        BLOCKED_RESOLVE.load(Ordering::Relaxed),
        BLOCKED_WINHTTP.load(Ordering::Relaxed),
    )
}

/// The run-report line, when the sandbox is on.
pub fn blocked_report() -> Option<String> {
    if !on() {
        return None;
    }
    let (c, s, r, h) = counts();
    Some(format!(
        "sandbox   outbound connections blocked: {} connect, {} sendto, {} resolve, {} winhttp ({} total)",
        c, s, r, h, c + s + r + h
    ))
}

/// The loud startup line, printed when the sandbox is on.
pub fn announce() {
    if !on() {
        return;
    }
    println!("SANDBOX offline online-mode on: all non-loopback network is blocked");
    if allow_lan() {
        println!("SANDBOX private LAN explicitly allowed out (REBUILD_OFFLINE_SANDBOX_LAN); public addresses still blocked");
    }
}

// ------------------------------------------------------------- the intercepts (Windows only)

#[cfg(windows)]
pub use win::override_export;

#[cfg(windows)]
// the console / Win32 function names, so the wrapper reads the same as the import it stands in for
#[allow(non_snake_case)]
mod win {
    use super::*;
    use std::ffi::{c_void, CString};
    use windows::core::{PCSTR, PCWSTR};
    use windows::Win32::Foundation::{SetLastError, WIN32_ERROR};
    use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_SYSTEM32};

    const SOCKET_ERROR: i32 = -1;
    const WSAENETUNREACH: u32 = 10051;
    const WSAHOST_NOT_FOUND: u32 = 11001;
    const ERROR_WINHTTP_CANNOT_CONNECT: u32 = 12029;

    type ConnectFn = unsafe extern "system" fn(usize, *const u8, i32) -> i32;
    type WsaConnectFn = unsafe extern "system" fn(usize, *const u8, i32, *const c_void, *mut c_void, *const c_void, *const c_void) -> i32;
    type SendToFn = unsafe extern "system" fn(usize, *const u8, i32, i32, *const u8, i32) -> i32;
    type WsaSendToFn = unsafe extern "system" fn(usize, *const c_void, u32, *mut u32, u32, *const u8, i32, *mut c_void, *const c_void) -> i32;
    type GetAddrInfoFn = unsafe extern "system" fn(*const u8, *const u8, *const c_void, *mut *mut c_void) -> i32;
    type GetAddrInfoWFn = unsafe extern "system" fn(*const u16, *const u16, *const c_void, *mut *mut c_void) -> i32;
    type WinHttpConnectFn = unsafe extern "system" fn(*mut c_void, *const u16, u16, u32) -> *mut c_void;

    /// A host export from System32, by DLL and name.
    fn host(dll: &str, name: &str) -> Option<usize> {
        let w: Vec<u16> = dll.encode_utf16().chain(std::iter::once(0)).collect();
        let m = unsafe { LoadLibraryExW(PCWSTR(w.as_ptr()), None, LOAD_LIBRARY_SEARCH_SYSTEM32) }.ok()?;
        let c = CString::new(name).ok()?;
        unsafe { GetProcAddress(m, PCSTR(c.as_ptr() as *const u8)) }.map(|f| f as usize)
    }

    fn fail_net() -> i32 {
        unsafe { SetLastError(WIN32_ERROR(WSAENETUNREACH)) };
        SOCKET_ERROR
    }

    /// The decision for a `SOCKADDR`: `Ok` to forward, `Err(reason)` to refuse (with the reason to log).
    fn sockaddr_decision(name: *const u8, namelen: i32) -> Result<(), String> {
        if name.is_null() || namelen <= 0 {
            return Err("null address".into());
        }
        let slice = unsafe { std::slice::from_raw_parts(name, namelen as usize) };
        match sockaddr_ip(slice) {
            Some(ip) if ip_allowed(ip) => Ok(()),
            Some(ip) => Err(ip.to_string()),
            None => Err("unknown address family".into()),
        }
    }

    unsafe fn cstr(p: *const u8) -> Option<String> {
        if p.is_null() {
            return None;
        }
        Some(unsafe { std::ffi::CStr::from_ptr(p as *const i8) }.to_string_lossy().into_owned())
    }

    unsafe fn wstr(p: *const u16) -> Option<String> {
        if p.is_null() {
            return None;
        }
        let mut len = 0usize;
        while unsafe { *p.add(len) } != 0 {
            len += 1;
        }
        Some(String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(p, len) }))
    }

    unsafe extern "system" fn connect(s: usize, name: *const u8, namelen: i32) -> i32 {
        if let Err(dest) = sockaddr_decision(name, namelen) {
            record(Kind::Connect, &dest);
            return fail_net();
        }
        match host("ws2_32.dll", "connect") {
            Some(f) => unsafe { std::mem::transmute::<usize, ConnectFn>(f)(s, name, namelen) },
            None => fail_net(),
        }
    }

    unsafe extern "system" fn WSAConnect(
        s: usize,
        name: *const u8,
        namelen: i32,
        caller: *const c_void,
        callee: *mut c_void,
        sqos: *const c_void,
        gqos: *const c_void,
    ) -> i32 {
        if let Err(dest) = sockaddr_decision(name, namelen) {
            record(Kind::Connect, &dest);
            return fail_net();
        }
        match host("ws2_32.dll", "WSAConnect") {
            Some(f) => unsafe { std::mem::transmute::<usize, WsaConnectFn>(f)(s, name, namelen, caller, callee, sqos, gqos) },
            None => fail_net(),
        }
    }

    unsafe extern "system" fn sendto(s: usize, buf: *const u8, len: i32, flags: i32, to: *const u8, tolen: i32) -> i32 {
        // A connected socket (no address) was already gated at connect; a datagram with an address is
        // checked here (UDP multiplayer discovery goes this way).
        if !to.is_null() && tolen > 0 {
            if let Err(dest) = sockaddr_decision(to, tolen) {
                record(Kind::Send, &dest);
                return fail_net();
            }
        }
        match host("ws2_32.dll", "sendto") {
            Some(f) => unsafe { std::mem::transmute::<usize, SendToFn>(f)(s, buf, len, flags, to, tolen) },
            None => fail_net(),
        }
    }

    #[allow(clippy::too_many_arguments)]
    unsafe extern "system" fn WSASendTo(
        s: usize,
        buffers: *const c_void,
        count: u32,
        sent: *mut u32,
        flags: u32,
        to: *const u8,
        tolen: i32,
        overlapped: *mut c_void,
        completion: *const c_void,
    ) -> i32 {
        if !to.is_null() && tolen > 0 {
            if let Err(dest) = sockaddr_decision(to, tolen) {
                record(Kind::Send, &dest);
                return fail_net();
            }
        }
        match host("ws2_32.dll", "WSASendTo") {
            Some(f) => unsafe { std::mem::transmute::<usize, WsaSendToFn>(f)(s, buffers, count, sent, flags, to, tolen, overlapped, completion) },
            None => fail_net(),
        }
    }

    unsafe extern "system" fn getaddrinfo(node: *const u8, service: *const u8, hints: *const c_void, res: *mut *mut c_void) -> i32 {
        let name = unsafe { cstr(node) };
        let allowed = name.as_deref().map(host_allowed).unwrap_or(true); // null node = local/passive
        if !allowed {
            record(Kind::Resolve, name.as_deref().unwrap_or("?"));
            if !res.is_null() {
                unsafe { *res = std::ptr::null_mut() };
            }
            unsafe { SetLastError(WIN32_ERROR(WSAHOST_NOT_FOUND)) };
            return WSAHOST_NOT_FOUND as i32;
        }
        match host("ws2_32.dll", "getaddrinfo") {
            Some(f) => unsafe { std::mem::transmute::<usize, GetAddrInfoFn>(f)(node, service, hints, res) },
            None => {
                unsafe { SetLastError(WIN32_ERROR(WSAHOST_NOT_FOUND)) };
                WSAHOST_NOT_FOUND as i32
            }
        }
    }

    unsafe extern "system" fn GetAddrInfoW(node: *const u16, service: *const u16, hints: *const c_void, res: *mut *mut c_void) -> i32 {
        let name = unsafe { wstr(node) };
        let allowed = name.as_deref().map(host_allowed).unwrap_or(true);
        if !allowed {
            record(Kind::Resolve, name.as_deref().unwrap_or("?"));
            if !res.is_null() {
                unsafe { *res = std::ptr::null_mut() };
            }
            unsafe { SetLastError(WIN32_ERROR(WSAHOST_NOT_FOUND)) };
            return WSAHOST_NOT_FOUND as i32;
        }
        match host("ws2_32.dll", "GetAddrInfoW") {
            Some(f) => unsafe { std::mem::transmute::<usize, GetAddrInfoWFn>(f)(node, service, hints, res) },
            None => {
                unsafe { SetLastError(WIN32_ERROR(WSAHOST_NOT_FOUND)) };
                WSAHOST_NOT_FOUND as i32
            }
        }
    }

    unsafe extern "system" fn WinHttpConnect(session: *mut c_void, server: *const u16, port: u16, reserved: u32) -> *mut c_void {
        let name = unsafe { wstr(server) };
        if !name.as_deref().map(host_allowed).unwrap_or(false) {
            record(Kind::WinHttp, name.as_deref().unwrap_or("?"));
            unsafe { SetLastError(WIN32_ERROR(ERROR_WINHTTP_CANNOT_CONNECT)) };
            return std::ptr::null_mut();
        }
        match host("winhttp.dll", "WinHttpConnect") {
            Some(f) => unsafe { std::mem::transmute::<usize, WinHttpConnectFn>(f)(session, server, port, reserved) },
            None => {
                unsafe { SetLastError(WIN32_ERROR(ERROR_WINHTTP_CANNOT_CONNECT)) };
                std::ptr::null_mut()
            }
        }
    }

    /// `WinHttpSendRequest` fails closed: the request handle's destination can't be re-checked from here,
    /// so in sandbox mode no request is ever sent. Returns FALSE.
    unsafe extern "system" fn WinHttpSendRequest(
        _request: *mut c_void,
        _headers: *const u16,
        _headers_len: u32,
        _optional: *const c_void,
        _optional_len: u32,
        _total_len: u32,
        _context: usize,
    ) -> i32 {
        record(Kind::WinHttp, "WinHttpSendRequest (fail closed)");
        unsafe { SetLastError(WIN32_ERROR(ERROR_WINHTTP_CANNOT_CONNECT)) };
        0 // FALSE
    }

    /// Our wrapper for a network function, by name — but only while the sandbox is on. Off, this returns
    /// `None`, so the resolver binds the host's own function and nothing changes.
    pub fn override_export(name: &str) -> Option<u64> {
        if !on() {
            return None;
        }
        let f: *const () = match name {
            "connect" => connect as *const (),
            "WSAConnect" => WSAConnect as *const (),
            "sendto" => sendto as *const (),
            "WSASendTo" => WSASendTo as *const (),
            "getaddrinfo" => getaddrinfo as *const (),
            "GetAddrInfoW" => GetAddrInfoW as *const (),
            "WinHttpConnect" => WinHttpConnect as *const (),
            "WinHttpSendRequest" => WinHttpSendRequest as *const (),
            _ => return None,
        };
        Some(f as usize as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v4(a: u8, b: u8, c: u8, d: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(a, b, c, d))
    }

    #[test]
    fn loopback_is_allowed_and_public_is_refused() {
        // loopback, always
        assert!(ip_allowed_with(v4(127, 0, 0, 1), false));
        assert!(ip_allowed_with(v4(127, 5, 5, 5), false));
        assert!(ip_allowed_with(IpAddr::V6(Ipv6Addr::LOCALHOST), false));
        // public, never — not even with LAN opened
        assert!(!ip_allowed_with(v4(93, 184, 216, 34), false));
        assert!(!ip_allowed_with(v4(93, 184, 216, 34), true));
        assert!(!ip_allowed_with(v4(8, 8, 8, 8), true));
        assert!(!ip_allowed_with("2606:2800:220:1::1".parse().unwrap(), true));
        // private LAN: off by default, on only with the opt-in
        assert!(!ip_allowed_with(v4(192, 168, 1, 10), false));
        assert!(ip_allowed_with(v4(192, 168, 1, 10), true));
        assert!(!ip_allowed_with(v4(10, 0, 0, 5), false));
        assert!(ip_allowed_with(v4(10, 0, 0, 5), true));
        assert!(ip_allowed_with(v4(169, 254, 1, 1), true) && !ip_allowed_with(v4(169, 254, 1, 1), false));
        // an IPv4-mapped public address is still public
        assert!(!ip_allowed_with("::ffff:93.184.216.34".parse().unwrap(), true));
        assert!(ip_allowed_with("::ffff:127.0.0.1".parse().unwrap(), false));
    }

    #[test]
    fn a_public_connect_is_refused_and_a_loopback_connect_is_allowed() {
        // AF_INET sockaddr_in for 93.184.216.34:443 — a public address: refused.
        let mut public = [0u8; 16];
        public[0..2].copy_from_slice(&2u16.to_ne_bytes()); // AF_INET
        public[2..4].copy_from_slice(&443u16.to_be_bytes());
        public[4..8].copy_from_slice(&[93, 184, 216, 34]);
        assert_eq!(sockaddr_ip(&public), Some(v4(93, 184, 216, 34)));
        assert!(!ip_allowed_with(sockaddr_ip(&public).unwrap(), true), "public connect refused");

        // AF_INET sockaddr_in for 127.0.0.1:3074 — loopback: allowed through.
        let mut loop4 = [0u8; 16];
        loop4[0..2].copy_from_slice(&2u16.to_ne_bytes());
        loop4[2..4].copy_from_slice(&3074u16.to_be_bytes());
        loop4[4..8].copy_from_slice(&[127, 0, 0, 1]);
        assert!(ip_allowed_with(sockaddr_ip(&loop4).unwrap(), false), "loopback connect allowed");

        // an unparseable / foreign-family address can't be proven loopback: not allowed.
        assert_eq!(sockaddr_ip(&[1, 0, 0, 0]), None);
        assert_eq!(sockaddr_ip(&[]), None);
    }

    #[test]
    fn host_names_only_resolve_when_local() {
        assert!(host_allowed_with("localhost", false));
        assert!(host_allowed_with("LocalHost", false));
        assert!(host_allowed_with("127.0.0.1", false));
        assert!(host_allowed_with("[::1]", false));
        assert!(!host_allowed_with("xboxlive.com", false));
        assert!(!host_allowed_with("title.example.com", true), "a public name is never resolved");
        assert!(!host_allowed_with("93.184.216.34", true));
        assert!(host_allowed_with("192.168.0.2", true) && !host_allowed_with("192.168.0.2", false));
    }
}
