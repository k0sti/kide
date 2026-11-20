//! The host rule for the device's own FIPS address (§4.2), which Kide
//! relies on but cannot enforce: the warning a FIPS server gives at start.

use std::net::Ipv6Addr;

/// The rule of §4.2.
const RULE: &str = "the host must drop packets addressed to its own FIPS \
    address that arrive on any interface other than the FIPS one, loopback \
    included; without it, a packet with a forged FIPS source address could \
    arrive over another network";

/// The start-up warning of a FIPS server listening on `addr`, with an
/// nftables rule for it. Reading the firewall needs privileges (`nft list
/// ruleset` needs CAP_NET_ADMIN), so the rule is never confirmed and the
/// warning always given; it names the interface that holds `addr`, from
/// `/proc/net/if_inet6`.
pub fn warning(addr: &Ipv6Addr) -> String {
    let table = std::fs::read_to_string("/proc/net/if_inet6").unwrap_or_default();
    let (iface, place) = match interface_in(&table, addr) {
        Some(i) => (i.clone(), format!("{addr} is on {i}")),
        None => (
            "fips0".into(),
            format!("{addr} is on no interface; is FIPS running?"),
        ),
    };
    format!(
        "warning: FIPS is experimental, pending a two-machine run (C7). Kide cannot check this: {RULE} ({place}). With nftables:\n{}",
        nft_rule(addr, &iface)
    )
}

/// The nftables rule of §4.2 for the address `addr` on the interface `iface`.
fn nft_rule(addr: &Ipv6Addr, iface: &str) -> String {
    format!(
        "table inet kide {{\n    chain input {{\n        \
         type filter hook input priority filter; policy accept;\n        \
         ip6 daddr {addr} iifname != \"{iface}\" drop\n    }}\n}}"
    )
}

/// The interface that holds `addr`, from the text of `/proc/net/if_inet6`:
/// per line, the address in 32 hex digits, four fields, the name.
fn interface_in(table: &str, addr: &Ipv6Addr) -> Option<String> {
    let hex: String = addr.octets().iter().map(|b| format!("{b:02x}")).collect();
    table.lines().find_map(|l| {
        let f: Vec<&str> = l.split_whitespace().collect();
        (f.len() == 6 && f[0] == hex).then(|| f[5].to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_interface_of_an_address() {
        let table = "00000000000000000000000000000001 01 80 10 80       lo\n\
                     fd74534fb5959a5d89cd740363ba2737 05 80 00 80     fips0\n";
        let a: Ipv6Addr = "fd74:534f:b595:9a5d:89cd:7403:63ba:2737".parse().unwrap();
        assert_eq!(interface_in(table, &a).as_deref(), Some("fips0"));
        assert_eq!(
            interface_in(table, &Ipv6Addr::LOCALHOST).as_deref(),
            Some("lo")
        );
        assert_eq!(interface_in(table, &"fd00::1".parse().unwrap()), None);
        assert_eq!(interface_in("garbage", &a), None);
        let w = warning(&"fd00::1".parse().unwrap());
        assert!(w.contains("addressed to its own FIPS address"), "{w}");
        assert!(
            w.contains("ip6 daddr fd00::1 iifname != \"fips0\" drop"),
            "{w}"
        );
        assert_eq!(
            nft_rule(&a, "fips0"),
            "table inet kide {\n    chain input {\n        \
             type filter hook input priority filter; policy accept;\n        \
             ip6 daddr fd74:534f:b595:9a5d:89cd:7403:63ba:2737 iifname != \"fips0\" drop\n    \
             }\n}"
        );
    }
}
