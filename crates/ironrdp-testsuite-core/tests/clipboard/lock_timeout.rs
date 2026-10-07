//! Tests for lock timeout and manual cleanup behavior.
//!
//! Migrated from `ironrdp-cliprdr/src/lib.rs` inline `#[cfg(test)]` module.
//! Behavior assertions verify returned PDUs and backend callbacks;
//! bookkeeping assertions (internal lock counts, throttle state) use the
//! `__test` feature gate.

use std::sync::{Arc, Mutex};

use ironrdp_cliprdr::pdu::{
    Capabilities, ClipboardFormat, ClipboardFormatId, ClipboardFormatName, ClipboardGeneralCapabilityFlags,
    ClipboardPdu, ClipboardProtocolVersion, FileContentsFlags, FileContentsRequest, FileContentsResponse,
    FileDescriptor, FormatList, LockDataId, PackedFileList,
};
use ironrdp_cliprdr::{Cliprdr, CliprdrClient, CliprdrState};
use ironrdp_core::Encode as _;
use ironrdp_svc::{SvcMessage, SvcProcessor as _};

use ironrdp_testsuite_core::clipboard::{CallbackTrackingBackend, LockingBackend};

/// Introduce `let` bindings for the encoded bytes and the decoded
/// [`ClipboardPdu`] in the caller's scope.  Two names are required so
/// that the byte buffer outlives the borrowing PDU.
macro_rules! decode_pdu {
    ($msg:expr => $bytes:ident, $pdu:ident) => {
        let $bytes = ($msg).encode_unframed_pdu().unwrap();
        let $pdu = ironrdp_core::decode::<ClipboardPdu<'_>>(&$bytes).unwrap();
    };
}

/// Helper: build a file list with sizes.
fn sized_file_list(entries: &[(&str, u64)]) -> PackedFileList {
    PackedFileList {
        files: entries
            .iter()
            .map(|(name, size)| FileDescriptor::new(*name).with_file_size(*size))
            .collect(),
    }
}

/// Helper: create a Format List PDU containing FileGroupDescriptorW.
fn file_format_list_buf() -> Vec<u8> {
    let formats = vec![ClipboardFormat {
        id: ClipboardFormatId(49171),
        name: Some(ClipboardFormatName::new("FileGroupDescriptorW")),
    }];

    let format_list = FormatList::new_unicode(&formats, true).unwrap();
    let pdu = ClipboardPdu::FormatList(format_list);
    let mut buf = vec![0u8; pdu.size()];
    let mut cursor = ironrdp_core::WriteCursor::new(&mut buf);
    pdu.encode(&mut cursor).unwrap();
    buf
}

/// Helper: create a Format List PDU containing only text (no files).
fn text_format_list_buf() -> Vec<u8> {
    let formats = vec![ClipboardFormat {
        id: ClipboardFormatId(13), // CF_UNICODETEXT
        name: None,
    }];

    let format_list = FormatList::new_unicode(&formats, true).unwrap();
    let pdu = ClipboardPdu::FormatList(format_list);
    let mut buf = vec![0u8; pdu.size()];
    let mut cursor = ironrdp_core::WriteCursor::new(&mut buf);
    pdu.encode(&mut cursor).unwrap();
    buf
}

/// Helper: set up a CliprdrClient with lock timeouts and a mock clock backend.
fn timed_locking_client(inactivity_ms: u64, max_ms: u64) -> CliprdrClient {
    let mut cliprdr: CliprdrClient = Cliprdr::with_lock_timeouts(
        Box::new(LockingBackend::new()),
        core::time::Duration::from_millis(inactivity_ms),
        core::time::Duration::from_millis(max_ms),
    );
    *cliprdr.__test_state_mut() = CliprdrState::Ready;
    *cliprdr.__test_capabilities_mut() = Capabilities::new(
        ClipboardProtocolVersion::V2,
        ClipboardGeneralCapabilityFlags::CAN_LOCK_CLIPDATA
            | ClipboardGeneralCapabilityFlags::STREAM_FILECLIP_ENABLED
            | ClipboardGeneralCapabilityFlags::USE_LONG_FORMAT_NAMES,
    );
    cliprdr
}

/// Helper: process a file FormatList and extract the lock ID from the
/// returned Lock PDU.
fn process_file_format_list(cliprdr: &mut CliprdrClient) -> u32 {
    let messages: Vec<SvcMessage> = cliprdr.process(&file_format_list_buf()).unwrap();
    assert!(messages.len() >= 2, "expected FormatListResponse + LockData");
    decode_pdu!(messages[1] => _bytes, lock_pdu);
    match lock_pdu {
        ClipboardPdu::LockData(id) => id.0,
        other => panic!("expected LockData PDU, got {other:?}"),
    }
}

/// Helper: expire a lock by processing a text FormatList.
fn expire_via_text_format_list(cliprdr: &mut CliprdrClient) {
    cliprdr.process(&text_format_list_buf()).unwrap();
}

// -- Activity & timeout ----------------------------------------------

#[test]
fn lock_activity_prevents_timeout() {
    let mut cliprdr = timed_locking_client(200, 10_000);
    *cliprdr.__test_remote_file_list_mut() = Some(sized_file_list(&[("test.txt", 1000)]));

    let clip_data_id = process_file_format_list(&mut cliprdr);

    // Expire the lock via text FormatList (simulates clipboard change)
    expire_via_text_format_list(&mut cliprdr);

    // Advance 50ms before making request
    cliprdr.downcast_backend::<LockingBackend>().unwrap().advance_ms(50);

    let request = FileContentsRequest {
        stream_id: 1,
        index: 0,
        flags: FileContentsFlags::SIZE,
        position: 0,
        requested_size: 8,
        data_id: Some(clip_data_id),
    };
    let _result = cliprdr.request_file_contents(request).unwrap();

    // Cleanup should not remove lock (activity happened < 200ms ago)
    let messages: Vec<SvcMessage> = Vec::from(cliprdr.drive_timeouts().unwrap());
    assert_eq!(messages.len(), 0, "no locks should be cleaned up yet");

    // Bookkeeping: lock still tracked
    assert!(cliprdr.__test_outgoing_locks().contains_key(&clip_data_id));

    // Advance past inactivity timeout (250ms more, total 300ms since start)
    cliprdr.downcast_backend::<LockingBackend>().unwrap().advance_ms(250);

    // Behavior: cleanup returns an Unlock PDU for the expired lock
    let messages: Vec<SvcMessage> = Vec::from(cliprdr.drive_timeouts().unwrap());
    assert_eq!(messages.len(), 1, "one lock should be cleaned up");
    decode_pdu!(messages[0] => _bytes, pdu);
    match pdu {
        ClipboardPdu::UnlockData(id) => assert_eq!(id.0, clip_data_id),
        other => panic!("expected UnlockData PDU, got {other:?}"),
    }

    // Bookkeeping: lock removed
    assert!(!cliprdr.__test_outgoing_locks().contains_key(&clip_data_id));
}

#[test]
fn max_lifetime_forces_cleanup() {
    let mut cliprdr = timed_locking_client(60_000, 150);

    let fl = sized_file_list(&[("test.txt", 1000)]);
    *cliprdr.__test_local_file_list_mut() = Some(fl.clone());
    *cliprdr.__test_remote_file_list_mut() = Some(fl);

    let clip_data_id = process_file_format_list(&mut cliprdr);

    // Expire the lock via text FormatList
    expire_via_text_format_list(&mut cliprdr);

    // Simulate active transfer with 40ms intervals via mock clock
    for i in 0..5 {
        cliprdr.downcast_backend::<LockingBackend>().unwrap().advance_ms(40);

        let request = FileContentsRequest {
            stream_id: i + 1,
            index: 0,
            flags: FileContentsFlags::SIZE,
            position: 0,
            requested_size: 8,
            data_id: Some(clip_data_id),
        };
        let _result = cliprdr.request_file_contents(request).unwrap();

        let messages: Vec<SvcMessage> = Vec::from(cliprdr.drive_timeouts().unwrap());
        if !messages.is_empty() {
            // Behavior: cleanup returns an Unlock PDU for the expired lock
            assert_eq!(messages.len(), 1);
            decode_pdu!(messages[0] => _bytes, pdu);
            match pdu {
                ClipboardPdu::UnlockData(id) => assert_eq!(id.0, clip_data_id),
                other => panic!("expected UnlockData PDU, got {other:?}"),
            }
            assert!(!cliprdr.__test_outgoing_locks().contains_key(&clip_data_id));
            return; // Test passed
        }
    }

    // Final check after all iterations (total 200ms > 150ms max lifetime)
    let messages: Vec<SvcMessage> = Vec::from(cliprdr.drive_timeouts().unwrap());
    assert_eq!(
        messages.len(),
        1,
        "lock should be cleaned up after max lifetime even with activity"
    );
    decode_pdu!(messages[0] => _bytes, pdu);
    match pdu {
        ClipboardPdu::UnlockData(id) => assert_eq!(id.0, clip_data_id),
        other => panic!("expected UnlockData PDU, got {other:?}"),
    }
    assert!(!cliprdr.__test_outgoing_locks().contains_key(&clip_data_id));
}

// -- Idle release of still-active locks --------------------------------
//
// Regression coverage: an outgoing lock only used to be released when the
// *remote clipboard changed*, when the local side initiated a file copy, or
// after the 2h max lifetime. Windows `rdpclip.exe` keeps the source files open
// for as long as a lock is held ([MS-RDPECLIP] 2.2.4.1, the owner must retain
// the File Stream data), so copying a file out of the session and then trying
// to delete it on the remote failed with "the file is open in another program"
// until something unrelated touched the clipboard.

#[test]
fn active_lock_released_once_transfer_goes_quiet() {
    let mut cliprdr = timed_locking_client(200, 10_000);
    *cliprdr.__test_remote_file_list_mut() = Some(sized_file_list(&[("test.txt", 1000)]));

    let clip_data_id = process_file_format_list(&mut cliprdr);

    // From here on the clipboard never changes: no text FormatList is processed,
    // which is exactly the situation the regression was about.
    let messages: Vec<SvcMessage> = Vec::from(cliprdr.drive_timeouts().unwrap());
    assert_eq!(messages.len(), 0, "fresh lock must not be released");
    assert!(cliprdr.__test_outgoing_locks().contains_key(&clip_data_id));

    // Idle past the inactivity timeout with no transfer pending.
    cliprdr.downcast_backend::<LockingBackend>().unwrap().advance_ms(250);

    let messages: Vec<SvcMessage> = Vec::from(cliprdr.drive_timeouts().unwrap());
    assert_eq!(messages.len(), 1, "idle active lock should be released");
    decode_pdu!(messages[0] => _bytes, pdu);
    match pdu {
        ClipboardPdu::UnlockData(id) => assert_eq!(id.0, clip_data_id),
        other => panic!("expected UnlockData PDU, got {other:?}"),
    }

    assert!(!cliprdr.__test_outgoing_locks().contains_key(&clip_data_id));
    assert_eq!(cliprdr.__test_current_lock_id(), None);
}

#[test]
fn active_lock_held_while_a_file_contents_request_is_pending() {
    let mut cliprdr = timed_locking_client(200, 10_000);
    let fl = sized_file_list(&[("test.txt", 1000)]);
    *cliprdr.__test_remote_file_list_mut() = Some(fl.clone());
    *cliprdr.__test_local_file_list_mut() = Some(fl);

    let clip_data_id = process_file_format_list(&mut cliprdr);

    // Leave a download request unanswered: the transfer is still in progress.
    let request = FileContentsRequest {
        stream_id: 7,
        index: 0,
        flags: FileContentsFlags::SIZE,
        position: 0,
        requested_size: 8,
        data_id: Some(clip_data_id),
    };
    cliprdr.request_file_contents(request).unwrap();

    // Idle far past the inactivity timeout while that request is outstanding.
    cliprdr.downcast_backend::<LockingBackend>().unwrap().advance_ms(5_000);

    let messages: Vec<SvcMessage> = Vec::from(cliprdr.drive_timeouts().unwrap());
    assert_eq!(messages.len(), 0, "lock must not be released mid-transfer");
    assert!(cliprdr.__test_outgoing_locks().contains_key(&clip_data_id));

    // The server answers; the transfer is over, so the lock becomes releasable.
    let response = FileContentsResponse::new_size_response(7, 1000);
    let response_pdu = ClipboardPdu::FileContentsResponse(response);
    let mut buf = vec![0u8; response_pdu.size()];
    let mut cursor = ironrdp_core::WriteCursor::new(&mut buf);
    response_pdu.encode(&mut cursor).unwrap();
    cliprdr.process(&buf).unwrap();
    assert!(
        cliprdr.__test_sent_file_contents_requests().is_empty(),
        "answering the request must drain the pending set"
    );

    cliprdr.downcast_backend::<LockingBackend>().unwrap().advance_ms(250);

    let messages: Vec<SvcMessage> = Vec::from(cliprdr.drive_timeouts().unwrap());
    assert_eq!(messages.len(), 1, "lock should be released once nothing is pending");
    decode_pdu!(messages[0] => _bytes, pdu);
    match pdu {
        ClipboardPdu::UnlockData(id) => assert_eq!(id.0, clip_data_id),
        other => panic!("expected UnlockData PDU, got {other:?}"),
    }
    assert!(!cliprdr.__test_outgoing_locks().contains_key(&clip_data_id));
}

#[test]
fn expire_via_format_list_transitions_state() {
    let cleared_ids = Arc::new(Mutex::new(Vec::new()));
    let expired_ids = Arc::new(Mutex::new(Vec::new()));
    let backend = CallbackTrackingBackend::with_expired_tracking(Arc::clone(&cleared_ids), Arc::clone(&expired_ids));

    let mut cliprdr: CliprdrClient = Cliprdr::with_lock_timeouts(
        Box::new(backend),
        core::time::Duration::from_secs(60),
        core::time::Duration::from_secs(600),
    );
    *cliprdr.__test_state_mut() = CliprdrState::Ready;
    *cliprdr.__test_capabilities_mut() = Capabilities::new(
        ClipboardProtocolVersion::V2,
        ClipboardGeneralCapabilityFlags::CAN_LOCK_CLIPDATA | ClipboardGeneralCapabilityFlags::USE_LONG_FORMAT_NAMES,
    );

    // Create lock via file FormatList
    let id1 = process_file_format_list(&mut cliprdr);

    // No expired callback yet (only lock creation)
    assert!(expired_ids.lock().unwrap().is_empty());

    // Text FormatList triggers expire_all_locks -> on_outgoing_locks_expired
    expire_via_text_format_list(&mut cliprdr);

    // Behavior: expired callback fired with the lock ID
    {
        let callbacks = expired_ids.lock().unwrap();
        assert_eq!(callbacks.len(), 1, "expired callback should fire once");
        assert_eq!(callbacks[0].len(), 1, "one lock should be expired");
        assert!(callbacks[0].iter().any(|id| id.0 == id1));
    }

    // Behavior: cleared callback has NOT fired
    assert!(cleared_ids.lock().unwrap().is_empty());

    // Bookkeeping: lock still tracked (expired, not removed)
    assert_eq!(cliprdr.__test_outgoing_locks().len(), 1);
}

// -- Backend-driven release of one specific lock -----------------------
//
// The idle sweep above is a fallback. A backend that knows the download it
// asked a lock for has finished (or failed, or was cancelled) can drop that
// lock right away with `ClipboardMessage::ReleaseFileLock`, so the remote
// lets go of the source files without waiting out the inactivity timeout.

/// Helper: build a client whose backend records `on_outgoing_locks_cleared`.
fn callback_tracking_client(cleared_ids: &Arc<Mutex<Vec<Vec<LockDataId>>>>) -> CliprdrClient {
    let backend = CallbackTrackingBackend::new(Arc::clone(cleared_ids));
    let mut cliprdr: CliprdrClient = Cliprdr::with_lock_timeouts(
        Box::new(backend),
        core::time::Duration::from_secs(60),
        core::time::Duration::from_secs(600),
    );
    *cliprdr.__test_state_mut() = CliprdrState::Ready;
    *cliprdr.__test_capabilities_mut() = Capabilities::new(
        ClipboardProtocolVersion::V2,
        ClipboardGeneralCapabilityFlags::CAN_LOCK_CLIPDATA
            | ClipboardGeneralCapabilityFlags::STREAM_FILECLIP_ENABLED
            | ClipboardGeneralCapabilityFlags::USE_LONG_FORMAT_NAMES,
    );
    cliprdr
}

#[test]
fn release_lock_releases_an_active_lock_immediately() {
    let cleared_ids = Arc::new(Mutex::new(Vec::new()));
    let mut cliprdr = callback_tracking_client(&cleared_ids);
    *cliprdr.__test_remote_file_list_mut() = Some(sized_file_list(&[("test.txt", 1000)]));

    let clip_data_id = process_file_format_list(&mut cliprdr);
    assert!(cleared_ids.lock().unwrap().is_empty());

    // No clock advance at all: the lock is released because the backend said
    // so, not because it went idle.
    let messages: Vec<SvcMessage> = Vec::from(cliprdr.release_lock(LockDataId(clip_data_id)).unwrap());
    assert_eq!(messages.len(), 1, "release must produce exactly one Unlock PDU");
    decode_pdu!(messages[0] => _bytes, pdu);
    match pdu {
        ClipboardPdu::UnlockData(id) => assert_eq!(id.0, clip_data_id),
        other => panic!("expected UnlockData PDU, got {other:?}"),
    }

    assert!(!cliprdr.__test_outgoing_locks().contains_key(&clip_data_id));
    assert_eq!(cliprdr.__test_current_lock_id(), None);
    assert_eq!(
        cleared_ids.lock().unwrap().as_slice(),
        &[vec![LockDataId(clip_data_id)]],
        "backend must be told which lock was released"
    );
}

#[test]
fn release_lock_is_a_noop_for_an_unknown_lock() {
    let cleared_ids = Arc::new(Mutex::new(Vec::new()));
    let mut cliprdr = callback_tracking_client(&cleared_ids);
    *cliprdr.__test_remote_file_list_mut() = Some(sized_file_list(&[("test.txt", 1000)]));

    let clip_data_id = process_file_format_list(&mut cliprdr);

    let messages: Vec<SvcMessage> = Vec::from(
        cliprdr
            .release_lock(LockDataId(clip_data_id.wrapping_add(999)))
            .unwrap(),
    );
    assert!(messages.is_empty(), "an unknown lock must not produce a PDU");

    assert!(cliprdr.__test_outgoing_locks().contains_key(&clip_data_id));
    assert_eq!(cliprdr.__test_current_lock_id(), Some(clip_data_id));
    assert!(cleared_ids.lock().unwrap().is_empty());
}

#[test]
fn released_lock_is_not_released_again_by_the_sweep() {
    let cleared_ids = Arc::new(Mutex::new(Vec::new()));
    let mut cliprdr = callback_tracking_client(&cleared_ids);
    *cliprdr.__test_remote_file_list_mut() = Some(sized_file_list(&[("test.txt", 1000)]));

    let clip_data_id = process_file_format_list(&mut cliprdr);
    let _ = cliprdr.release_lock(LockDataId(clip_data_id)).unwrap();

    // Far beyond every timeout: the lock is gone, so the sweep must stay silent
    // rather than release it a second time.
    cliprdr
        .downcast_backend::<CallbackTrackingBackend>()
        .unwrap()
        .advance_ms(10_000_000);
    let messages: Vec<SvcMessage> = Vec::from(cliprdr.drive_timeouts().unwrap());
    assert!(messages.is_empty(), "a released lock must not be released twice");
}

#[test]
fn release_lock_leaves_other_locks_in_place() {
    let cleared_ids = Arc::new(Mutex::new(Vec::new()));
    let mut cliprdr = callback_tracking_client(&cleared_ids);
    *cliprdr.__test_remote_file_list_mut() = Some(sized_file_list(&[("first.txt", 10)]));

    let first = process_file_format_list(&mut cliprdr);

    // A second file copy on the remote: the first lock is only *expired*, it
    // stays tracked so a download still running against it can finish.
    let second = process_file_format_list(&mut cliprdr);
    assert_ne!(first, second, "each file copy must get its own lock id");
    assert_eq!(cliprdr.__test_outgoing_locks().len(), 2);

    let messages: Vec<SvcMessage> = Vec::from(cliprdr.release_lock(LockDataId(second)).unwrap());
    assert_eq!(messages.len(), 1, "releasing one lock sends one Unlock PDU");

    assert!(!cliprdr.__test_outgoing_locks().contains_key(&second));
    assert!(
        cliprdr.__test_outgoing_locks().contains_key(&first),
        "other locks must be left alone"
    );
}
