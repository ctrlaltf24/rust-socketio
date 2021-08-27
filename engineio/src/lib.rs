//! rust_engineio is a engine.io client written in the Rust Programming Language.
//! ## Example usage
//!
//TODO: include me
//! ``` rust
//!
//! ```
//!
//! The main entry point for using this crate is the [`SocketBuilder`] which provides
//! a way to easily configure a socket in the needed way. When the `connect` method
//! is called on the builder, it returns a connected client which then could be used
//! to emit messages to certain events. One client can only be connected to one namespace.
//! If you need to listen to the messages in different namespaces you need to
//! allocate multiple sockets.
//!
//! ## Current features
//!
//! This implementation now supports all of the features of the socket.io protocol mentioned
//! [here](https://github.com/socketio/socket.io-protocol).
//! It generally tries to make use of websockets as often as possible. This means most times
//! only the opening request uses http and as soon as the server mentions that he is able to use
//! websockets, an upgrade  is performed. But if this upgrade is not successful or the server
//! does not mention an upgrade possibility, http-long polling is used (as specified in the protocol specs).
//!
//! Here's an overview of possible use-cases:
//!
//! - connecting to a server.
//! - register callbacks for the following event types:
//!     - open
//!     - close
//!     - error
//!     - message
//! - send JSON data to the server (via `serde_json` which provides safe
//! handling).
//! - send and handle Binary data.
//!
#![allow(clippy::rc_buffer)]
#![warn(clippy::complexity)]
#![warn(clippy::style)]
#![warn(clippy::perf)]
#![warn(clippy::correctness)]
/// A small macro that spawns a scoped thread. Used for calling the callback
/// functions.
macro_rules! spawn_scoped {
    ($e:expr) => {
        crossbeam_utils::thread::scope(|s| {
            s.spawn(|_| $e);
        })
        .unwrap();
    };
}

pub mod client;
/// Generic header map
pub mod header;
pub mod packet;
pub(self) mod socket;
pub mod transport;
pub mod transports;

/// Contains the error type which will be returned with every result in this
/// crate. Handles all kinds of errors.
pub mod error;

pub use client::{Socket, SocketBuilder};
pub use error::Error;
pub use packet::{Packet, PacketId};

#[cfg(test)]
pub(crate) mod test {
    use super::*;
    use native_tls::TlsConnector;
    const CERT_PATH: &str = "../ci/cert/ca.crt";
    use native_tls::Certificate;
    use std::fs::File;
    use std::io::Read;

    pub(crate) fn tls_connector() -> error::Result<TlsConnector> {
        let cert_path = std::env::var("CA_CERT_PATH").unwrap_or_else(|_| CERT_PATH.to_owned());
        let mut cert_file = File::open(cert_path)?;
        let mut buf = vec![];
        cert_file.read_to_end(&mut buf)?;
        let cert: Certificate = Certificate::from_pem(&buf[..]).unwrap();
        Ok(TlsConnector::builder()
            // ONLY USE FOR TESTING!
            .danger_accept_invalid_hostnames(true)
            .add_root_certificate(cert)
            .build()
            .unwrap())
    }
    /// The `engine.io` server for testing runs on port 4201
    const SERVER_URL: &str = "http://localhost:4201";
    const SERVER_URL_SECURE: &str = "https://localhost:4202";
    use url::Url;

    pub(crate) fn engine_io_server() -> crate::error::Result<Url> {
        let url = std::env::var("ENGINE_IO_SERVER").unwrap_or_else(|_| SERVER_URL.to_owned());
        Ok(Url::parse(&url)?)
    }

    pub(crate) fn engine_io_server_secure() -> crate::error::Result<Url> {
        let url = std::env::var("ENGINE_IO_SECURE_SERVER")
            .unwrap_or_else(|_| SERVER_URL_SECURE.to_owned());
        Ok(Url::parse(&url)?)
    }
}