// SPDX-License-Identifier: GPL-3.0-or-later

use super::*;
use crate::config::fixture;
use std::io::{Read, Write};

mod r1_batch1 {
    include!("r1_batch1_tests.rs");
}

fn arrival(tx: &mut Channel, generation: u64) -> UnixStream {
    let (stream, phone) = UnixStream::pair().unwrap();
    phone
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    tx.send(Message::Arrival {
        generation,
        peer: fixture().phone(),
        local: fixture().phone(),
        stream: stream.into(),
    })
    .unwrap();
    phone
}

fn adopted(rx: &mut Channel) -> OwnedFd {
    let Some(Message::Arrival { stream, .. }) = rx.receive().unwrap() else {
        panic!("expected one adopted descriptor")
    };
    stream
}

fn ack(generation: u64, transfer: u64) -> Vec<u8> {
    let mut bytes = b"LNH1".to_vec();
    bytes.extend_from_slice(&[4, 0, 0, 0]);
    bytes.extend_from_slice(&generation.to_be_bytes());
    bytes.extend_from_slice(&8u32.to_be_bytes());
    bytes.extend_from_slice(&[0; 12]);
    bytes.extend_from_slice(&transfer.to_be_bytes());
    bytes
}

fn closed(mut phone: UnixStream) {
    assert_eq!(phone.read(&mut [0]).unwrap(), 0);
}

#[test]
fn ownership_is_retained_until_exact_ack_and_transport_is_bidirectional() {
    let (mut tx, mut rx) = super::tests::pair();
    let mut phone = arrival(&mut tx, 7);
    let pending = tx.pending.as_ref().unwrap();
    let mut original = std::mem::MaybeUninit::<libc::stat>::uninit();
    assert_eq!(
        unsafe { libc::fstat(pending.stream.as_raw_fd(), original.as_mut_ptr()) },
        0
    );
    assert!(!tx.poll_adoption().unwrap());
    let received = adopted(&mut rx);
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    assert_eq!(
        unsafe { libc::fstat(received.as_raw_fd(), stat.as_mut_ptr()) },
        0
    );
    let (original, stat) = unsafe { (original.assume_init(), stat.assume_init()) };
    assert_eq!(
        (original.st_dev, original.st_ino),
        (stat.st_dev, stat.st_ino)
    );
    assert!(tx.pending.is_some());
    assert!(tx.poll_adoption().unwrap());
    assert!(tx.pending.is_none());
    let mut received = UnixStream::from(received);
    received.write_all(b"offer").unwrap();
    let mut offer = [0; 5];
    phone.read_exact(&mut offer).unwrap();
    assert_eq!(&offer, b"offer");
    phone.write_all(b"reply").unwrap();
    received.read_exact(&mut offer).unwrap();
    assert_eq!(&offer, b"reply");
    // Successful ACK release must not shut down the receiver's descriptor.
    drop(tx);
    phone.write_all(b"x").unwrap();
    assert_eq!(received.read(&mut offer).unwrap(), 1);
    drop(received);
    closed(phone);
}

#[test]
fn missing_ack_deadline_revokes_received_alias_and_poison_is_terminal() {
    let (mut tx, mut rx) = super::tests::pair();
    let phone = arrival(&mut tx, 3);
    let received = adopted(&mut rx);
    // Simulate a lost ACK by consuming it without acknowledging in Channel.
    let mut bytes = [0; 40];
    tx.stream.as_mut().unwrap().read_exact(&mut bytes).unwrap();
    assert_eq!(bytes.to_vec(), ack(3, 1));
    tx.pending.as_mut().unwrap().since = Instant::now() - ADOPTION_TIMEOUT;
    assert_eq!(tx.poll_adoption(), Err(Error::TimedOut));
    assert!(tx.pending.is_none());
    assert!(tx.stream.is_none());
    closed(phone);
    assert_eq!(tx.poll_adoption(), Err(Error::Disconnected));
    drop(received);
}

#[test]
fn unadopted_timeout_and_drop_release_queued_descriptors() {
    for timeout in [false, true] {
        let (mut tx, rx) = super::tests::pair();
        let phone = arrival(&mut tx, 1);
        if timeout {
            tx.pending.as_mut().unwrap().since = Instant::now() - ADOPTION_TIMEOUT;
            assert_eq!(tx.poll_adoption(), Err(Error::TimedOut));
        }
        drop(tx);
        drop(rx);
        closed(phone);
    }
}

#[test]
fn stale_cancel_keeps_current_transfer_matching_cancel_revokes_it() {
    let (mut tx, mut rx) = super::tests::pair();
    let phone = arrival(&mut tx, 2);
    let received = adopted(&mut rx);
    tx.send(Message::Cancel { generation: 1 }).unwrap();
    assert!(tx.pending.is_some());
    tx.send(Message::Cancel { generation: 2 }).unwrap();
    assert!(tx.pending.is_none());
    closed(phone);
    drop(received);
    assert!(matches!(
        rx.receive().unwrap(),
        Some(Message::Cancel { generation: 1 })
    ));
    assert!(matches!(
        rx.receive().unwrap(),
        Some(Message::Cancel { generation: 2 })
    ));
}

#[test]
fn a_second_arrival_cannot_replace_the_pending_owner() {
    let (mut tx, mut rx) = super::tests::pair();
    let first = arrival(&mut tx, 1);
    let (stream, second) = UnixStream::pair().unwrap();
    assert_eq!(
        tx.send(Message::Arrival {
            generation: 2,
            peer: fixture().phone(),
            local: fixture().phone(),
            stream: stream.into(),
        }),
        Err(Error::Backpressure)
    );
    closed(second);
    assert_eq!(tx.pending.as_ref().unwrap().generation, 1);
    let received = adopted(&mut rx);
    assert!(tx.poll_adoption().unwrap());
    drop(received);
    closed(first);
}

#[test]
fn stale_generation_or_transfer_ack_never_releases_a_new_owner() {
    for (generation, transfer) in [(1, 1), (2, 2), (2, 0)] {
        let (mut tx, mut rx) = super::tests::pair();
        let phone = arrival(&mut tx, 2);
        rx.stream
            .as_mut()
            .unwrap()
            .write_all(&ack(generation, transfer))
            .unwrap();
        assert_eq!(tx.poll_adoption(), Err(Error::Stale));
        assert!(tx.pending.is_none());
        closed(phone);
    }
}

#[test]
fn fragmented_ack_retains_ownership_and_obeys_assembly_deadline() {
    for expire in [false, true] {
        let (mut tx, mut rx) = super::tests::pair();
        let phone = arrival(&mut tx, 2);
        let bytes = ack(2, 1);
        rx.stream.as_mut().unwrap().write_all(&bytes[..9]).unwrap();
        assert!(!tx.poll_adoption().unwrap());
        assert!(tx.pending.is_some());
        if expire {
            tx.started = Some(Instant::now() - ASSEMBLY_TIMEOUT);
            assert_eq!(tx.poll_adoption(), Err(Error::TimedOut));
        } else {
            // Adopt the queued descriptor before completing the synthetic ACK.
            let received = adopted(&mut rx);
            // adopted() emitted a real ACK; the partial synthetic frame cannot
            // be mixed with it. Consume that real ACK on the raw sender side.
            let mut real = [0; 40];
            tx.stream.as_mut().unwrap().read_exact(&mut real).unwrap();
            assert_eq!(real.to_vec(), bytes);
            rx.stream.as_mut().unwrap().write_all(&bytes[9..]).unwrap();
            assert!(tx.poll_adoption().unwrap());
            drop(received);
        }
        closed(phone);
    }
}

#[test]
fn peer_disconnect_before_ack_revokes_receiver_alias() {
    let (mut tx, mut rx) = super::tests::pair();
    let phone = arrival(&mut tx, 4);
    let received = adopted(&mut rx);
    // Remove queued ACK first, then observe the disconnect with an ACK pending.
    let mut bytes = [0; 40];
    tx.stream.as_mut().unwrap().read_exact(&mut bytes).unwrap();
    drop(rx);
    assert_eq!(tx.poll_adoption(), Err(Error::Disconnected));
    closed(phone);
    drop(received);
}

#[test]
fn receiver_ack_write_failure_returns_no_descriptor() {
    let (mut tx, mut rx) = super::tests::pair();
    let phone = arrival(&mut tx, 1);
    // Fill even the last byte of the nonblocking ACK direction. SHUT_RD on
    // the peer does not reliably fail a subsequent Darwin Unix socket write.
    let mut full = false;
    for _ in 0..100_000 {
        match rx.stream.as_mut().unwrap().write(&[0]) {
            Ok(1) => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                full = true;
                break;
            }
            other => panic!("unexpected buffer-fill result: {other:?}"),
        }
    }
    assert!(full, "fault injection must reach ACK backpressure");
    assert_eq!(rx.receive().err(), Some(Error::Backpressure));
    assert!(rx.poisoned);
    assert!(rx.rights.is_empty());
    drop(tx);
    closed(phone);
}

#[test]
fn transfer_ids_do_not_repeat_for_reused_bluetooth_generation() {
    let (mut tx, mut rx) = super::tests::pair();
    for transfer in 1..=3 {
        let phone = arrival(&mut tx, 5);
        assert_eq!(tx.pending.as_ref().unwrap().transfer, transfer);
        let received = adopted(&mut rx);
        assert!(tx.poll_adoption().unwrap());
        drop(received);
        closed(phone);
    }
    rx.stream.as_mut().unwrap().write_all(&ack(5, 1)).unwrap();
    assert_eq!(tx.receive().err(), Some(Error::Stale));
}

#[test]
fn replayed_or_zero_arrival_id_is_never_acknowledged() {
    for transfer in [0u64, 1] {
        let (mut tx, mut rx) = super::tests::pair();
        let phone = arrival(&mut tx, 5);
        let received = adopted(&mut rx);
        assert!(tx.poll_adoption().unwrap());
        drop(received);
        closed(phone);
        let (extra, extra_phone) = UnixStream::pair().unwrap();
        let mut frame = ack(5, transfer);
        frame[4] = 2;
        frame[20..26].copy_from_slice(&fixture().phone().0);
        frame[26..32].copy_from_slice(&fixture().phone().0);
        super::tests::raw_rights(tx.stream.as_ref().unwrap(), &mut frame, &[extra.into()]);
        assert_eq!(rx.receive().err(), Some(Error::Stale));
        assert!(rx.poisoned);
        closed(extra_phone);
        assert_eq!(tx.poll_adoption(), Err(Error::Disconnected));
    }
}

#[test]
fn old_ack_cannot_release_new_transfer_in_same_generation() {
    let (mut tx, mut rx) = super::tests::pair();
    let first = arrival(&mut tx, 5);
    let received = adopted(&mut rx);
    assert!(tx.poll_adoption().unwrap());
    drop(received);
    closed(first);
    let second = arrival(&mut tx, 5);
    assert_eq!(tx.pending.as_ref().unwrap().transfer, 2);
    rx.stream.as_mut().unwrap().write_all(&ack(5, 1)).unwrap();
    assert_eq!(tx.poll_adoption(), Err(Error::Stale));
    assert!(tx.poisoned);
    closed(second);
}

#[test]
fn transfer_id_exhaustion_never_wraps_or_sends_a_descriptor() {
    let (mut tx, mut rx) = super::tests::pair();
    tx.next_transfer = u64::MAX;
    let (stream, phone) = UnixStream::pair().unwrap();
    assert_eq!(
        tx.send(Message::Arrival {
            generation: 1,
            peer: fixture().phone(),
            local: fixture().phone(),
            stream: stream.into(),
        }),
        Err(Error::Stale)
    );
    assert_eq!(tx.next_transfer, u64::MAX);
    assert!(tx.pending.is_none());
    assert!(rx.receive().unwrap().is_none());
    closed(phone);
}

#[test]
fn malformed_ack_or_ancillary_descriptor_fails_closed() {
    for with_right in [false, true] {
        let (mut tx, mut rx) = super::tests::pair();
        let phone = arrival(&mut tx, 1);
        if with_right {
            let (extra, extra_phone) = UnixStream::pair().unwrap();
            super::tests::raw_rights(rx.stream.as_ref().unwrap(), &mut ack(1, 1), &[extra.into()]);
            assert_eq!(tx.poll_adoption(), Err(Error::Malformed));
            closed(extra_phone);
        } else {
            let mut bytes = ack(1, 1);
            bytes[20] = 1;
            rx.stream.as_mut().unwrap().write_all(&bytes).unwrap();
            assert_eq!(tx.poll_adoption(), Err(Error::Malformed));
        }
        closed(phone);
    }
}
