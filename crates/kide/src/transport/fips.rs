//! FIPS addresses (§4.2): `0xfd ‖ H(P)[0..15]` for the x-only public key P.
//! The host rule the FIPS transport relies on is the `kide` program's to
//! warn about.

use crate::hash::h;
use std::net::Ipv6Addr;

/// The FIPS address of the 32-byte x-only public key `key`.
pub fn address(key: &[u8; 32]) -> Ipv6Addr {
    let mut a = [0xfd; 16];
    a[1..].copy_from_slice(&h(key).0[..15]);
    Ipv6Addr::from(a)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash::Hash;

    #[test]
    fn known_address() {
        // The key of the NIP-19 example (npub10elfcs4fr0l0r8af98jlmgdh9c8tcxjvz9qkw038js35mp4dma8qzvjptg).
        // Computed independently with Python:
        //   ipaddress.IPv6Address(b"\xfd" + hashlib.sha256(bytes.fromhex(key)).digest()[:15])
        let key = "7e7e9c42a91bfef19fa929e5fda1b72e0ebc1a4c1141673e2794234d86addf4e";
        let key = Hash::from_hex(key).unwrap().0;
        let want: Ipv6Addr = "fd74:534f:b595:9a5d:89cd:7403:63ba:2737".parse().unwrap();
        assert_eq!(address(&key), want);
    }
}
