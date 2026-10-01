//! Кодек кадров: префикс-длина u32 big-endian + JSON-тело.
//!
//! Формат кадра: `u32 BE (длина JSON-тела)` + `JSON-тело`.
//! Кадры длиннее [`MAX_FRAME_LEN`] отвергаются до аллокаций,
//! пропорциональных заявленной длине, — защита control-канала
//! от враждебного префикса длины.

use std::io;
use std::marker::PhantomData;

use bytes::{BufMut, BytesMut};
use serde::de::DeserializeOwned;
use serde::Serialize;
use tokio_util::codec::{Decoder, Encoder};

/// Максимальная длина кадра (16 MiB).
pub const MAX_FRAME_LEN: usize = 16 * 1024 * 1024;

/// Ошибки кодирования и декодирования кадров.
#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    /// Заявленная длина кадра превышает [`MAX_FRAME_LEN`].
    #[error("frame length {0} bytes exceeds the 16 MiB limit")]
    TooLarge(usize),
    /// Тело кадра не разбирается как JSON ожидаемого типа.
    #[error("invalid frame JSON: {0}")]
    BadJson(#[from] serde_json::Error),
    /// Ошибка ввода-вывода; вариант нужен границей `From<io::Error>`
    /// трейтов `Decoder`/`Encoder`.
    #[error("io error: {0}")]
    Io(#[from] io::Error),
}

/// Кодек «u32 BE длина + JSON» для использования с `Framed`.
///
/// Универсален по типу сообщения `T` (например, [`crate::protocol::AgentRequest`]).
pub struct LengthJsonCodec<T>(PhantomData<T>);

impl<T> LengthJsonCodec<T> {
    /// Создаёт кодек.
    pub fn new() -> Self {
        Self(PhantomData)
    }
}

impl<T> Default for LengthJsonCodec<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Serialize> Encoder<T> for LengthJsonCodec<T> {
    type Error = FrameError;

    fn encode(&mut self, item: T, dst: &mut BytesMut) -> Result<(), Self::Error> {
        write_frame(&item, dst)
    }
}

impl<T: DeserializeOwned> Decoder for LengthJsonCodec<T> {
    type Item = T;
    type Error = FrameError;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        decode_frame(src)
    }
}

/// Кодирует значение в кадр и дописывает его в `dst`.
fn write_frame<T: Serialize>(value: &T, dst: &mut BytesMut) -> Result<(), FrameError> {
    let json = serde_json::to_vec(value)?;
    if json.len() > MAX_FRAME_LEN {
        return Err(FrameError::TooLarge(json.len()));
    }
    dst.reserve(4 + json.len());
    dst.put_u32(json.len() as u32);
    dst.extend_from_slice(&json);
    Ok(())
}

/// Кодирует значение в самостоятельный кадр: u32 BE длина + JSON.
///
/// Паникует, если значение не сериализуется или не влезает в лимит кадра —
/// для типов протокола оба случая недостижимы.
pub fn encode_frame<T: Serialize>(value: &T) -> Vec<u8> {
    let mut buf = BytesMut::new();
    write_frame(value, &mut buf).expect("protocol frame is serializable and fits 16 MiB");
    buf.to_vec()
}

/// Разбирает один кадр из буфера.
///
/// Возвращает `Ok(None)`, пока буфер не содержит кадр целиком
/// (стиль `tokio_util::codec::Decoder`). Разобранный кадр удаляется
/// из буфера, поэтому последовательные вызовы читают поток кадров.
pub fn decode_frame<T: DeserializeOwned>(bytes: &mut BytesMut) -> Result<Option<T>, FrameError> {
    if bytes.len() < 4 {
        return Ok(None);
    }
    let mut prefix = [0u8; 4];
    prefix.copy_from_slice(&bytes[..4]);
    let len = u32::from_be_bytes(prefix) as usize;

    // Проверка до аллокаций и чтения тела: враждебный префикс длины
    // не должен заставлять демона резервировать память.
    if len > MAX_FRAME_LEN {
        return Err(FrameError::TooLarge(len));
    }
    if bytes.len() < 4 + len {
        bytes.reserve(4 + len - bytes.len());
        return Ok(None);
    }

    let mut frame = bytes.split_to(4 + len);
    let body = frame.split_off(4);
    Ok(Some(serde_json::from_slice(&body)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::AgentRequest;
    use bytes::BytesMut;
    use tokio_util::codec::Decoder;

    fn sample_request() -> AgentRequest {
        AgentRequest::Down {
            id: "office-a".to_string(),
            instance: 7,
        }
    }

    /// Собирает кадр вручную: u32 BE длина + тело.
    fn raw_frame(body: &[u8]) -> BytesMut {
        let mut buf = BytesMut::new();
        buf.extend_from_slice(&(body.len() as u32).to_be_bytes());
        buf.extend_from_slice(body);
        buf
    }

    #[test]
    fn standalone_helpers_round_trip_and_wire_format() {
        let req = sample_request();
        let frame = encode_frame(&req);

        let json = serde_json::to_vec(&req).expect("serialize");
        assert_eq!(&frame[..4], &(json.len() as u32).to_be_bytes()[..]);
        assert_eq!(&frame[4..], &json[..]);

        let mut buf = BytesMut::from_iter(frame);
        assert_eq!(decode_frame(&mut buf).expect("decode"), Some(req));
        assert!(buf.is_empty(), "frame must be fully consumed");
    }

    #[test]
    fn partial_buffer_returns_none() {
        let full = encode_frame(&sample_request());

        // Пустой буфер.
        let mut buf = BytesMut::new();
        assert!(decode_frame::<AgentRequest>(&mut buf).unwrap().is_none());

        // Неполный префикс длины.
        let mut buf = BytesMut::new();
        buf.extend_from_slice(&full[..2]);
        assert!(decode_frame::<AgentRequest>(&mut buf).unwrap().is_none());

        // Префикс есть, тело не дошло целиком.
        let mut buf = BytesMut::new();
        buf.extend_from_slice(&full[..full.len() - 1]);
        assert!(decode_frame::<AgentRequest>(&mut buf).unwrap().is_none());

        // Тот же результат через трейт Decoder (стиль Framed).
        let mut codec = LengthJsonCodec::<AgentRequest>::new();
        let mut buf = BytesMut::new();
        buf.extend_from_slice(&full[..full.len() - 1]);
        assert!(codec.decode(&mut buf).expect("decode").is_none());
    }

    #[test]
    fn truncated_json_is_bad_json() {
        let mut buf = raw_frame(br#"{"Down":{"id":"office-a""#);
        assert!(matches!(
            decode_frame::<AgentRequest>(&mut buf),
            Err(FrameError::BadJson(_))
        ));
    }

    #[test]
    fn garbage_json_is_bad_json() {
        let mut buf = raw_frame(b"this is not json");
        assert!(matches!(
            decode_frame::<AgentRequest>(&mut buf),
            Err(FrameError::BadJson(_))
        ));
    }

    #[test]
    fn oversized_length_prefix_is_rejected_before_body_arrives() {
        // В буфере только 4 байта префикса, заявляющие длину сверх лимита:
        // отказ должен произойти до чтения тела и до аллокаций под него.
        let mut buf = BytesMut::new();
        buf.extend_from_slice(&((MAX_FRAME_LEN + 1) as u32).to_be_bytes());
        assert!(matches!(
            decode_frame::<AgentRequest>(&mut buf),
            Err(FrameError::TooLarge(_))
        ));

        let mut codec = LengthJsonCodec::<AgentRequest>::new();
        let mut buf = BytesMut::new();
        buf.extend_from_slice(&((MAX_FRAME_LEN + 1) as u32).to_be_bytes());
        assert!(matches!(
            codec.decode(&mut buf),
            Err(FrameError::TooLarge(_))
        ));
    }

    #[test]
    fn decodes_successive_frames_from_one_buffer() {
        let first = encode_frame(&sample_request());
        let second = encode_frame(&AgentRequest::Shutdown);

        let mut buf = BytesMut::new();
        buf.extend_from_slice(&first);
        buf.extend_from_slice(&second);

        assert_eq!(
            decode_frame(&mut buf).expect("decode first"),
            Some(sample_request())
        );
        assert_eq!(
            decode_frame(&mut buf).expect("decode second"),
            Some(AgentRequest::Shutdown)
        );
        assert!(decode_frame::<AgentRequest>(&mut buf).unwrap().is_none());
    }
}
