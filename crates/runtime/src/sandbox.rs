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
//! ws2_32   connect, WSAConnect,  non-loopback peer -> fail WSAENETUNREACH (10051), counted
//!          WSAConnectByName{W,A},  (ByName/ByList resolve+connect; refused unless the name / every
//!          WSAConnectByList        candidate address is local)
//!          sendto, WSASendTo,    non-loopback peer -> fail WSAENETUNREACH, counted (UDP multiplayer)
//!          WSASendMsg
//!          WSAIoctl              SIO_GET_EXTENSION_FUNCTION_POINTER for ConnectEx / WSASendMsg returns
//!                                OUR wrapper (same decision); every other IOCTL is the host's, untouched
//!          getaddrinfo,          a non-loopback host -> WSAHOST_NOT_FOUND (11001); public names are
//!          GetAddrInfoW,         never resolved
//!          GetAddrInfoExW,
//!          gethostbyname,
//!          WSAAsyncGetHostByName
//! winhttp  WinHttpConnect        a non-loopback host -> fail (NULL)
//!          WinHttpSendRequest    fail closed: the connect handle's destination can't be re-checked here,
//!                                so in sandbox mode every send is refused
//! wininet  InternetConnect{W,A}  a non-loopback host -> fail (NULL)
//!          InternetOpenUrl{W,A}  a non-loopback URL host -> fail (NULL)
//!          HttpSendRequest{W,A}, fail closed (like WinHttpSendRequest)
//!          HttpSendRequestEx{W,A}
//! ```
//!
//! Each HTTP library (WinHTTP, WinINet) runs its own sockets inside a *host* DLL, which the socket
//! overrides above do not reach (they rebind only the title's / its own DLLs' imports) — so the HTTP
//! libraries are gated at their own entry points. The title's own HTTP transport (XCurl, a title module)
//! uses `ws2_32` sockets, which the socket overrides *do* rebind, so it is contained there.
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

/// The host of a URL (scheme stripped, userinfo and `:port` and path removed; an `[ipv6]` literal kept
/// bracketed). Used to gate `InternetOpenUrl`, which takes a whole URL rather than a host.
pub fn url_host(url: &str) -> Option<String> {
    let after = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let authority = after.split(['/', '?', '#']).next().unwrap_or("");
    let hostport = authority.rsplit_once('@').map(|(_, h)| h).unwrap_or(authority);
    let host = if let Some(rest) = hostport.strip_prefix('[') {
        // [ipv6]:port -> [ipv6]
        rest.split(']').next().map(|h| format!("[{h}]")).unwrap_or_else(|| hostport.to_string())
    } else {
        hostport.rsplit_once(':').map(|(h, _)| h).unwrap_or(hostport).to_string()
    };
    (!host.is_empty()).then_some(host)
}

/// Whether a URL's host may be reached (used by `InternetOpenUrl`). A URL with no recognisable host is
/// refused.
pub fn url_allowed(url: &str) -> bool {
    url_host(url).map(|h| host_allowed(&h)).unwrap_or(false)
}

// --------------------------------------------------------------------------------- counters

#[derive(Clone, Copy)]
pub enum Kind {
    Connect,
    Send,
    Resolve,
    WinHttp,
    WinInet,
}

static BLOCKED_CONNECT: AtomicU64 = AtomicU64::new(0);
static BLOCKED_SEND: AtomicU64 = AtomicU64::new(0);
static BLOCKED_RESOLVE: AtomicU64 = AtomicU64::new(0);
static BLOCKED_WINHTTP: AtomicU64 = AtomicU64::new(0);
static BLOCKED_WININET: AtomicU64 = AtomicU64::new(0);

fn counter(kind: Kind) -> &'static AtomicU64 {
    match kind {
        Kind::Connect => &BLOCKED_CONNECT,
        Kind::Send => &BLOCKED_SEND,
        Kind::Resolve => &BLOCKED_RESOLVE,
        Kind::WinHttp => &BLOCKED_WINHTTP,
        Kind::WinInet => &BLOCKED_WININET,
    }
}

fn kind_name(kind: Kind) -> &'static str {
    match kind {
        Kind::Connect => "connect",
        Kind::Send => "sendto",
        Kind::Resolve => "resolve",
        Kind::WinHttp => "winhttp",
        Kind::WinInet => "wininet",
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

/// The counts so far: (connect, sendto, resolve, winhttp, wininet).
pub fn counts() -> (u64, u64, u64, u64, u64) {
    (
        BLOCKED_CONNECT.load(Ordering::Relaxed),
        BLOCKED_SEND.load(Ordering::Relaxed),
        BLOCKED_RESOLVE.load(Ordering::Relaxed),
        BLOCKED_WINHTTP.load(Ordering::Relaxed),
        BLOCKED_WININET.load(Ordering::Relaxed),
    )
}

/// The run-report line, when the sandbox is on.
pub fn blocked_report() -> Option<String> {
    if !on() {
        return None;
    }
    let (c, s, r, h, i) = counts();
    Some(format!(
        "sandbox   outbound connections blocked: {} connect, {} sendto, {} resolve, {} winhttp, {} wininet ({} total)",
        c, s, r, h, i, c + s + r + h + i
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
    type GetAddrInfoExWFn = unsafe extern "system" fn(*const u16, *const u16, u32, *const c_void, *const c_void, *mut *mut c_void, *const c_void, *mut c_void, *const c_void, *mut *mut c_void) -> i32;
    type GethostbynameFn = unsafe extern "system" fn(*const u8) -> *mut c_void;
    type WsaAsyncGetHostByNameFn = unsafe extern "system" fn(*mut c_void, u32, *const u8, *mut u8, i32) -> *mut c_void;
    type WsaIoctlFn = unsafe extern "system" fn(usize, u32, *mut c_void, u32, *mut c_void, u32, *mut u32, *mut c_void, *const c_void) -> i32;
    type ConnectExFn = unsafe extern "system" fn(usize, *const u8, i32, *mut c_void, u32, *mut u32, *mut c_void) -> i32;
    type WsaSendMsgFn = unsafe extern "system" fn(usize, *const u8, u32, *mut u32, *mut c_void, *const c_void) -> i32;
    type ConnectByNameWFn = unsafe extern "system" fn(usize, *const u16, *const u16, *mut u32, *mut u8, *mut u32, *mut u8, *const c_void, *const c_void) -> i32;
    type ConnectByNameAFn = unsafe extern "system" fn(usize, *const u8, *const u8, *mut u32, *mut u8, *mut u32, *mut u8, *const c_void, *const c_void) -> i32;
    type ConnectByListFn = unsafe extern "system" fn(usize, *const u8, *mut u32, *mut u8, *mut u32, *mut u8, *const c_void, *const c_void) -> i32;
    type WinHttpConnectFn = unsafe extern "system" fn(*mut c_void, *const u16, u16, u32) -> *mut c_void;
    type InternetConnectWFn = unsafe extern "system" fn(*mut c_void, *const u16, u16, *const u16, *const u16, u32, u32, usize) -> *mut c_void;
    type InternetConnectAFn = unsafe extern "system" fn(*mut c_void, *const u8, u16, *const u8, *const u8, u32, u32, usize) -> *mut c_void;
    type InternetOpenUrlWFn = unsafe extern "system" fn(*mut c_void, *const u16, *const u16, u32, u32, usize) -> *mut c_void;
    type InternetOpenUrlAFn = unsafe extern "system" fn(*mut c_void, *const u8, *const u8, u32, u32, usize) -> *mut c_void;

    const ERROR_INTERNET_CANNOT_CONNECT: u32 = 12029; // same code space as WinHTTP

    /// `SIO_GET_EXTENSION_FUNCTION_POINTER`, and the mswsock extension-function GUIDs we swap for our own
    /// wrappers when the title asks `WSAIoctl` for them (little-endian GUID bytes, as they sit in the
    /// `WSAIoctl` input buffer). `ConnectEx` and `WSASendMsg` are the two that egress.
    const SIO_GET_EXTENSION_FUNCTION_POINTER: u32 = 0xC800_0006;
    const WSAID_CONNECTEX: [u8; 16] = [0xB9, 0x07, 0xA2, 0x25, 0xF3, 0xDD, 0x60, 0x46, 0x8E, 0xE9, 0x76, 0xE5, 0x8C, 0x74, 0x06, 0x3E];
    const WSAID_WSASENDMSG: [u8; 16] = [0x12, 0xE7, 0x41, 0xA4, 0x4F, 0x75, 0xCA, 0x43, 0x84, 0xA7, 0x0D, 0xEE, 0x44, 0xCF, 0x60, 0x6D];

    /// The real `ConnectEx` behind a title's extension pointer (it is not a `ws2_32` export, so we can
    /// only reach it through the one the host handed back via `WSAIoctl`).
    static REAL_CONNECTEX: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

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

    /// The decision for a `SOCKET_ADDRESS_LIST` (`WSAConnectByList`): forward only if *every* candidate
    /// address is allowed, so the call can never fall through to a public one.
    ///
    /// # Safety
    /// `list` must be null or a valid `SOCKET_ADDRESS_LIST`.
    unsafe fn addr_list_decision(list: *const u8) -> Result<(), String> {
        if list.is_null() {
            return Err("null address list".into());
        }
        let count = unsafe { (list as *const i32).read_unaligned() };
        if count <= 0 {
            return Err("empty address list".into());
        }
        // SOCKET_ADDRESS_LIST: INT iAddressCount; then SOCKET_ADDRESS Address[] aligned to 8. Each
        // SOCKET_ADDRESS is { LPSOCKADDR lpSockaddr; INT iSockaddrLength } = 16 bytes.
        for i in 0..count as usize {
            let entry = unsafe { list.add(8 + i * 16) };
            let sa = unsafe { (entry as *const *const u8).read_unaligned() };
            let len = unsafe { (entry.add(8) as *const i32).read_unaligned() };
            if sa.is_null() || len <= 0 {
                return Err("unknown address family".into());
            }
            let slice = unsafe { std::slice::from_raw_parts(sa, len as usize) };
            match sockaddr_ip(slice) {
                Some(ip) if ip_allowed(ip) => {}
                Some(ip) => return Err(ip.to_string()),
                None => return Err("unknown address family".into()),
            }
        }
        Ok(())
    }

    /// The decision for a `WSAMSG`'s destination (`WSASendMsg`). A connected socket carries no name — then
    /// there is nothing to check here and the connect was already gated.
    ///
    /// # Safety
    /// `msg` must be null or a valid `WSAMSG`.
    unsafe fn wsamsg_decision(msg: *const u8) -> Result<(), String> {
        if msg.is_null() {
            return Ok(());
        }
        // WSAMSG: LPSOCKADDR name (offset 0); INT namelen (offset 8).
        let name = unsafe { (msg as *const *const u8).read_unaligned() };
        let namelen = unsafe { (msg.add(8) as *const i32).read_unaligned() };
        if name.is_null() || namelen <= 0 {
            return Ok(());
        }
        sockaddr_decision(name, namelen)
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

    /// `ConnectEx` (mswsock), reached only through the pointer the title fetched with
    /// `WSAIoctl(SIO_GET_EXTENSION_FUNCTION_POINTER, WSAID_CONNECTEX)`, which we swapped for this. Same
    /// decision as `connect`; allowed calls forward to the real `ConnectEx` the host handed back.
    unsafe extern "system" fn connect_ex(s: usize, name: *const u8, namelen: i32, send: *mut c_void, send_len: u32, sent: *mut u32, overlapped: *mut c_void) -> i32 {
        if let Err(dest) = sockaddr_decision(name, namelen) {
            record(Kind::Connect, &dest);
            unsafe { SetLastError(WIN32_ERROR(WSAENETUNREACH)) };
            return 0; // FALSE
        }
        let real = REAL_CONNECTEX.load(Ordering::Relaxed);
        if real == 0 {
            unsafe { SetLastError(WIN32_ERROR(WSAENETUNREACH)) };
            return 0;
        }
        unsafe { std::mem::transmute::<usize, ConnectExFn>(real)(s, name, namelen, send, send_len, sent, overlapped) }
    }

    /// `WSAIoctl`: forwarded unchanged, except that a successful `SIO_GET_EXTENSION_FUNCTION_POINTER`
    /// lookup for `ConnectEx` / `WSASendMsg` has its returned pointer replaced by our wrapper, so those
    /// egress paths cannot bypass the containment. Every other IOCTL is the host's, untouched.
    unsafe extern "system" fn WSAIoctl(
        s: usize,
        code: u32,
        in_buf: *mut c_void,
        in_len: u32,
        out_buf: *mut c_void,
        out_len: u32,
        returned: *mut u32,
        overlapped: *mut c_void,
        completion: *const c_void,
    ) -> i32 {
        static HOST: OnceLock<Option<usize>> = OnceLock::new();
        let Some(f) = *HOST.get_or_init(|| host("ws2_32.dll", "WSAIoctl")) else { return fail_net() };
        let ret = unsafe { std::mem::transmute::<usize, WsaIoctlFn>(f)(s, code, in_buf, in_len, out_buf, out_len, returned, overlapped, completion) };
        if ret == 0 && code == SIO_GET_EXTENSION_FUNCTION_POINTER && in_len >= 16 && out_len >= 8 && !in_buf.is_null() && !out_buf.is_null() {
            let guid = unsafe { std::slice::from_raw_parts(in_buf as *const u8, 16) };
            if guid == WSAID_CONNECTEX {
                REAL_CONNECTEX.store(unsafe { (out_buf as *const usize).read_unaligned() }, Ordering::Relaxed);
                unsafe { (out_buf as *mut usize).write_unaligned(connect_ex as *const () as usize) };
                rebuild_gdk::discovery::note("SANDBOX wrapped ConnectEx (WSAIoctl extension pointer)".into());
            } else if guid == WSAID_WSASENDMSG {
                unsafe { (out_buf as *mut usize).write_unaligned(WSASendMsg as *const () as usize) };
                rebuild_gdk::discovery::note("SANDBOX wrapped WSASendMsg (WSAIoctl extension pointer)".into());
            }
        }
        ret
    }

    /// `WSASendMsg` (direct export or the swapped extension pointer): a datagram with a destination is
    /// checked; the real one is the `ws2_32` export.
    unsafe extern "system" fn WSASendMsg(s: usize, msg: *const u8, flags: u32, sent: *mut u32, overlapped: *mut c_void, completion: *const c_void) -> i32 {
        if let Err(dest) = unsafe { wsamsg_decision(msg) } {
            record(Kind::Send, &dest);
            return fail_net();
        }
        match host("ws2_32.dll", "WSASendMsg") {
            Some(f) => unsafe { std::mem::transmute::<usize, WsaSendMsgFn>(f)(s, msg, flags, sent, overlapped, completion) },
            None => fail_net(),
        }
    }

    /// `WSAConnectByNameW` resolves a name and connects in one call — so the name is gated like a resolve
    /// and the connect both: only a local name proceeds.
    #[allow(clippy::too_many_arguments)]
    unsafe extern "system" fn WSAConnectByNameW(
        s: usize,
        node: *const u16,
        service: *const u16,
        local_len: *mut u32,
        local: *mut u8,
        remote_len: *mut u32,
        remote: *mut u8,
        timeout: *const c_void,
        reserved: *const c_void,
    ) -> i32 {
        let name = unsafe { wstr(node) };
        if !name.as_deref().map(host_allowed).unwrap_or(false) {
            record(Kind::Connect, name.as_deref().unwrap_or("?"));
            unsafe { SetLastError(WIN32_ERROR(WSAENETUNREACH)) };
            return 0; // FALSE
        }
        match host("ws2_32.dll", "WSAConnectByNameW") {
            Some(f) => unsafe { std::mem::transmute::<usize, ConnectByNameWFn>(f)(s, node, service, local_len, local, remote_len, remote, timeout, reserved) },
            None => {
                unsafe { SetLastError(WIN32_ERROR(WSAENETUNREACH)) };
                0
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    unsafe extern "system" fn WSAConnectByNameA(
        s: usize,
        node: *const u8,
        service: *const u8,
        local_len: *mut u32,
        local: *mut u8,
        remote_len: *mut u32,
        remote: *mut u8,
        timeout: *const c_void,
        reserved: *const c_void,
    ) -> i32 {
        let name = unsafe { cstr(node) };
        if !name.as_deref().map(host_allowed).unwrap_or(false) {
            record(Kind::Connect, name.as_deref().unwrap_or("?"));
            unsafe { SetLastError(WIN32_ERROR(WSAENETUNREACH)) };
            return 0;
        }
        match host("ws2_32.dll", "WSAConnectByNameA") {
            Some(f) => unsafe { std::mem::transmute::<usize, ConnectByNameAFn>(f)(s, node, service, local_len, local, remote_len, remote, timeout, reserved) },
            None => {
                unsafe { SetLastError(WIN32_ERROR(WSAENETUNREACH)) };
                0
            }
        }
    }

    /// `WSAConnectByList`: refused unless every candidate address is allowed.
    #[allow(clippy::too_many_arguments)]
    unsafe extern "system" fn WSAConnectByList(
        s: usize,
        list: *const u8,
        local_len: *mut u32,
        local: *mut u8,
        remote_len: *mut u32,
        remote: *mut u8,
        timeout: *const c_void,
        reserved: *const c_void,
    ) -> i32 {
        if let Err(dest) = unsafe { addr_list_decision(list) } {
            record(Kind::Connect, &dest);
            unsafe { SetLastError(WIN32_ERROR(WSAENETUNREACH)) };
            return 0;
        }
        match host("ws2_32.dll", "WSAConnectByList") {
            Some(f) => unsafe { std::mem::transmute::<usize, ConnectByListFn>(f)(s, list, local_len, local, remote_len, remote, timeout, reserved) },
            None => {
                unsafe { SetLastError(WIN32_ERROR(WSAENETUNREACH)) };
                0
            }
        }
    }

    /// `GetAddrInfoExW` (the extended / async resolver): a non-loopback host is not resolved.
    #[allow(clippy::too_many_arguments)]
    unsafe extern "system" fn GetAddrInfoExW(
        name: *const u16,
        service: *const u16,
        namespace_: u32,
        nspid: *const c_void,
        hints: *const c_void,
        result: *mut *mut c_void,
        timeout: *const c_void,
        overlapped: *mut c_void,
        completion: *const c_void,
        handle: *mut *mut c_void,
    ) -> i32 {
        let n = unsafe { wstr(name) };
        if !n.as_deref().map(host_allowed).unwrap_or(true) {
            record(Kind::Resolve, n.as_deref().unwrap_or("?"));
            if !result.is_null() {
                unsafe { *result = std::ptr::null_mut() };
            }
            unsafe { SetLastError(WIN32_ERROR(WSAHOST_NOT_FOUND)) };
            return WSAHOST_NOT_FOUND as i32;
        }
        match host("ws2_32.dll", "GetAddrInfoExW") {
            Some(f) => unsafe { std::mem::transmute::<usize, GetAddrInfoExWFn>(f)(name, service, namespace_, nspid, hints, result, timeout, overlapped, completion, handle) },
            None => {
                unsafe { SetLastError(WIN32_ERROR(WSAHOST_NOT_FOUND)) };
                WSAHOST_NOT_FOUND as i32
            }
        }
    }

    /// `gethostbyname` (legacy): only a local name resolves; anything else returns NULL.
    unsafe extern "system" fn gethostbyname(name: *const u8) -> *mut c_void {
        let n = unsafe { cstr(name) };
        if !n.as_deref().map(host_allowed).unwrap_or(false) {
            record(Kind::Resolve, n.as_deref().unwrap_or("?"));
            unsafe { SetLastError(WIN32_ERROR(WSAHOST_NOT_FOUND)) };
            return std::ptr::null_mut();
        }
        match host("ws2_32.dll", "gethostbyname") {
            Some(f) => unsafe { std::mem::transmute::<usize, GethostbynameFn>(f)(name) },
            None => {
                unsafe { SetLastError(WIN32_ERROR(WSAHOST_NOT_FOUND)) };
                std::ptr::null_mut()
            }
        }
    }

    /// `WSAAsyncGetHostByName` (legacy async): a non-local name fails at once (NULL task handle).
    unsafe extern "system" fn WSAAsyncGetHostByName(hwnd: *mut c_void, msg: u32, name: *const u8, buf: *mut u8, buflen: i32) -> *mut c_void {
        let n = unsafe { cstr(name) };
        if !n.as_deref().map(host_allowed).unwrap_or(false) {
            record(Kind::Resolve, n.as_deref().unwrap_or("?"));
            unsafe { SetLastError(WIN32_ERROR(WSAHOST_NOT_FOUND)) };
            return std::ptr::null_mut();
        }
        match host("ws2_32.dll", "WSAAsyncGetHostByName") {
            Some(f) => unsafe { std::mem::transmute::<usize, WsaAsyncGetHostByNameFn>(f)(hwnd, msg, name, buf, buflen) },
            None => {
                unsafe { SetLastError(WIN32_ERROR(WSAHOST_NOT_FOUND)) };
                std::ptr::null_mut()
            }
        }
    }

    // WinINet (legacy). A GDK title's HTTP goes through XCurl (socket-contained, a title module) or
    // WinHTTP; WinINet is covered too so there is no HTTP egress entry point left uncontained. Each HTTP
    // library runs its own sockets inside a host DLL, which our socket overrides do not reach, so the
    // library's own entry points must be gated here.

    unsafe extern "system" fn InternetConnectW(
        internet: *mut c_void,
        server: *const u16,
        port: u16,
        user: *const u16,
        pass: *const u16,
        service: u32,
        flags: u32,
        context: usize,
    ) -> *mut c_void {
        let name = unsafe { wstr(server) };
        if !name.as_deref().map(host_allowed).unwrap_or(false) {
            record(Kind::WinInet, name.as_deref().unwrap_or("?"));
            unsafe { SetLastError(WIN32_ERROR(ERROR_INTERNET_CANNOT_CONNECT)) };
            return std::ptr::null_mut();
        }
        match host("wininet.dll", "InternetConnectW") {
            Some(f) => unsafe { std::mem::transmute::<usize, InternetConnectWFn>(f)(internet, server, port, user, pass, service, flags, context) },
            None => {
                unsafe { SetLastError(WIN32_ERROR(ERROR_INTERNET_CANNOT_CONNECT)) };
                std::ptr::null_mut()
            }
        }
    }

    unsafe extern "system" fn InternetConnectA(
        internet: *mut c_void,
        server: *const u8,
        port: u16,
        user: *const u8,
        pass: *const u8,
        service: u32,
        flags: u32,
        context: usize,
    ) -> *mut c_void {
        let name = unsafe { cstr(server) };
        if !name.as_deref().map(host_allowed).unwrap_or(false) {
            record(Kind::WinInet, name.as_deref().unwrap_or("?"));
            unsafe { SetLastError(WIN32_ERROR(ERROR_INTERNET_CANNOT_CONNECT)) };
            return std::ptr::null_mut();
        }
        match host("wininet.dll", "InternetConnectA") {
            Some(f) => unsafe { std::mem::transmute::<usize, InternetConnectAFn>(f)(internet, server, port, user, pass, service, flags, context) },
            None => {
                unsafe { SetLastError(WIN32_ERROR(ERROR_INTERNET_CANNOT_CONNECT)) };
                std::ptr::null_mut()
            }
        }
    }

    unsafe extern "system" fn InternetOpenUrlW(internet: *mut c_void, url: *const u16, headers: *const u16, headers_len: u32, flags: u32, context: usize) -> *mut c_void {
        let u = unsafe { wstr(url) };
        if !u.as_deref().map(url_allowed).unwrap_or(false) {
            record(Kind::WinInet, u.as_deref().unwrap_or("?"));
            unsafe { SetLastError(WIN32_ERROR(ERROR_INTERNET_CANNOT_CONNECT)) };
            return std::ptr::null_mut();
        }
        match host("wininet.dll", "InternetOpenUrlW") {
            Some(f) => unsafe { std::mem::transmute::<usize, InternetOpenUrlWFn>(f)(internet, url, headers, headers_len, flags, context) },
            None => {
                unsafe { SetLastError(WIN32_ERROR(ERROR_INTERNET_CANNOT_CONNECT)) };
                std::ptr::null_mut()
            }
        }
    }

    unsafe extern "system" fn InternetOpenUrlA(internet: *mut c_void, url: *const u8, headers: *const u8, headers_len: u32, flags: u32, context: usize) -> *mut c_void {
        let u = unsafe { cstr(url) };
        if !u.as_deref().map(url_allowed).unwrap_or(false) {
            record(Kind::WinInet, u.as_deref().unwrap_or("?"));
            unsafe { SetLastError(WIN32_ERROR(ERROR_INTERNET_CANNOT_CONNECT)) };
            return std::ptr::null_mut();
        }
        match host("wininet.dll", "InternetOpenUrlA") {
            Some(f) => unsafe { std::mem::transmute::<usize, InternetOpenUrlAFn>(f)(internet, url, headers, headers_len, flags, context) },
            None => {
                unsafe { SetLastError(WIN32_ERROR(ERROR_INTERNET_CANNOT_CONNECT)) };
                std::ptr::null_mut()
            }
        }
    }

    /// `HttpSendRequest{W,A}` and `HttpSendRequestEx{W,A}` fail closed, like `WinHttpSendRequest`: the
    /// request handle's destination can't be re-checked here. FALSE.
    unsafe extern "system" fn HttpSendRequestW(_r: *mut c_void, _headers: *const u16, _headers_len: u32, _optional: *mut c_void, _optional_len: u32) -> i32 {
        record(Kind::WinInet, "HttpSendRequestW (fail closed)");
        unsafe { SetLastError(WIN32_ERROR(ERROR_INTERNET_CANNOT_CONNECT)) };
        0
    }
    unsafe extern "system" fn HttpSendRequestA(_r: *mut c_void, _headers: *const u8, _headers_len: u32, _optional: *mut c_void, _optional_len: u32) -> i32 {
        record(Kind::WinInet, "HttpSendRequestA (fail closed)");
        unsafe { SetLastError(WIN32_ERROR(ERROR_INTERNET_CANNOT_CONNECT)) };
        0
    }
    unsafe extern "system" fn HttpSendRequestExW(_r: *mut c_void, _in: *mut c_void, _out: *mut c_void, _flags: u32, _context: usize) -> i32 {
        record(Kind::WinInet, "HttpSendRequestExW (fail closed)");
        unsafe { SetLastError(WIN32_ERROR(ERROR_INTERNET_CANNOT_CONNECT)) };
        0
    }
    unsafe extern "system" fn HttpSendRequestExA(_r: *mut c_void, _in: *mut c_void, _out: *mut c_void, _flags: u32, _context: usize) -> i32 {
        record(Kind::WinInet, "HttpSendRequestExA (fail closed)");
        unsafe { SetLastError(WIN32_ERROR(ERROR_INTERNET_CANNOT_CONNECT)) };
        0
    }

    /// The wrapper for a network function, by name (independent of the sandbox flag — used by
    /// `override_export` once it has checked the flag, and by the coverage test).
    fn wrapper(name: &str) -> Option<*const ()> {
        let f: *const () = match name {
            "connect" => connect as *const (),
            "WSAConnect" => WSAConnect as *const (),
            "WSAConnectByNameW" => WSAConnectByNameW as *const (),
            "WSAConnectByNameA" => WSAConnectByNameA as *const (),
            "WSAConnectByList" => WSAConnectByList as *const (),
            "WSAIoctl" => WSAIoctl as *const (),
            "sendto" => sendto as *const (),
            "WSASendTo" => WSASendTo as *const (),
            "WSASendMsg" => WSASendMsg as *const (),
            "getaddrinfo" => getaddrinfo as *const (),
            "GetAddrInfoW" => GetAddrInfoW as *const (),
            "GetAddrInfoExW" => GetAddrInfoExW as *const (),
            "gethostbyname" => gethostbyname as *const (),
            "WSAAsyncGetHostByName" => WSAAsyncGetHostByName as *const (),
            "WinHttpConnect" => WinHttpConnect as *const (),
            "WinHttpSendRequest" => WinHttpSendRequest as *const (),
            "InternetConnectW" => InternetConnectW as *const (),
            "InternetConnectA" => InternetConnectA as *const (),
            "InternetOpenUrlW" => InternetOpenUrlW as *const (),
            "InternetOpenUrlA" => InternetOpenUrlA as *const (),
            "HttpSendRequestW" => HttpSendRequestW as *const (),
            "HttpSendRequestA" => HttpSendRequestA as *const (),
            "HttpSendRequestExW" => HttpSendRequestExW as *const (),
            "HttpSendRequestExA" => HttpSendRequestExA as *const (),
            _ => return None,
        };
        Some(f)
    }

    /// Our wrapper for a network function, by name — but only while the sandbox is on. Off, this returns
    /// `None`, so the resolver binds the host's own function and nothing changes.
    pub fn override_export(name: &str) -> Option<u64> {
        if !on() {
            return None;
        }
        wrapper(name).map(|f| f as usize as u64)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// An `AF_INET` `sockaddr_in` for `a.b.c.d`.
        fn sa_in(a: u8, b: u8, c: u8, d: u8) -> [u8; 16] {
            let mut s = [0u8; 16];
            s[0..2].copy_from_slice(&2u16.to_ne_bytes());
            s[4..8].copy_from_slice(&[a, b, c, d]);
            s
        }

        #[test]
        fn connectex_guid_is_the_known_little_endian_bytes() {
            // {25A207B9-DDF3-4660-8EE9-76E58C74063E}
            assert_eq!(
                WSAID_CONNECTEX,
                [0xB9, 0x07, 0xA2, 0x25, 0xF3, 0xDD, 0x60, 0x46, 0x8E, 0xE9, 0x76, 0xE5, 0x8C, 0x74, 0x06, 0x3E]
            );
            assert_eq!(SIO_GET_EXTENSION_FUNCTION_POINTER, 0xC800_0006);
        }

        #[test]
        fn a_connect_by_list_is_refused_if_any_member_is_public() {
            let loopback = sa_in(127, 0, 0, 1);
            let public = sa_in(8, 8, 8, 8);
            // one entry, loopback: forwarded
            let mut ok = vec![0u8; 8 + 16];
            ok[0..4].copy_from_slice(&1i32.to_ne_bytes());
            ok[8..16].copy_from_slice(&(loopback.as_ptr() as usize).to_ne_bytes());
            ok[16..20].copy_from_slice(&16i32.to_ne_bytes());
            assert!(unsafe { addr_list_decision(ok.as_ptr()) }.is_ok());
            // two entries, loopback + public: refused
            let mut bad = vec![0u8; 8 + 32];
            bad[0..4].copy_from_slice(&2i32.to_ne_bytes());
            bad[8..16].copy_from_slice(&(loopback.as_ptr() as usize).to_ne_bytes());
            bad[16..20].copy_from_slice(&16i32.to_ne_bytes());
            bad[24..32].copy_from_slice(&(public.as_ptr() as usize).to_ne_bytes());
            bad[32..36].copy_from_slice(&16i32.to_ne_bytes());
            assert_eq!(unsafe { addr_list_decision(bad.as_ptr()) }, Err("8.8.8.8".to_string()));
            assert!(unsafe { addr_list_decision(std::ptr::null()) }.is_err());
        }

        #[test]
        fn a_wsamsg_to_a_public_address_is_refused_and_loopback_or_no_address_is_allowed() {
            let public = sa_in(93, 184, 216, 34);
            let loopback = sa_in(127, 0, 0, 1);
            // WSAMSG: name ptr (offset 0), namelen (offset 8)
            let mut msg = [0u8; 56];
            msg[0..8].copy_from_slice(&(public.as_ptr() as usize).to_ne_bytes());
            msg[8..12].copy_from_slice(&16i32.to_ne_bytes());
            assert_eq!(unsafe { wsamsg_decision(msg.as_ptr()) }, Err("93.184.216.34".to_string()));
            msg[0..8].copy_from_slice(&(loopback.as_ptr() as usize).to_ne_bytes());
            assert!(unsafe { wsamsg_decision(msg.as_ptr()) }.is_ok());
            // a connected socket: no name
            let mut connected = [0u8; 56];
            connected[8..12].copy_from_slice(&0i32.to_ne_bytes());
            assert!(unsafe { wsamsg_decision(connected.as_ptr()) }.is_ok());
            assert!(unsafe { wsamsg_decision(std::ptr::null()) }.is_ok());
        }

        #[test]
        fn every_network_override_has_a_wrapper() {
            // The resolver calls override_export for every function listed in OVERRIDES as "network";
            // each one must have a containment wrapper here, or that name would silently bind to the host
            // (an egress hole) when the sandbox is on.
            let names: Vec<&str> = crate::OVERRIDES.iter().filter(|(_, w)| *w == "network").map(|(f, _)| *f).collect();
            assert!(names.contains(&"WSAIoctl"), "ConnectEx is contained through WSAIoctl");
            assert!(!names.contains(&"ConnectEx"), "ConnectEx is not an import; it comes via the WSAIoctl extension pointer");
            for n in &names {
                assert!(wrapper(n).is_some(), "{n} is in OVERRIDES(network) but has no wrapper");
            }
            // and nothing outside the list maps to a wrapper
            assert!(wrapper("WaitForSingleObject").is_none());
        }
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

    #[test]
    fn url_hosts_are_extracted_and_only_local_ones_are_allowed() {
        assert_eq!(url_host("https://xboxlive.com/path?q=1"), Some("xboxlive.com".into()));
        assert_eq!(url_host("http://user:pw@title.example.com:8443/x"), Some("title.example.com".into()));
        assert_eq!(url_host("http://127.0.0.1:8080/health"), Some("127.0.0.1".into()));
        assert_eq!(url_host("http://[::1]:9000/"), Some("[::1]".into()));
        assert_eq!(url_host("localhost"), Some("localhost".into()));
        // only local URLs are allowed out
        assert!(!url_allowed("https://xboxlive.com/"));
        assert!(!url_allowed("https://93.184.216.34/"));
        assert!(url_allowed("http://127.0.0.1:8080/health"));
        assert!(url_allowed("http://localhost/x"));
        assert!(url_allowed("http://[::1]/x"));
        assert!(!url_allowed(""));
    }
}
