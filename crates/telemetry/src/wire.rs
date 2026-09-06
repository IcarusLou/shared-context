use crate::{Authorization, EntryPoint, Event, EventKind, Outcome};

pub const FRAME_MAX_BYTES: usize = 512;
pub const FRAME_MAGIC: [u8; 4] = *b"SCTX";
pub const FRAME_VERSION: u8 = 1;
const HEADER_BYTES: usize = 8;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DecodeError {
    InvalidVersion,
    InvalidLength,
    InvalidPayload,
    InvalidUtf8,
}

#[derive(Debug, Default)]
pub struct FrameDecoder {
    buffer: Vec<u8>,
}

impl FrameDecoder {
    #[must_use]
    pub const fn new() -> Self {
        Self { buffer: Vec::new() }
    }

    pub fn push(&mut self, bytes: &[u8]) -> Vec<Result<Event, DecodeError>> {
        let mut decoded = Vec::new();
        for chunk in bytes.chunks(FRAME_MAX_BYTES) {
            self.buffer.extend_from_slice(chunk);
            self.decode_available(&mut decoded);
        }
        decoded
    }

    fn decode_available(&mut self, decoded: &mut Vec<Result<Event, DecodeError>>) {
        loop {
            let Some(magic_at) = self
                .buffer
                .windows(FRAME_MAGIC.len())
                .position(|window| window == FRAME_MAGIC)
            else {
                let keep = self.buffer.len().min(FRAME_MAGIC.len() - 1);
                self.buffer.drain(..self.buffer.len() - keep);
                return;
            };
            if magic_at > 0 {
                self.buffer.drain(..magic_at);
            }
            if self.buffer.len() < HEADER_BYTES {
                return;
            }
            if self.buffer[4] != FRAME_VERSION {
                self.buffer.drain(..1);
                decoded.push(Err(DecodeError::InvalidVersion));
                continue;
            }
            let payload_len = usize::from(u16::from_be_bytes([self.buffer[6], self.buffer[7]]));
            let frame_len = HEADER_BYTES.saturating_add(payload_len);
            if frame_len > FRAME_MAX_BYTES {
                self.buffer.drain(..1);
                decoded.push(Err(DecodeError::InvalidLength));
                continue;
            }
            if self.buffer.len() < frame_len {
                return;
            }
            let result = decode_payload(&self.buffer[HEADER_BYTES..frame_len]);
            self.buffer.drain(..frame_len);
            decoded.push(result);
        }
    }
}

pub(crate) fn encode_frame(event: &Event) -> Option<Vec<u8>> {
    let mut candidate = Event::bounded_from(event);
    for optional in 0..8 {
        if let Some(frame) = encode_candidate(&candidate) {
            return Some(frame);
        }
        match optional {
            0 => candidate.summary = None,
            1 => candidate.session_digest = None,
            2 => candidate.task_session_id = None,
            3 => candidate.episode_id = None,
            4 => candidate.checkpoint_id = None,
            5 => candidate.operation_id = None,
            6 => candidate.task_id = None,
            _ => candidate.reason = None,
        }
    }
    encode_candidate(&candidate)
}

fn encode_candidate(event: &Event) -> Option<Vec<u8>> {
    let mut payload = Vec::with_capacity(FRAME_MAX_BYTES - HEADER_BYTES);
    payload.extend_from_slice(&event.occurred_at_unix_ms.to_be_bytes());
    payload.extend_from_slice(&event.duration_ms.unwrap_or(u32::MAX).to_be_bytes());
    payload.extend_from_slice(&event.sequence.to_be_bytes());
    payload.push(entry_point_code(event.entry_point));
    payload.push(kind_code(event.kind));
    payload.push(outcome_code(event.outcome));
    payload.push(authorization_code(event.authorization));
    payload.extend_from_slice(&event.result_count.unwrap_or(u32::MAX).to_be_bytes());
    put_string(&mut payload, &event.invocation_id)?;
    put_string(&mut payload, &event.program_version)?;
    put_optional(&mut payload, event.operation.as_deref())?;
    put_optional(&mut payload, event.error_code.as_deref())?;
    put_optional(&mut payload, event.error_family.as_deref())?;
    put_optional(&mut payload, event.reason.as_deref())?;
    put_optional(&mut payload, event.summary.as_deref())?;
    put_optional(&mut payload, event.session_digest.as_deref())?;
    put_optional(&mut payload, event.task_id.as_deref())?;
    put_optional(&mut payload, event.task_session_id.as_deref())?;
    put_optional(&mut payload, event.episode_id.as_deref())?;
    put_optional(&mut payload, event.checkpoint_id.as_deref())?;
    put_optional(&mut payload, event.operation_id.as_deref())?;
    if payload.len() + HEADER_BYTES > FRAME_MAX_BYTES {
        return None;
    }
    let payload_len = u16::try_from(payload.len()).ok()?;
    let mut frame = Vec::with_capacity(payload.len() + HEADER_BYTES);
    frame.extend_from_slice(&FRAME_MAGIC);
    frame.push(FRAME_VERSION);
    frame.push(0);
    frame.extend_from_slice(&payload_len.to_be_bytes());
    frame.extend_from_slice(&payload);
    Some(frame)
}

fn put_string(output: &mut Vec<u8>, value: &str) -> Option<()> {
    let len = u8::try_from(value.len()).ok()?;
    output.push(len);
    output.extend_from_slice(value.as_bytes());
    Some(())
}

fn put_optional(output: &mut Vec<u8>, value: Option<&str>) -> Option<()> {
    if let Some(value) = value {
        put_string(output, value)
    } else {
        output.push(u8::MAX);
        Some(())
    }
}

fn decode_payload(payload: &[u8]) -> Result<Event, DecodeError> {
    let mut reader = Reader::new(payload);
    let occurred_at_unix_ms = reader.i64()?;
    let duration = reader.u32()?;
    let sequence = reader.u32()?;
    let entry_point = decode_entry_point(reader.u8()?)?;
    let kind = decode_kind(reader.u8()?)?;
    let outcome = decode_outcome(reader.u8()?)?;
    let authorization = decode_authorization(reader.u8()?)?;
    let result_count = reader.u32()?;
    let event = Event {
        occurred_at_unix_ms,
        duration_ms: (duration != u32::MAX).then_some(duration),
        sequence,
        entry_point,
        kind,
        outcome,
        authorization,
        invocation_id: reader.string()?,
        program_version: reader.string()?,
        operation: reader.optional()?,
        error_code: reader.optional()?,
        error_family: reader.optional()?,
        reason: reader.optional()?,
        summary: reader.optional()?,
        session_digest: reader.optional()?,
        task_id: reader.optional()?,
        task_session_id: reader.optional()?,
        episode_id: reader.optional()?,
        checkpoint_id: reader.optional()?,
        operation_id: reader.optional()?,
        result_count: (result_count != u32::MAX).then_some(result_count),
    };
    if reader.remaining() != 0 {
        return Err(DecodeError::InvalidPayload);
    }
    Ok(event)
}

struct Reader<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> Reader<'a> {
    const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }
    fn remaining(&self) -> usize {
        self.bytes.len().saturating_sub(self.position)
    }
    fn take(&mut self, len: usize) -> Result<&'a [u8], DecodeError> {
        let end = self
            .position
            .checked_add(len)
            .ok_or(DecodeError::InvalidPayload)?;
        let value = self
            .bytes
            .get(self.position..end)
            .ok_or(DecodeError::InvalidPayload)?;
        self.position = end;
        Ok(value)
    }
    fn u8(&mut self) -> Result<u8, DecodeError> {
        Ok(self.take(1)?[0])
    }
    fn u32(&mut self) -> Result<u32, DecodeError> {
        Ok(u32::from_be_bytes(
            self.take(4)?
                .try_into()
                .map_err(|_| DecodeError::InvalidPayload)?,
        ))
    }
    fn i64(&mut self) -> Result<i64, DecodeError> {
        Ok(i64::from_be_bytes(
            self.take(8)?
                .try_into()
                .map_err(|_| DecodeError::InvalidPayload)?,
        ))
    }
    fn string(&mut self) -> Result<String, DecodeError> {
        let len = usize::from(self.u8()?);
        let bytes = self.take(len)?;
        String::from_utf8(bytes.to_vec()).map_err(|_| DecodeError::InvalidUtf8)
    }
    fn optional(&mut self) -> Result<Option<String>, DecodeError> {
        let len = self.u8()?;
        if len == u8::MAX {
            return Ok(None);
        }
        let bytes = self.take(usize::from(len))?;
        String::from_utf8(bytes.to_vec())
            .map(Some)
            .map_err(|_| DecodeError::InvalidUtf8)
    }
}

fn entry_point_code(value: EntryPoint) -> u8 {
    match value {
        EntryPoint::Cli => 0,
        EntryPoint::Hook => 1,
        EntryPoint::Mcp => 2,
        EntryPoint::Maintenance => 3,
        EntryPoint::Collector => 4,
    }
}
fn kind_code(value: EventKind) -> u8 {
    match value {
        EventKind::OperationStarted => 0,
        EventKind::OperationFinished => 1,
        EventKind::HookDecision => 2,
        EventKind::ToolStarted => 3,
        EventKind::ToolFinished => 4,
        EventKind::ProtocolFailure => 5,
        EventKind::MaintenanceStepFinished => 6,
    }
}
fn outcome_code(value: Outcome) -> u8 {
    match value {
        Outcome::Started => 0,
        Outcome::Success => 1,
        Outcome::Failure => 2,
        Outcome::Disabled => 3,
        Outcome::FailOpen => 4,
        Outcome::Degraded => 5,
        Outcome::Dropped => 6,
        Outcome::Unknown => 7,
    }
}
fn authorization_code(value: Authorization) -> u8 {
    match value {
        Authorization::NotApplicable => 0,
        Authorization::Authorized => 1,
        Authorization::Unauthorized => 2,
        Authorization::Unverified => 3,
    }
}
fn decode_entry_point(value: u8) -> Result<EntryPoint, DecodeError> {
    match value {
        0 => Ok(EntryPoint::Cli),
        1 => Ok(EntryPoint::Hook),
        2 => Ok(EntryPoint::Mcp),
        3 => Ok(EntryPoint::Maintenance),
        4 => Ok(EntryPoint::Collector),
        _ => Err(DecodeError::InvalidPayload),
    }
}
fn decode_kind(value: u8) -> Result<EventKind, DecodeError> {
    match value {
        0 => Ok(EventKind::OperationStarted),
        1 => Ok(EventKind::OperationFinished),
        2 => Ok(EventKind::HookDecision),
        3 => Ok(EventKind::ToolStarted),
        4 => Ok(EventKind::ToolFinished),
        5 => Ok(EventKind::ProtocolFailure),
        6 => Ok(EventKind::MaintenanceStepFinished),
        _ => Err(DecodeError::InvalidPayload),
    }
}
fn decode_outcome(value: u8) -> Result<Outcome, DecodeError> {
    match value {
        0 => Ok(Outcome::Started),
        1 => Ok(Outcome::Success),
        2 => Ok(Outcome::Failure),
        3 => Ok(Outcome::Disabled),
        4 => Ok(Outcome::FailOpen),
        5 => Ok(Outcome::Degraded),
        6 => Ok(Outcome::Dropped),
        7 => Ok(Outcome::Unknown),
        _ => Err(DecodeError::InvalidPayload),
    }
}
fn decode_authorization(value: u8) -> Result<Authorization, DecodeError> {
    match value {
        0 => Ok(Authorization::NotApplicable),
        1 => Ok(Authorization::Authorized),
        2 => Ok(Authorization::Unauthorized),
        3 => Ok(Authorization::Unverified),
        _ => Err(DecodeError::InvalidPayload),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decoder_recovers_from_noise_and_partial_frames() {
        let event = Event::started(
            EntryPoint::Cli,
            EventKind::OperationStarted,
            "inv-1",
            "status",
        );
        let frame = encode_frame(&event).expect("frame");
        assert!(frame.len() <= FRAME_MAX_BYTES);
        let mut decoder = FrameDecoder::new();
        assert!(decoder.push(&[1, 2, 3]).is_empty());
        assert!(decoder.push(&frame[..5]).is_empty());
        let results = decoder.push(&frame[5..]);
        assert_eq!(results, vec![Ok(event.normalized())]);
    }

    #[test]
    fn arbitrarily_large_caller_text_is_bounded_before_allocation_and_free_text_is_dropped() {
        let mut event = Event::started(
            EntryPoint::Mcp,
            EventKind::ToolStarted,
            "x".repeat(1_000_000),
            "tool".repeat(250_000),
        );
        event.summary = Some("sk-proj-secret".repeat(100_000));
        event.task_id = Some("task".repeat(250_000));
        let frame = encode_frame(&event).expect("bounded frame");
        assert!(frame.len() <= FRAME_MAX_BYTES);
        let mut decoder = FrameDecoder::new();
        let result = decoder
            .push(&frame)
            .pop()
            .expect("one frame")
            .expect("valid frame");
        assert_eq!(result.summary, None);
        assert!(result.invocation_id.len() <= 64);
        assert!(
            result
                .operation
                .as_ref()
                .is_some_and(|value| value.len() <= 48)
        );
    }

    #[test]
    fn large_followup_chunks_preserve_a_preceding_partial_frame() {
        let mut stream = Vec::new();
        let expected = (0..160)
            .map(|index| {
                Event::started(
                    EntryPoint::Mcp,
                    EventKind::ToolStarted,
                    format!("invocation-{index}"),
                    "query",
                )
                .normalized()
            })
            .collect::<Vec<_>>();
        for event in &expected {
            stream.extend_from_slice(&encode_frame(event).expect("frame"));
        }
        assert!(stream.len() > 8_192);
        let mut decoder = FrameDecoder::new();
        let mut actual = decoder.push(&stream[..5]);
        actual.extend(decoder.push(&stream[5..4_101]));
        actual.extend(decoder.push(&stream[4_101..8_197]));
        actual.extend(decoder.push(&stream[8_197..]));
        assert_eq!(
            actual
                .into_iter()
                .map(|result| result.expect("valid frame"))
                .collect::<Vec<_>>(),
            expected
        );
    }
}
