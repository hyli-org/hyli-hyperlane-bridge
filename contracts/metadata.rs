#[allow(unused)]
#[cfg(all(not(clippy), feature = "nonreproducible"))]
mod methods {
    include!(concat!(env!("OUT_DIR"), "/methods.rs"));
}

#[cfg(all(not(clippy), feature = "nonreproducible", feature = "all"))]
mod metadata {
    pub const HYPERLANE_BRIDGE_ELF: &[u8] = crate::methods::HYPERLANE_BRIDGE_ELF;
    pub const HYPERLANE_BRIDGE_ID: [u8; 32] =
        sdk::to_u8_array(&crate::methods::HYPERLANE_BRIDGE_ID);
}

#[cfg(any(clippy, not(feature = "nonreproducible")))]
mod metadata {
    pub const HYPERLANE_BRIDGE_ELF: &[u8] =
        hyperlane_bridge::client::tx_executor_handler::metadata::HYPERLANE_BRIDGE_ELF;
    pub const HYPERLANE_BRIDGE_ID: [u8; 32] =
        hyperlane_bridge::client::tx_executor_handler::metadata::HYPERLANE_BRIDGE_PROGRAM_ID;
}

pub use metadata::*;
