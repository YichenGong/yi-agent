//! 电源来源与网络资格探测。
//!
//! **网络词义收窄**：这里判断的是「本机有没有一条可用的本地链路」，不是
//! 「互联网或 provider 是否可达」。这样一个布尔值就能表达的原因：把某个
//! 远端服务（网关 TCP 80、DNS、provider）当作在线证据，等于把别人的可用性
//! 变成我们的策略输入——网关丢弃 TCP 80 时任务明明能跑，断言却会被释放。
//!
//! 所以采样是**被动**的：一次 `getifaddrs` 快照，不发起任何连接、不起子进程。
//! 局域网有地址而上游断开时允许保活，这是「保任务优先」的显式取舍。
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

pub trait PowerProbe: Send + Sync {
    fn on_ac(&self) -> bool;
}

/// 一次 `getifaddrs` 快照里单个接口的聚合视图：同名接口的多条地址节点合并
/// 成一项，`flags` 取并集。平台层只负责复制，资格判定在这里做纯函数。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterfaceSnapshot {
    pub name: String,
    pub flags: u32,
    pub addresses: Vec<IpAddr>,
}

/// 网络资格三态。
///
/// `Unknown` 不是「离线」：它表示**没有可信快照**（`getifaddrs` 失败或返回空
/// 链表）。采样失败时倒向「允许保活」，因为丢掉一次保活比多保一段时间更难
/// 补救；但仍受开关、电源来源与 busy 约束，不是无条件 acquire。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkState {
    Online,
    Offline,
    Unknown,
}

impl NetworkState {
    /// 是否提供保活资格。只有确认离线才不给。
    pub fn eligible(self) -> bool {
        !matches!(self, Self::Offline)
    }
}

/// 网络资格探针。三态而非布尔：采样失败与「确认离线」是两件事，
/// 混成一个 `false` 会让一次 FFI 抖动被当成断网。
pub trait NetProbe: Send + Sync {
    fn state(&self) -> NetworkState;
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

/// 该地址能否单独证明一条可用的本地链路。
///
/// IPv6 的 link-local 用 octets 判别（`fe80::/10`）而不是新稳定 API，避免
/// 抬高 crate 的 `rust-version`。ULA（`fc00::/7`）与全局单播都允许：它们说明
/// 接口拿到了可路由地址，而不是只挂了个链路本地地址。
fn usable_address(address: &IpAddr) -> bool {
    match address {
        IpAddr::V4(ip) => {
            !ip.is_unspecified()
                && !ip.is_loopback()
                && !ip.is_multicast()
                && !ip.is_link_local()
                && *ip != Ipv4Addr::BROADCAST
        }
        IpAddr::V6(ip) => {
            let octets = ip.octets();
            !ip.is_unspecified()
                && !ip.is_loopback()
                && !ip.is_multicast()
                && !(octets[0] == 0xfe && (octets[1] & 0xc0) == 0x80)
        }
    }
}

/// 快照 → 三态。**成功但无候选是 `Offline`**，不是 `Unknown`：
/// 「遍历成功且确实没有可用链路」是一个确定结论。
///
/// 合格接口需同时满足：`IFF_UP` 与 `IFF_RUNNING` 都置位、不是 loopback、
/// 不在虚拟/旁路前缀集合里、且至少一个可用单播地址。
pub fn network_state(interfaces: &[InterfaceSnapshot]) -> NetworkState {
    let has_candidate = interfaces.iter().any(|interface| {
        let up = interface.flags & (libc::IFF_UP as u32) != 0;
        let running = interface.flags & (libc::IFF_RUNNING as u32) != 0;
        let loopback = interface.flags & IFF_LOOPBACK != 0;
        up && running
            && !loopback
            && !is_virtual_interface(&interface.name)
            && interface.addresses.iter().any(usable_address)
    });
    if has_candidate {
        NetworkState::Online
    } else {
        NetworkState::Offline
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
    fn state(&self) -> NetworkState {
        // 采样失败（含无 unix 支持的构建）是 `Unknown`，不是 `Offline`。
        match collect_interfaces() {
            Some(interfaces) => network_state(&interfaces),
            None => NetworkState::Unknown,
        }
    }
}

/// 把同名接口的多条 `getifaddrs` 节点合并成一项。
///
/// 两条节点分别带 v4/v6 地址时，`addresses` 保留两者——否则双栈接口会因为
/// 「第一个节点只有 v6」而被误判。
#[cfg_attr(not(unix), allow(dead_code))]
fn aggregate<I>(entries: I) -> Vec<InterfaceSnapshot>
where
    I: IntoIterator<Item = (String, u32, Option<IpAddr>)>,
{
    let mut out: Vec<InterfaceSnapshot> = Vec::new();
    for (name, flags, address) in entries {
        match out.iter_mut().find(|existing| existing.name == name) {
            Some(slot) => {
                slot.flags |= flags;
                if let Some(address) = address {
                    slot.addresses.push(address);
                }
            }
            None => out.push(InterfaceSnapshot {
                name,
                flags,
                addresses: address.into_iter().collect(),
            }),
        }
    }
    out
}

/// 一条 `sockaddr` → `IpAddr`。`ifa_addr` 为空或地址族不是 v4/v6 时返回 `None`，
/// 绝不把其他族的字节硬转成 IP。
#[cfg(unix)]
fn address_from_sockaddr(address: *const libc::sockaddr) -> Option<IpAddr> {
    if address.is_null() {
        return None;
    }
    // SAFETY: 非空时 `sa_family` 是 sockaddr 的公共首字段。
    let family = unsafe { (*address).sa_family as i32 };
    match family {
        libc::AF_INET => {
            // SAFETY: 族已确认是 AF_INET，可按 sockaddr_in 解释同一块内存。
            let addr = unsafe { &*(address as *const libc::sockaddr_in) };
            // `sin_addr.s_addr` 是网络字节序：先按内存字节还原成整数，
            // 再交给 `Ipv4Addr::from` 取那四个字节。
            Some(IpAddr::V4(Ipv4Addr::from(u32::from_be(
                addr.sin_addr.s_addr,
            ))))
        }
        libc::AF_INET6 => {
            // SAFETY: 族已确认是 AF_INET6，可按 sockaddr_in6 解释。
            let addr = unsafe { &*(address as *const libc::sockaddr_in6) };
            Some(IpAddr::V6(Ipv6Addr::from(addr.sin6_addr.s6_addr)))
        }
        _ => None,
    }
}

/// `getifaddrs` 链表头的持有者：任何返回路径都只释放一次。
///
/// 为什么不用裸 `freeifaddrs` 调用：遍历里有多个提前返回点，手写释放必然漏
/// 一条。RAII 把「释放」绑在作用域上，包括 `getifaddrs` 成功但后续判定失败
/// 的分支。
#[cfg(unix)]
struct IfaddrsHead {
    head: *mut libc::ifaddrs,
    release: fn(*mut libc::ifaddrs),
}

#[cfg(unix)]
impl IfaddrsHead {
    fn from_getifaddrs(head: *mut libc::ifaddrs) -> Self {
        Self {
            head,
            // 非捕获闭包可转成 fn 指针，Drop 里无需 unsafe 块。
            release: |head| unsafe { libc::freeifaddrs(head) },
        }
    }

    /// 测试用：注入替代释放函数，避免伪造一个会被真实 `freeifaddrs` 释放的
    /// 地址（那会直接崩溃或破坏堆）。
    #[cfg(test)]
    fn with_release(head: *mut libc::ifaddrs, release: fn(*mut libc::ifaddrs)) -> Self {
        Self { head, release }
    }

    fn head(&self) -> *mut libc::ifaddrs {
        self.head
    }
}

#[cfg(unix)]
impl Drop for IfaddrsHead {
    fn drop(&mut self) {
        (self.release)(self.head);
    }
}

#[cfg(unix)]
fn snapshot_from_head(head: *mut libc::ifaddrs) -> Vec<InterfaceSnapshot> {
    use std::ffi::CStr;

    let mut entries: Vec<(String, u32, Option<IpAddr>)> = Vec::new();
    let mut cursor = head;
    while !cursor.is_null() {
        // SAFETY: `cursor` 是 getifaddrs 链表内的非空节点。
        let entry = unsafe { &*cursor };
        if !entry.ifa_name.is_null() {
            // SAFETY: `ifa_name` 是 NUL 结尾字符串。
            let name = unsafe { CStr::from_ptr(entry.ifa_name) }
                .to_string_lossy()
                .into_owned();
            entries.push((name, entry.ifa_flags, address_from_sockaddr(entry.ifa_addr)));
        }
        cursor = entry.ifa_next;
    }
    aggregate(entries)
}

/// 一次被动采样。`None` = 没有可信快照（FFI 失败或空链表）。
#[cfg(unix)]
fn collect_interfaces() -> Option<Vec<InterfaceSnapshot>> {
    let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: `getifaddrs` 成功时把链表头写进 `head`，失败时该指针不可用。
    let rc = unsafe { libc::getifaddrs(&mut head) };
    if head.is_null() {
        return None;
    }
    // 先接管所有权再判 rc：即使失败路径也由 Drop 释放，不漏。
    let owned = IfaddrsHead::from_getifaddrs(head);
    if rc != 0 {
        return None;
    }
    Some(snapshot_from_head(owned.head()))
}

#[cfg(not(unix))]
fn collect_interfaces() -> Option<Vec<InterfaceSnapshot>> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::IpAddr;

    const UP: u32 = libc::IFF_UP as u32;
    const RUNNING: u32 = libc::IFF_RUNNING as u32;

    fn interface(name: &str, flags: u32, addresses: &[&str]) -> InterfaceSnapshot {
        InterfaceSnapshot {
            name: name.to_string(),
            flags,
            addresses: addresses
                .iter()
                .map(|address| address.parse::<IpAddr>().expect("test address"))
                .collect(),
        }
    }

    fn state_of(name: &str, flags: u32, addresses: &[&str]) -> NetworkState {
        network_state(&[interface(name, flags, addresses)])
    }

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

    // ---- N1/N2/N3/N5：纯资格判定的表驱动覆盖 ----------------------------

    #[test]
    fn sleep_hardening_ipv6_only_is_online() {
        let interfaces = vec![InterfaceSnapshot {
            name: "en0".into(),
            flags: (libc::IFF_UP | libc::IFF_RUNNING) as u32,
            addresses: vec!["2001:db8::1234".parse().unwrap()],
        }];
        assert_eq!(network_state(&interfaces), NetworkState::Online);
    }

    #[test]
    fn sleep_hardening_successful_empty_snapshot_is_offline() {
        assert_eq!(network_state(&[]), NetworkState::Offline);
        assert!(NetworkState::Unknown.eligible());
        assert!(!NetworkState::Offline.eligible());
        assert!(NetworkState::Online.eligible());
    }

    /// v4 公网/私网、v6 ULA 各自单独出现都算在线（N1/N2）。
    #[test]
    fn sleep_hardening_usable_single_addresses_are_online() {
        for address in ["192.0.2.1", "10.0.0.1", "2001:db8::1234", "fd00::1"] {
            assert_eq!(
                state_of("en0", UP | RUNNING, &[address]),
                NetworkState::Online,
                "{address} alone must be online"
            );
        }
    }

    /// unspecified / loopback / multicast / link-local / broadcast 都不提供资格（N3）。
    #[test]
    fn sleep_hardening_unusable_addresses_do_not_qualify() {
        for address in [
            "0.0.0.0",
            "127.0.0.1",
            "169.254.1.1",
            "224.0.0.1",
            "255.255.255.255",
            "::",
            "::1",
            "fe80::1",
            "ff02::1",
        ] {
            assert_eq!(
                state_of("en0", UP | RUNNING, &[address]),
                NetworkState::Offline,
                "{address} alone must not qualify"
            );
        }
    }

    /// DOWN、只有 UP、loopback 标志、虚拟/旁路接口都不合格（N4）。
    #[test]
    fn sleep_hardening_interfaces_without_a_live_link_are_offline() {
        let cases: [(&str, u32); 4] = [
            ("en0", 0),
            ("en0", UP),
            ("lo0", UP | RUNNING | 0x8),
            ("utun4", UP | RUNNING),
        ];
        for (name, flags) in cases {
            assert_eq!(
                state_of(name, flags, &["192.0.2.1"]),
                NetworkState::Offline,
                "{name} with flags {flags:#x} must be offline"
            );
        }
        for name in ["awdl0", "llw0", "bridge0"] {
            assert_eq!(
                state_of(name, UP | RUNNING, &["192.0.2.1"]),
                NetworkState::Offline,
                "{name} must not be the sole candidate"
            );
        }
    }

    /// 一个接口不合格不影响另一个合格接口。
    #[test]
    fn sleep_hardening_one_qualifying_interface_is_enough() {
        let interfaces = vec![
            interface("utun4", UP | RUNNING, &["10.8.0.2"]),
            interface("en0", UP | RUNNING, &["fe80::1"]),
            interface("en1", UP | RUNNING, &["fd00::1"]),
        ];
        assert_eq!(network_state(&interfaces), NetworkState::Online);
    }

    // ---- N7：地址解析与聚合 ---------------------------------------------

    #[cfg(unix)]
    #[test]
    fn sleep_hardening_sockaddr_in_bytes_round_trip() {
        let mut raw: libc::sockaddr_in = unsafe { std::mem::zeroed() };
        raw.sin_family = libc::AF_INET as libc::sa_family_t;
        // 直接按内存字节写，断言解析结果与这些字节逐字一致。
        raw.sin_addr.s_addr = u32::from_ne_bytes([192, 0, 2, 1]);
        let parsed =
            address_from_sockaddr(&raw as *const libc::sockaddr_in as *const libc::sockaddr);
        assert_eq!(parsed, Some(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1))));
    }

    #[cfg(unix)]
    #[test]
    fn sleep_hardening_sockaddr_in6_bytes_round_trip() {
        let mut raw: libc::sockaddr_in6 = unsafe { std::mem::zeroed() };
        raw.sin6_family = libc::AF_INET6 as libc::sa_family_t;
        let bytes: [u8; 16] = [
            0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x12, 0x34,
        ];
        raw.sin6_addr.s6_addr = bytes;
        let parsed =
            address_from_sockaddr(&raw as *const libc::sockaddr_in6 as *const libc::sockaddr);
        assert_eq!(parsed, Some(IpAddr::V6(Ipv6Addr::from(bytes))));
    }

    #[cfg(unix)]
    #[test]
    fn sleep_hardening_null_and_foreign_families_are_skipped() {
        assert_eq!(address_from_sockaddr(std::ptr::null()), None);
        // 一个别的地址族：不得被硬转成 IP。
        let mut other: libc::sockaddr = unsafe { std::mem::zeroed() };
        other.sa_family = libc::AF_UNIX as libc::sa_family_t;
        assert_eq!(address_from_sockaddr(&other as *const libc::sockaddr), None);
    }

    /// 同一接口的两条节点（v4 + v6）必须都保留；无名节点不进快照。
    #[test]
    fn sleep_hardening_same_interface_keeps_both_families() {
        let v4: IpAddr = "192.0.2.1".parse().unwrap();
        let v6: IpAddr = "2001:db8::1234".parse().unwrap();
        let snapshot = aggregate(vec![
            ("en0".to_string(), UP, Some(v4)),
            ("en0".to_string(), RUNNING, Some(v6)),
        ]);
        assert_eq!(snapshot.len(), 1, "one interface, aggregated");
        assert_eq!(snapshot[0].name, "en0");
        assert_eq!(snapshot[0].flags, UP | RUNNING, "flags are unioned");
        assert_eq!(snapshot[0].addresses, vec![v4, v6]);
    }

    #[test]
    fn sleep_hardening_addressless_nodes_stay_in_the_snapshot() {
        let snapshot = aggregate(vec![
            ("en0".to_string(), UP | RUNNING, None),
            (
                "en0".to_string(),
                UP | RUNNING,
                Some("10.0.0.1".parse().unwrap()),
            ),
        ]);
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].addresses.len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn sleep_hardening_snapshot_holder_releases_exactly_once() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static RELEASES: AtomicUsize = AtomicUsize::new(0);

        // 计数释放函数：绝不交给真实 `freeifaddrs`，所以可以安全地配一个
        // 永远不会被解引用的伪造指针。
        fn count_release(_: *mut libc::ifaddrs) {
            RELEASES.fetch_add(1, Ordering::SeqCst);
        }

        RELEASES.store(0, Ordering::SeqCst);
        {
            // 伪造指针只被传给注入的计数函数，从未解引用或交给真实
            // `freeifaddrs`；用 `NonNull::dangling` 而不是整数转型，避免
            // 手搓悬垂指针（clippy::manual_dangling_ptr）。
            let fake = std::ptr::NonNull::<libc::ifaddrs>::dangling().as_ptr();
            let holder = IfaddrsHead::with_release(fake, count_release);
            assert_eq!(holder.head(), fake);
        }
        assert_eq!(RELEASES.load(Ordering::SeqCst), 1, "drop releases once");
    }

    #[test]
    fn sleep_hardening_unknown_is_not_offline() {
        assert_ne!(NetworkState::Unknown, NetworkState::Offline);
        assert!(NetworkState::Unknown.eligible());
    }

    // ---- N6：不存在主动探测 ---------------------------------------------

    #[test]
    fn the_system_probes_run_without_panicking() {
        let power = SystemPowerProbe::new();
        let _ = power.on_ac();
        let net = SystemNetProbe::new();
        // 真实采样：本机至少要么给出快照（Online/Offline），要么 Unknown。
        let _ = net.state();
    }
}
