// SPDX-FileCopyrightText: 2025-2026 Lasse Heitgres
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional terms: see NOTICE

//! Owned transport handoff. Authentication, AP readiness, arbitration, and session lifetime
//! belong to consumers, not this envelope. No cloning or hidden relay task.

use std::io;
use std::os::fd::OwnedFd;
use tokio::net::UnixStream;

pub struct Arrival<S> {
    pub peer: String,
    /// Local address in display/network byte order.
    pub local: [u8; 6],
    pub stream: S,
}

pub type StreamArrival = Arrival<UnixStream>;

impl Arrival<OwnedFd> {
    /// Conversion happens at the consumer's existing nonblocking-I/O boundary.
    pub fn into_stream(self) -> io::Result<StreamArrival> {
        let stream = std::os::unix::net::UnixStream::from(self.stream);
        stream.set_nonblocking(true)?;
        Ok(Arrival {
            peer: self.peer,
            local: self.local,
            stream: UnixStream::from_std(stream)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn fd_conversion_preserves_metadata_and_bidirectional_transport() {
        let (stream, remote) = std::os::unix::net::UnixStream::pair().unwrap();
        remote.set_nonblocking(true).unwrap();
        let mut remote = UnixStream::from_std(remote).unwrap();
        let mut arrival = Arrival {
            peer: "aa:BB".into(),
            local: [1, 2, 3, 4, 5, 6],
            stream: OwnedFd::from(stream),
        }.into_stream().unwrap();
        assert_eq!(arrival.peer, "aa:BB");
        assert_eq!(arrival.local, [1, 2, 3, 4, 5, 6]);
        arrival.stream.write_all(b"a").await.unwrap();
        assert_eq!(remote.read_u8().await.unwrap(), b'a');
        remote.write_all(b"b").await.unwrap();
        assert_eq!(arrival.stream.read_u8().await.unwrap(), b'b');
        drop(arrival);
        assert_eq!(remote.read(&mut [0]).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn failed_delivery_drops_the_owned_transport() {
        let (stream, mut remote) = UnixStream::pair().unwrap();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        drop(rx);
        let rejected = tx.send(Arrival {
            peer: "phone".into(), local: [0; 6], stream,
        });
        assert!(rejected.is_err());
        drop(rejected);
        assert_eq!(remote.read(&mut [0]).await.unwrap(), 0);
    }
}
