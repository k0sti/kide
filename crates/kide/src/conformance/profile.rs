//! The `Profile` suite (§4.1): every walk follows the same links, so a
//! profile names its root the same way every time and gives an object's
//! links the same way every time; bytes that are not one of its objects,
//! such as a cut-off object, are `Invalid`, never a panic or another
//! error class (§4.4).

use crate::error::Error;
use crate::iface::Profile;

/// Checks `p` against `objects`, some of its own valid objects.
pub fn profile(p: &dyn Profile, objects: &[Vec<u8>]) {
    assert_eq!(p.root(), p.root(), "profile: the root's want changes");
    for (i, bytes) in objects.iter().enumerate() {
        let links = p
            .children(bytes)
            .unwrap_or_else(|e| panic!("profile: object {i} is refused: {e}"));
        let again = p.children(bytes).expect("profile: a second call fails");
        assert_eq!(links, again, "profile: object {i} links differently");
        for cut in [0, 1, bytes.len() / 2, bytes.len().saturating_sub(1)] {
            if cut >= bytes.len() {
                continue;
            }
            match p.children(&bytes[..cut]) {
                Ok(_) | Err(Error::Invalid(_)) => {}
                Err(e) => panic!("profile: object {i} cut to {cut} bytes: {e:?}, not Invalid"),
            }
        }
    }
    for junk in [&b""[..], b"\xff", &[0u8; 100]] {
        match p.children(junk) {
            Ok(_) | Err(Error::Invalid(_)) => {}
            Err(e) => panic!("profile: {} junk bytes: {e:?}, not Invalid", junk.len()),
        }
    }
}
