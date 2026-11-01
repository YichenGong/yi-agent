//! 电源来源与网络可达性探测。
use std::net::{IpAddr, SocketAddr, TcpStream};
use std::time::Duration;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);

pub trait PowerProbe: Send + Sync {
    fn on_ac(&self) -> bool;
}

pub trait NetProbe: Send + Sync {
    fn online(&self) -> bool;
}

pub fn is_on_ac(power_source_type: &str) -> bool {
    power_source_type != "Battery Power"
}

pub fn is_virtual_interface(name: &str) -> bool {
    const PREFIXES: &[&str] = &[
        "utun", "awdl", "llw", "bridge", "gif", "stf", "anpi", "ap", "tun", "tap", "docker", "br-",
        "veth", "virbr", "lo",
    ];
    PREFIXES.iter().any(|prefix| name.starts_with(prefix))
}

const IFF_LOOPBACK: u32 = 0x8;

pub fn has_usable_interface(interfaces: &[(String, u32, bool)]) -> bool {
    interfaces.iter().any(|(name, flags, has_v4)| {
        let up = flags & (libc::IFF_UP as u32) != 0;
        let loopback = flags & IFF_LOOPBACK != 0;
        up && !loopback && *has_v4 && !is_virtual_interface(name)
    })
}

pub fn decide_online(has_interface: Option<bool>, gateway_reachable: Option<bool>) -> bool {
    match (has_interface, gateway_reachable) {
        (Some(false), _) => false,
        (_, Some(reachable)) => reachable,
        (Some(true), None) => true,
        (None, None) => false,
    }
}

pub fn parse_gateway(route_output: &str) -> Option<IpAddr> {
    route_output.lines().find_map(|line| {
        let rest = line.trim().strip_prefix("gateway:")?;
        rest.trim().parse::<IpAddr>().ok()
    })
}

pub fn classify_connect_result(result: std::io::Result<()>) -> bool {
    match result {
        Ok(()) => true,
        Err(error) => matches!(error.raw_os_error(), Some(libc::ECONNREFUSED)),
    }
}

pub struct SystemPowerProbe;

impl SystemPowerProbe {
    pub fn new() -> Self {
        Self
    }
}
impl Default for SystemPowerProbe {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(target_os = "macos")]
impl PowerProbe for SystemPowerProbe {
    fn on_ac(&self) -> bool {
        mac::providing_power_source_type()
            .map(|ty| is_on_ac(&ty))
            .unwrap_or(true)
    }
}

#[cfg(not(target_os = "macos"))]
impl PowerProbe for SystemPowerProbe {
    fn on_ac(&self) -> bool {
        true
    }
}

#[cfg(target_os = "macos")]
mod mac {
    use std::ffi::{CStr, c_char, c_void};

    type CFTypeRef = *const c_void;
    type CFStringRef = *const c_void;
    type Boolean = u8;
    const UTF8: u32 = 0x0800_0100;

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFStringGetCString(
            s: CFStringRef,
            buffer: *mut c_char,
            size: isize,
            encoding: u32,
        ) -> Boolean;
        fn CFRelease(cf: CFTypeRef);
    }

    #[link(name = "IOKit", kind = "framework")]
    unsafe extern "C" {
        fn IOPSCopyPowerSourcesInfo() -> CFTypeRef;
        fn IOPSGetProvidingPowerSourceType(blob: CFTypeRef) -> CFStringRef;
    }

    pub(super) fn providing_power_source_type() -> Option<String> {
        unsafe {
            let blob = IOPSCopyPowerSourcesInfo();
            if blob.is_null() {
                return None;
            }
            let ty = IOPSGetProvidingPowerSourceType(blob);
            if ty.is_null() {
                CFRelease(blob);
                return None;
            }
            let mut buffer = [0i8; 128];
            let ok = CFStringGetCString(ty, buffer.as_mut_ptr(), buffer.len() as isize, UTF8);
            let value = if ok != 0 {
                Some(
                    CStr::from_ptr(buffer.as_ptr())
                        .to_string_lossy()
                        .into_owned(),
                )
            } else {
                None
            };
            CFRelease(blob);
            value
        }
    }
}

pub struct SystemNetProbe;

impl SystemNetProbe {
    pub fn new() -> Self {
        Self
    }
}
impl Default for SystemNetProbe {
    fn default() -> Self {
        Self::new()
    }
}

impl NetProbe for SystemNetProbe {
    fn online(&self) -> bool {
        let interfaces = collect_interfaces();
        let has_interface = if interfaces.is_empty() {
            None
        } else {
            Some(has_usable_interface(&interfaces))
        };
        let gateway = default_gateway().map(|ip| {
            let addr = SocketAddr::new(ip, 80);
            match TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT) {
                Ok(_) => true,
                Err(error) => classify_connect_result(Err(error)),
            }
        });
        let online = decide_online(has_interface, gateway);
        if gateway.is_none() {
            eprintln!(
                "[yi-agent-power] cannot determine the default gateway; falling back to the interface check"
            );
        }
        online
    }
}

fn collect_interfaces() -> Vec<(String, u32, bool)> {
    #[cfg(unix)]
    {
        use std::ffi::CStr;
        let mut out: Vec<(String, u32, bool)> = Vec::new();
        let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
        // SAFETY: `getifaddrs` 填充 `head`；成功时链表由我们 freeifaddrs。
        let rc = unsafe { libc::getifaddrs(&mut head) };
        if rc != 0 || head.is_null() {
            return out;
        }
        let mut cursor = head;
        while !cursor.is_null() {
            // SAFETY: `cursor` 在链表内且非空。
            let entry = unsafe { &*cursor };
            if !entry.ifa_name.is_null() {
                // SAFETY: `ifa_name` 是 NUL 结尾字符串。
                let name = unsafe { CStr::from_ptr(entry.ifa_name) }
                    .to_string_lossy()
                    .into_owned();
                let flags = entry.ifa_flags;
                let has_v4 = !entry.ifa_addr.is_null()
                    // SAFETY: 非空时 `ifa_addr` 指向 sockaddr，首字段是 sa_family。
                    && unsafe { (*entry.ifa_addr).sa_family as i32 } == libc::AF_INET;
                match out.iter_mut().find(|(existing, _, _)| existing == &name) {
                    Some(slot) => slot.2 |= has_v4,
                    None => out.push((name, flags, has_v4)),
                }
            }
            cursor = entry.ifa_next;
        }
        // SAFETY: 链表来自 getifaddrs，此处释放一次。
        unsafe { libc::freeifaddrs(head) };
        out
    }
    #[cfg(not(unix))]
    {
        Vec::new()
    }
}

fn default_gateway() -> Option<IpAddr> {
    let output = std::process::Command::new("route")
        .args(["-n", "get", "default"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_gateway(&String::from_utf8_lossy(&output.stdout))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::IpAddr;

    #[test]
    fn battery_is_the_only_thing_that_is_not_ac() {
        assert!(!is_on_ac("Battery Power"));
        assert!(is_on_ac("AC Power"));
        assert!(is_on_ac("UPS Power"));
        assert!(is_on_ac("Something New"));
    }

    #[test]
    fn virtual_interface_prefixes_are_recognised() {
        for name in [
            "utun0", "utun4", "awdl0", "llw0", "bridge0", "gif0", "stf0", "anpi1", "ap1", "tun0",
            "tap0", "docker0", "br-1234", "veth1a2b", "virbr0", "lo",
        ] {
            assert!(is_virtual_interface(name), "{name} must be virtual");
        }
        for name in ["en0", "en1", "eth0", "ens5", "enp0s3", "wlan0", "eno1"] {
            assert!(!is_virtual_interface(name), "{name} must not be virtual");
        }
    }

    #[test]
    fn interfaces_without_an_address_are_not_candidates() {
        let ifaces = vec![
            ("en1".to_string(), 0x1u32, false),
            ("en2".to_string(), 0x1u32, false),
            ("utun4".to_string(), 0x1u32, true),
        ];
        assert!(!has_usable_interface(&ifaces));
    }

    #[test]
    fn a_real_interface_with_an_address_is_a_candidate_whatever_its_name() {
        assert!(has_usable_interface(&[("ens5".to_string(), 0x1u32, true)]));
    }

    #[test]
    fn a_tunnel_with_an_address_is_still_not_a_candidate() {
        assert!(!has_usable_interface(&[(
            "utun4".to_string(),
            0x1u32,
            true
        )]));
    }

    #[test]
    fn loopback_is_never_a_candidate() {
        assert!(!has_usable_interface(&[("lo0".to_string(), 0x9u32, true)]));
    }

    #[test]
    fn the_default_gateway_is_parsed_out_of_route_output() {
        let output = "   route to: default\ndestination: default\n       mask: default\n    gateway: 192.168.31.1\n       interface: en0\n";
        assert_eq!(
            parse_gateway(output),
            Some("192.168.31.1".parse::<IpAddr>().unwrap())
        );
    }

    #[test]
    fn a_link_scoped_gateway_parses_to_none() {
        assert_eq!(
            parse_gateway("   route to: default\ndestination: default\n    gateway: link#28\n"),
            None
        );
    }

    #[test]
    fn missing_output_parses_to_none() {
        assert_eq!(parse_gateway(""), None);
        assert_eq!(
            parse_gateway("route: writing to routing socket: not in table"),
            None
        );
    }

    #[test]
    fn connection_refused_counts_as_online() {
        let refused = std::io::Error::from_raw_os_error(libc::ECONNREFUSED);
        assert!(classify_connect_result(Err(refused)));
    }

    #[test]
    fn a_successful_connect_counts_as_online() {
        assert!(classify_connect_result(Ok(())));
    }

    #[test]
    fn timeouts_and_unreachable_networks_count_as_offline() {
        for code in [libc::ETIMEDOUT, libc::EHOSTUNREACH, libc::ENETUNREACH] {
            let err = std::io::Error::from_raw_os_error(code);
            assert!(
                !classify_connect_result(Err(err)),
                "errno {code} must be offline"
            );
        }
    }

    #[test]
    fn the_system_probes_run_without_panicking() {
        let power = SystemPowerProbe::new();
        let _ = power.on_ac();
        let net = SystemNetProbe::new();
        let _ = net.online();
    }

    #[test]
    fn no_gateway_falls_back_to_the_interface_check() {
        assert!(decide_online(Some(true), None));
        assert!(!decide_online(Some(false), None));
        assert!(decide_online(Some(true), Some(true)));
        assert!(!decide_online(Some(true), Some(false)));
    }
}
