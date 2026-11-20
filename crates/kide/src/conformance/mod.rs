//! Conformance suites: one public function per interface, taking a factory and
//! panicking with a clear message at the first failed check. Suites depend
//! only on the format and the interfaces. Also the shared test support: the
//! seeded PRNG, temporary directories and the §8 inputs. Enabled by
//! `cfg(test)` or the `conformance` feature.

#[cfg(unix)]
mod change_source;
mod live;
mod object_store;
mod plain;
mod profile;
mod resolver;
mod transport;
#[cfg(unix)]
mod workspace;

#[cfg(unix)]
pub use change_source::{ChangeFixture, change_source};
pub use live::{echo, live};
pub use object_store::{object_store, object_store_read};
pub use plain::Plain;
pub use profile::profile;
pub use resolver::resolver;
pub use transport::{MakePeer, TransportFixture, transport};
#[cfg(unix)]
pub use workspace::{WorkspaceFixture, workspace};

/// Whether the heavy tests run: `KIDE_FULL=1`, which `ci.sh` sets.
pub fn full_tests() -> bool {
    std::env::var("KIDE_FULL").is_ok_and(|v| v == "1")
}

/// SplitMix64, seeded: reproducible test data without a dependency.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Rng {
        Rng(seed)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// Uniform enough in `0..n`; `n` must be positive.
    pub fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }

    pub fn bytes(&mut self, len: usize) -> Vec<u8> {
        let mut v = Vec::with_capacity(len + 8);
        while v.len() < len {
            v.extend(self.next_u64().to_le_bytes());
        }
        v.truncate(len);
        v
    }
}

/// A uniquely named directory under `target/tmp` of this crate, removed on drop.
pub struct TempDir(std::path::PathBuf);

impl TempDir {
    pub fn new(tag: &str) -> TempDir {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let name = format!("kide-{tag}-{}-{n}", std::process::id());
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target/tmp")
            .join(name);
        let _ = std::fs::remove_dir_all(&path); // left by a killed run
        std::fs::create_dir_all(&path).expect("cannot create a temporary directory");
        TempDir(path)
    }

    pub fn path(&self) -> &std::path::Path {
        &self.0
    }

    /// A new, empty subdirectory with a unique name.
    pub fn subdir(&self) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let p = self
            .0
            .join(format!("d{}", N.fetch_add(1, Ordering::Relaxed)));
        std::fs::create_dir_all(&p).expect("cannot create a temporary directory");
        p
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Input T (§8): 3,000 numbered lines of repeated words, 128,928 bytes.
pub fn input_t() -> Vec<u8> {
    (0..3000)
        .map(|i| format!("{i:05} {}\n", "lorem ipsum ".repeat(i % 7)))
        .collect::<String>()
        .into_bytes()
}

/// Input B (§8): 262,144 pseudo-random bytes.
pub fn input_b() -> Vec<u8> {
    (0u32..8192)
        .flat_map(|i| crate::hash::h(&i.to_le_bytes()).0)
        .collect()
}

/// Input L (§8): one line of 148,890 bytes, without a newline.
pub fn input_l() -> Vec<u8> {
    (0..6000)
        .map(|i| format!("{{\"id\":{i},\"tag\":\"{}\"}},", "ab".repeat(i % 5)))
        .collect::<String>()
        .into_bytes()
}

/// Input Z (§8): 1,048,576 zero bytes, all cuts forced.
pub fn input_z() -> Vec<u8> {
    vec![0; 1 << 20]
}
