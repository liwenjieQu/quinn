//! Maintain the state of local connection IDs
use std::{
    collections::{HashSet, VecDeque},
    time::{Duration, Instant},
};

use tracing::{debug, trace};

use crate::{shared::IssuedCid, TransportError};

/// Data structure that records when issued cids should be retired
#[derive(Copy, Clone, Eq, PartialEq)]
struct CidTimeStamp {
    /// Highest cid sequence number created in a batch
    sequence: u64,
    /// Timestamp when cid needs to be retired
    timestamp: Instant,
}

/// Local connection IDs management
///
/// `CidState` maintains attributes of local connection IDs
pub struct CidState {
    /// Timestamp when issued cids should be retired
    retire_timestamp: VecDeque<CidTimeStamp>,
    /// Number of local connection IDs that have been issued in NEW_CONNECTION_ID frames.
    issued: u64,
    /// Sequence numbers of local connection IDs not yet retired by the peer
    active_seq: HashSet<u64>,
    /// Sequence number the peer has already retired all CIDs below at our request via `retire_prior_to`
    prev_retire_seq: u64,
    /// Sequence number to set in retire_prior_to field in NEW_CONNECTION_ID frame
    retire_seq: u64,
    /// cid length used to decode short packet
    cid_len: usize,
    //// cid lifetime
    cid_lifetime: Option<Duration>,
}

impl CidState {
    pub(crate) fn new(cid_len: usize, cid_lifetime: Option<Duration>) -> Self {
        let mut active_seq = HashSet::new();
        // Add sequence number of CID used in handshaking into tracking set
        active_seq.insert(0);
        CidState {
            retire_timestamp: VecDeque::new(),
            issued: 1, // One CID is already supplied during handshaking
            active_seq,
            prev_retire_seq: 0,
            retire_seq: 0,
            cid_len,
            cid_lifetime,
        }
    }

    /// Find the next timestamp when previously issued CID should be retired
    pub(crate) fn next_timeout(&mut self) -> Option<Instant> {
        self.retire_timestamp.front().map(|nc| {
            trace!("CID {} will expire at {:?}", nc.sequence, nc.timestamp);
            nc.timestamp
        })
    }

    /// Track the lifetime of issued cids in `retire_timestamp`
    pub(crate) fn track_lifetime(&mut self, new_cid_seq: u64, now: Instant) {
        let lifetime = match self.cid_lifetime {
            Some(lifetime) => lifetime,
            None => return,
        };
        let expire_timestamp = now.checked_add(lifetime);
        if let Some(expire_at) = expire_timestamp {
            let last_record = self.retire_timestamp.back_mut();
            if let Some(last) = last_record {
                // Compare the timestamp with the last inserted record
                // Combine into a single batch if timestamp of current cid is same as the last record
                if expire_at == last.timestamp {
                    debug_assert!(new_cid_seq > last.sequence);
                    last.sequence = new_cid_seq;
                    return;
                }
            }
            self.retire_timestamp.push_back(CidTimeStamp {
                sequence: new_cid_seq,
                timestamp: expire_at,
            });
        }
    }

    /// Update local CID state when previously issued CID is retired
    /// Return a flag that indicates whether a new CID needs to be pushed that notifies remote peer to respond `RETIRE_CONNECTION_ID`
    pub(crate) fn on_cid_timeout(&mut self) -> bool {
        // Whether the peer hasn't retired all the CIDs we asked it to yet
        let unretired_ids_found =
            (self.prev_retire_seq..self.retire_seq).any(|seq| self.active_seq.contains(&seq));
        // According to RFC:
        // Endpoints SHOULD NOT issue updates of the Retire Prior To field
        // before receiving RETIRE_CONNECTION_ID frames that retire all
        // connection IDs indicated by the previous Retire Prior To value.
        // https://tools.ietf.org/html/draft-ietf-quic-transport-29#section-5.1.2
        //
        // All Cids are retired, `prev_retire_cid_seq` can be assigned to `retire_cid_seq`
        if !unretired_ids_found {
            self.prev_retire_seq = self.retire_seq;
        }

        let next_retire_sequence = self
            .retire_timestamp
            .pop_front()
            .map(|seq| seq.sequence + 1);
        let current_retire_prior_to = self.retire_seq;

        // Advance `retire_cid_seq` if next cid that needs to be retired exists
        if let Some(next_retire_prior_to) = next_retire_sequence {
            if !unretired_ids_found && next_retire_prior_to > current_retire_prior_to {
                self.retire_seq = next_retire_prior_to;
            }
        }

        // Check if retirement of all CIDs that reach their lifetime is still needed
        // If yes (return true), a new CID must be pushed with updated `retire_prior_to` field to remote peer.
        // If no (return false), it means remote peer has proactively retired those CIDs (for other reasons) before CID lifetime is reached.
        (current_retire_prior_to..self.retire_seq).any(|seq| self.active_seq.contains(&seq))
    }

    /// Update cid state when `NewIdentifiers` event is received
    pub(crate) fn new_cids(&mut self, ids: &[IssuedCid], now: Instant) {
        // `ids` could be `None` once active_connection_id_limit is set to 1 by peer
        if let Some(last_cid) = ids.last() {
            self.issued += ids.len() as u64;
            // Record the timestamp of CID with the largest seq number
            let sequence = last_cid.sequence;
            ids.iter().for_each(|frame| {
                self.active_seq.insert(frame.sequence);
            });
            self.track_lifetime(sequence, now);
        }
    }

    /// Update CidState when recieve a `RETIRE_CONNECTION_ID` frame
    /// Return a boolean variable or `TransportError` if CID content violates RFC
    /// When boolean variable is `true`, a new CID should be issued to peer.
    pub(crate) fn on_cid_retirement(
        &mut self,
        sequence: u64,
        limit: u64,
    ) -> Result<bool, TransportError> {
        if self.cid_len == 0 {
            return Err(TransportError::PROTOCOL_VIOLATION(
                "RETIRE_CONNECTION_ID when CIDs aren't in use",
            ));
        }
        if sequence > self.issued {
            debug!(
                sequence,
                "got RETIRE_CONNECTION_ID for unissued sequence number"
            );
            return Err(TransportError::PROTOCOL_VIOLATION(
                "RETIRE_CONNECTION_ID for unissued sequence number",
            ));
        }
        self.active_seq.remove(&sequence);
        // Consider a scenario where peer A has active remote cid 0,1,2.
        // Peer B first send a NEW_CONNECTION_ID with cid 3 and retire_prior_to set to 1.
        // Peer A processes this NEW_CONNECTION_ID frame; update remote cid to 1,2,3
        // and meanwhile send a RETIRE_CONNECTION_ID to retire cid 0 to peer B.
        // If peer B doesn't check the cid limit here and send a new cid again, peer A will then face CONNECTION_ID_LIMIT_ERROR
        let allow_more_cids = limit > self.active_seq.len() as u64;

        Ok(allow_more_cids)
    }

    /// Length of local Connection IDs
    pub(crate) fn cid_len(&self) -> usize {
        self.cid_len
    }

    /// The value for `retire_prior_to` field in `NEW_CONNECTION_ID` frame
    pub(crate) fn retire_prior_to(&self) -> u64 {
        self.retire_seq
    }

    #[cfg(test)]
    pub(crate) fn active_seq(&self) -> (u64, u64) {
        (
            *self.active_seq.iter().min().unwrap(),
            *self.active_seq.iter().max().unwrap(),
        )
    }

    #[cfg(test)]
    pub(crate) fn assign_retire_seq(&mut self, v: u64) -> u64 {
        // Cannot retire more CIDs than what have been issued
        debug_assert!(v <= *self.active_seq.iter().max().unwrap() + 1);
        let n = v.checked_sub(self.retire_seq).unwrap();
        self.retire_seq = v;
        n
    }
}
