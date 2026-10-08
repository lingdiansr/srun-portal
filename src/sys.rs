//! System probes the reference CLI obtains from Node built-ins.
//!
//! * `os.userInfo().homedir` -> [`home_dir`] (passwd entry, not `$HOME`; SPEC §3.1)
//! * `os.networkInterfaces()` -> [`network_interfaces`] (SPEC §3.3)
//! * `os.platform()` / `os.type()` -> [`platform`] / [`os_type`] (SPEC §7.1)
//! * `SIGINT` handler -> [`install_sigint_handler`] (SPEC §10)
//!
//! The surface above is portable; each target answers it with its own syscalls,
//! in a `unix`/`windows` submodule next to the portable parts
//! ([`correct_user_ips`], the line readers). The device identity strings are
//! Node's, spelled per platform, because SPEC §7.1 sends them verbatim as the
//! `os` and `name` parameters of the login request.

use std::path::PathBuf;

/// `os.platform()`: `'linux'`, `'darwin'` or `'win32'`.
#[cfg(target_os = "linux")]
pub fn platform() -> &'static str {
    "linux"
}

/// `os.platform()`: `'linux'`, `'darwin'` or `'win32'`.
#[cfg(target_os = "macos")]
pub fn platform() -> &'static str {
    "darwin"
}

/// `os.platform()`: `'linux'`, `'darwin'` or `'win32'`.
#[cfg(target_os = "windows")]
pub fn platform() -> &'static str {
    "win32"
}

/// `os.type()`: `'Linux'`, `'Darwin'` or `'Windows_NT'`.
#[cfg(target_os = "linux")]
pub fn os_type() -> &'static str {
    "Linux"
}

/// `os.type()`: `'Linux'`, `'Darwin'` or `'Windows_NT'`.
#[cfg(target_os = "macos")]
pub fn os_type() -> &'static str {
    "Darwin"
}

/// `os.type()`: `'Linux'`, `'Darwin'` or `'Windows_NT'`.
#[cfg(target_os = "windows")]
pub fn os_type() -> &'static str {
    "Windows_NT"
}

/// The directory holding the running executable, where a portable
/// `srun-portal.toml` lives.
pub fn exe_dir() -> Result<PathBuf, String> {
    let exe = std::env::current_exe()
        .map_err(|err| format!("cannot locate the running executable: {err}"))?;
    exe.parent()
        .map(|dir| dir.to_path_buf())
        .ok_or_else(|| format!("{} has no parent directory", exe.display()))
}

/// One address of one interface, as `os.networkInterfaces()` reports them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetIfAddr {
    pub name: String,
    pub addr: IpAddr,
}

/// `constructorChecker`: find the interface that owns `config_ip` (or the
/// other-stack address when it is already known) and overwrite `user_ip` /
/// `user_ip_other_stack` with that interface's IPv4 / IPv6 address (SPEC §3.3).
///
/// An address that has no counterpart on the matching interface leaves the
/// previous value in place, which is what the reference does when the property
/// is missing from the interface record.
pub fn correct_user_ips(
    interfaces: &[NetIfAddr],
    config_ip: &str,
    user_ip: &mut String,
    user_ip_other_stack: &mut String,
) {
    let owner = interfaces.iter().find(|iface| {
        iface.name_is_owner_of(config_ip) || iface.name_is_owner_of(user_ip_other_stack)
    });
    let Some(owner) = owner else { return };
    let v4 = interfaces
        .iter()
        .find(|i| i.name == owner.name && i.addr.is_ipv4())
        .map(|i| i.addr.to_string());
    let v6 = interfaces
        .iter()
        .find(|i| i.name == owner.name && i.addr.is_ipv6())
        .map(|i| i.addr.to_string());
    if let Some(v4) = v4 {
        *user_ip = v4;
    }
    if let Some(v6) = v6 {
        *user_ip_other_stack = v6;
    }
}

impl NetIfAddr {
    fn name_is_owner_of(&self, addr: &str) -> bool {
        !addr.is_empty() && self.addr.to_string() == addr
    }
}

/// Read one line from stdin, without its line terminator. `None` on EOF.
pub fn read_line() -> Result<Option<String>, String> {
    let mut line = String::new();
    match std::io::stdin().read_line(&mut line) {
        Ok(0) => Ok(None),
        Ok(_) => {
            let line = line.trim_end_matches('\n').trim_end_matches('\r');
            Ok(Some(line.to_string()))
        }
        Err(err) => Err(format!("read stdin: {err}")),
    }
}

/// Prompt on stderr and read a line. `None` on EOF.
pub fn prompt_line(label: &str) -> Result<Option<String>, String> {
    eprint!("{label}");
    let _ = std::io::Write::flush(&mut std::io::stderr());
    read_line()
}

pub use std::net::IpAddr;

pub use platform_impl::{
    home_dir, install_sigint_handler, network_interfaces, prompt_password, stdin_is_tty,
};

#[cfg(unix)]
mod platform_impl {
    use super::{IpAddr, NetIfAddr};
    use std::ffi::CStr;
    use std::net::{Ipv4Addr, Ipv6Addr};
    use std::path::PathBuf;

    /// Whether stdin is a terminal (`isatty(STDIN_FILENO)`).
    ///
    /// A piped stdin is not one, which is what makes the prompts degrade instead
    /// of blocking when the CLI is scripted.
    pub fn stdin_is_tty() -> bool {
        unsafe { libc::isatty(libc::STDIN_FILENO) == 1 }
    }

    /// `os.userInfo().homedir`: the home directory of the *real* uid's passwd
    /// entry.
    ///
    /// Falls back to `$HOME` when the passwd lookup fails (no NSS entry), which
    /// is only observable under `sudo`-style uid changes (SPEC §3.1).
    pub fn home_dir() -> Result<PathBuf, String> {
        if let Some(home) = passwd_home(unsafe { libc::getuid() }) {
            return Ok(PathBuf::from(home));
        }
        match std::env::var_os("HOME") {
            Some(home) if !home.is_empty() => Ok(PathBuf::from(home)),
            _ => Err("cannot determine home directory".to_string()),
        }
    }

    fn passwd_home(uid: libc::uid_t) -> Option<String> {
        let mut size = match unsafe { libc::sysconf(libc::_SC_GETPW_R_SIZE_MAX) } {
            n if n > 0 => n as usize,
            _ => 1024,
        };
        loop {
            let mut buf = vec![0i8; size];
            let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
            let mut result: *mut libc::passwd = std::ptr::null_mut();
            let rc = unsafe {
                libc::getpwuid_r(uid, &mut pwd, buf.as_mut_ptr(), buf.len(), &mut result)
            };
            if rc == libc::ERANGE && size < 1 << 20 {
                size *= 2;
                continue;
            }
            if rc != 0 || result.is_null() || pwd.pw_dir.is_null() {
                return None;
            }
            let dir = unsafe { CStr::from_ptr(pwd.pw_dir) };
            return dir.to_str().ok().map(|s| s.to_string());
        }
    }

    /// `os.networkInterfaces()`: every interface address, IPv4 and IPv6.
    pub fn network_interfaces() -> Vec<NetIfAddr> {
        let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
        if unsafe { libc::getifaddrs(&mut list) } != 0 {
            return Vec::new();
        }
        let mut out = Vec::new();
        let mut cur = list;
        while !cur.is_null() {
            let ifa = unsafe { &*cur };
            if !ifa.ifa_name.is_null() && !ifa.ifa_addr.is_null() {
                let name = unsafe { CStr::from_ptr(ifa.ifa_name) }
                    .to_string_lossy()
                    .into_owned();
                let family = unsafe { (*ifa.ifa_addr).sa_family } as libc::c_int;
                let addr = match family {
                    libc::AF_INET => {
                        let sa = unsafe { &*(ifa.ifa_addr as *const libc::sockaddr_in) };
                        Some(IpAddr::V4(Ipv4Addr::from(u32::from_be(sa.sin_addr.s_addr))))
                    }
                    libc::AF_INET6 => {
                        let sa = unsafe { &*(ifa.ifa_addr as *const libc::sockaddr_in6) };
                        Some(IpAddr::V6(Ipv6Addr::from(sa.sin6_addr.s6_addr)))
                    }
                    _ => None,
                };
                if let Some(addr) = addr {
                    out.push(NetIfAddr { name, addr });
                }
            }
            cur = unsafe { (*cur).ifa_next };
        }
        unsafe { libc::freeifaddrs(list) };
        out
    }

    /// Install the `SIGINT` handler: print `"\n\nPortal exit!\n"` and exit
    /// (SPEC §10).
    pub fn install_sigint_handler() {
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = handle_sigint as extern "C" fn(libc::c_int) as libc::sighandler_t;
            action.sa_flags = 0;
            libc::sigemptyset(&mut action.sa_mask);
            libc::sigaction(libc::SIGINT, &action, std::ptr::null_mut());
        }
    }

    extern "C" fn handle_sigint(_sig: libc::c_int) {
        // async-signal-safe: a single write(2), then _exit
        let msg = crate::messages::EXIT_MESSAGE.as_bytes();
        unsafe {
            libc::write(
                libc::STDERR_FILENO,
                msg.as_ptr() as *const libc::c_void,
                msg.len(),
            );
            libc::_exit(0);
        }
    }

    /// Prompt and read a line with terminal echo disabled, the way the
    /// reference's `invisible` prompt behaves. Falls back to a plain read when
    /// stdin is not a terminal (piped input), because there is no echo to
    /// suppress there.
    pub fn prompt_password(label: &str) -> Result<Option<String>, String> {
        use std::io::Write;

        let tty = stdin_is_tty();
        if !tty {
            return super::prompt_line(label);
        }
        eprint!("{label}");
        let _ = std::io::stderr().flush();

        let mut saved: libc::termios = unsafe { std::mem::zeroed() };
        let muted = unsafe { libc::tcgetattr(libc::STDIN_FILENO, &mut saved) } == 0;
        let mut restored = saved;
        if muted {
            restored.c_lflag &= !libc::ECHO;
            unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &restored) };
        }
        let line = super::read_line();
        if muted {
            unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &saved) };
        }
        eprintln!();
        line
    }
}

#[cfg(windows)]
mod platform_impl {
    use super::{IpAddr, NetIfAddr};
    use std::net::{Ipv4Addr, Ipv6Addr};
    use std::path::PathBuf;
    use windows_sys::Win32::Foundation::{ERROR_BUFFER_OVERFLOW, HANDLE};
    use windows_sys::Win32::NetworkManagement::IpHelper::{
        GetAdaptersAddresses, GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_MULTICAST,
        IP_ADAPTER_ADDRESSES_LH,
    };
    use windows_sys::Win32::Networking::WinSock::{
        AF_INET, AF_INET6, AF_UNSPEC, SOCKADDR, SOCKADDR_IN, SOCKADDR_IN6,
    };
    use windows_sys::Win32::System::Console::{
        GetConsoleMode, GetStdHandle, SetConsoleCtrlHandler, SetConsoleMode, ENABLE_ECHO_INPUT,
        STD_INPUT_HANDLE,
    };

    /// Whether stdin is a terminal (`GetConsoleMode` on `STD_INPUT_HANDLE`).
    ///
    /// The Windows console answers this; a redirected stdin or a MSYS-style pipe
    /// does not, so `windows-sys` is the right probe here.
    pub fn stdin_is_tty() -> bool {
        let mut mode = 0u32;
        unsafe { GetConsoleMode(std_handle(), &mut mode) != 0 }
    }

    fn std_handle() -> HANDLE {
        unsafe { GetStdHandle(STD_INPUT_HANDLE) }
    }

    /// `os.userInfo().homedir`: `%USERPROFILE%`, or `%HOMEDRIVE%%HOMEPATH%`.
    pub fn home_dir() -> Result<PathBuf, String> {
        match std::env::var_os("USERPROFILE") {
            Some(home) if !home.is_empty() => return Ok(PathBuf::from(home)),
            _ => {}
        }
        let drive = std::env::var_os("HOMEDRIVE").unwrap_or_default();
        let path = std::env::var_os("HOMEPATH").unwrap_or_default();
        if !drive.is_empty() && !path.is_empty() {
            let mut home = PathBuf::from(drive);
            home.push(path);
            return Ok(home);
        }
        Err("cannot determine home directory".to_string())
    }

    /// `os.networkInterfaces()`: every interface address, IPv4 and IPv6.
    ///
    /// Down adapters are listed too, like the unix probe (which does not filter
    /// `IFF_UP`); a failing call yields no interfaces at all.
    pub fn network_interfaces() -> Vec<NetIfAddr> {
        // `GetAdaptersAddresses` fills the buffer and answers
        // `ERROR_BUFFER_OVERFLOW` with the size it needs; retry once with it.
        let flags = GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST;
        let mut size = 16 * 1024u32;
        for _ in 0..2 {
            let mut buf = vec![0u8; size as usize];
            let rc = unsafe {
                GetAdaptersAddresses(
                    AF_UNSPEC as u32,
                    flags,
                    std::ptr::null(),
                    buf.as_mut_ptr() as *mut IP_ADAPTER_ADDRESSES_LH,
                    &mut size,
                )
            };
            if rc == ERROR_BUFFER_OVERFLOW {
                continue;
            }
            if rc != 0 {
                return Vec::new();
            }
            return unsafe { collect(buf.as_ptr() as *const IP_ADAPTER_ADDRESSES_LH) };
        }
        Vec::new()
    }

    /// Walk the adapter list, then each adapter's unicast address list.
    unsafe fn collect(first: *const IP_ADAPTER_ADDRESSES_LH) -> Vec<NetIfAddr> {
        let mut out = Vec::new();
        let mut adapter = first;
        while !adapter.is_null() {
            let name = unsafe { pwstr_to_string((*adapter).FriendlyName) };
            let mut unicast = unsafe { (*adapter).FirstUnicastAddress };
            while !unicast.is_null() {
                let sa = unsafe { (*unicast).Address.lpSockaddr };
                if !sa.is_null() {
                    if let Some(addr) = unsafe { parse_sockaddr(sa) } {
                        out.push(NetIfAddr {
                            name: name.clone(),
                            addr,
                        });
                    }
                }
                unicast = unsafe { (*unicast).Next };
            }
            adapter = unsafe { (*adapter).Next };
        }
        out
    }

    unsafe fn pwstr_to_string(ptr: *const u16) -> String {
        if ptr.is_null() {
            return String::new();
        }
        let len = unsafe { (0..).take_while(|i| *ptr.add(*i) != 0).count() };
        String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(ptr, len) })
    }

    unsafe fn parse_sockaddr(sa: *const SOCKADDR) -> Option<IpAddr> {
        match unsafe { (*sa).sa_family } {
            AF_INET => {
                let v4 = unsafe { &*(sa as *const SOCKADDR_IN) };
                Some(IpAddr::V4(Ipv4Addr::from(u32::from_be(unsafe {
                    v4.sin_addr.S_un.S_addr
                }))))
            }
            AF_INET6 => {
                let v6 = unsafe { &*(sa as *const SOCKADDR_IN6) };
                Some(IpAddr::V6(Ipv6Addr::from(unsafe { v6.sin6_addr.u.Byte })))
            }
            _ => None,
        }
    }

    /// Install the `Ctrl+C` handler: print `"\n\nPortal exit!\n"` and exit
    /// (SPEC §10).
    pub fn install_sigint_handler() {
        unsafe { SetConsoleCtrlHandler(Some(handle_ctrl), 1) };
    }

    unsafe extern "system" fn handle_ctrl(_ctrl_type: u32) -> windows_sys::core::BOOL {
        use std::io::Write;
        let _ = std::io::stderr().write_all(crate::messages::EXIT_MESSAGE.as_bytes());
        std::process::exit(0);
    }

    /// Prompt and read a line with terminal echo disabled, the way the
    /// reference's `invisible` prompt behaves. Falls back to a plain read when
    /// stdin is not a terminal (piped input), because there is no echo to
    /// suppress there.
    pub fn prompt_password(label: &str) -> Result<Option<String>, String> {
        use std::io::Write;

        let handle = std_handle();
        let mut saved = 0u32;
        if unsafe { GetConsoleMode(handle, &mut saved) } == 0 {
            return super::prompt_line(label);
        }
        eprint!("{label}");
        let _ = std::io::stderr().flush();
        unsafe { SetConsoleMode(handle, saved & !ENABLE_ECHO_INPUT) };
        let line = super::read_line();
        unsafe { SetConsoleMode(handle, saved) };
        eprintln!();
        line
    }
}

#[cfg(not(any(unix, windows)))]
compile_error!("srun-portal supports unix and windows targets only");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_identity_matches_node_semantics() {
        #[cfg(target_os = "linux")]
        {
            assert_eq!(platform(), "linux");
            assert_eq!(os_type(), "Linux");
        }
        #[cfg(target_os = "macos")]
        {
            assert_eq!(platform(), "darwin");
            assert_eq!(os_type(), "Darwin");
        }
        #[cfg(target_os = "windows")]
        {
            assert_eq!(platform(), "win32");
            assert_eq!(os_type(), "Windows_NT");
        }
    }

    #[cfg(unix)]
    #[test]
    fn passwd_home_is_absolute() {
        let home = home_dir().expect("HOME resolvable");
        assert!(home.is_absolute(), "{home:?}");
    }

    #[test]
    fn interfaces_have_loopback() {
        let ifaces = network_interfaces();
        assert!(
            ifaces.iter().any(|i| i.addr.is_loopback()),
            "expected a loopback address, got {ifaces:?}"
        );
    }

    #[test]
    fn correct_user_ips_adopts_matching_interface() {
        let ifaces = vec![
            NetIfAddr {
                name: "lo".into(),
                addr: "127.0.0.1".parse().unwrap(),
            },
            NetIfAddr {
                name: "eth0".into(),
                addr: "10.0.0.5".parse().unwrap(),
            },
            NetIfAddr {
                name: "eth0".into(),
                addr: "2001:db8::5".parse().unwrap(),
            },
        ];
        let mut ip = "10.0.0.5".to_string();
        let mut other = String::new();
        correct_user_ips(&ifaces, "10.0.0.5", &mut ip, &mut other);
        assert_eq!(ip, "10.0.0.5");
        assert_eq!(other, "2001:db8::5");

        // unmatched address: nothing is rewritten
        let mut ip = "192.0.2.1".to_string();
        let mut other = String::new();
        correct_user_ips(&ifaces, "192.0.2.1", &mut ip, &mut other);
        assert_eq!(ip, "192.0.2.1");
        assert_eq!(other, "");
    }
}
