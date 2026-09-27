// Minimal MQTT v3.1.1 client for the Bambu local broker, written from the
// protocol specification (MQTT 3.1.1 is an OASIS standard). The packet
// formats here are the standard wire format, not any implementation's code.
//
// Why hand-rolled instead of a crate: this client's receive path is a
// cancellation-safe state machine. The main loop multiplexes MQTT reads
// against touch input and timers with `select`, which *abandons* the receive
// future at an arbitrary await point; a client that buffers partial reads in
// temporaries would lose them on drop and desync the stream. Here, all
// read-progress state lives in `self`, so a dropped future resumes cleanly.
//
// Scope: exactly what the printer needs - CONNECT/CONNACK, SUBSCRIBE/SUBACK,
// PUBLISH QoS 0 both ways, PINGREQ/PINGRESP, remaining-length varint decode.
// QoS 1/2, retain, LWT and MQTT 5 are all unnecessary against this broker.

use embedded_io_async::{Read, Write};

#[derive(Debug)]
pub enum Error<E> {
    Io(E),
    /// Broker refused the connection; payload is the CONNACK return code.
    ConnectionRefused(u8),
    /// Protocol violation from the broker (or stream desync).
    Protocol,
    /// A packet didn't fit the buffer we gave it.
    TooLarge,
    /// Broker didn't PINGRESP within the timeout.
    PingTimeout,
}

impl<E> From<E> for Error<E> {
    fn from(e: E) -> Self {
        Error::Io(e)
    }
}

impl<E> From<embedded_io_async::ReadExactError<E>> for Error<E> {
    fn from(e: embedded_io_async::ReadExactError<E>) -> Self {
        match e {
            embedded_io::ReadExactError::UnexpectedEof => Error::Protocol,
            embedded_io::ReadExactError::Other(e) => Error::Io(e),
        }
    }
}

/// An incoming PUBLISH, borrowed from the receive buffer.
pub struct Message<'a> {
    pub topic: &'a str,
    pub payload: &'a [u8],
}

/// Receive state machine. `None` = idle; `Some(Header)` = a fixed header has
/// been read and `remaining` more bytes are being read into the buffer.
#[derive(Default)]
enum RxState {
    #[default]
    Idle,
    /// (packet type nibble, remaining length, bytes read so far)
    Body(u8, usize, usize),
}

pub struct Client<'a, T> {
    transport: &'a mut T,
    rx: RxState,
    next_packet_id: u16,
}

impl<'a, T> Client<'a, T>
where
    T: Read + Write,
{
    pub fn new(transport: &'a mut T) -> Self {
        Self { transport, rx: RxState::Idle, next_packet_id: 1 }
    }

    /// CONNECT with username/password auth, wait for CONNACK.
    pub async fn connect(
        &mut self,
        client_id: &str,
        username: &str,
        password: &str,
        keep_alive_secs: u16,
    ) -> Result<(), Error<T::Error>> {
        // Variable header: protocol name, level 4, connect flags, keep-alive.
        let flags: u8 = 0x02        // clean session
            | 0x80                  // username flag
            | 0x40;                 // password flag
        let mut vh: heapless::Vec<u8, 512> = heapless::Vec::new();
        push_string(&mut vh, "MQTT").map_err(|_| Error::TooLarge)?;
        vh.push(0x04).map_err(|_| Error::TooLarge)?;
        vh.push(flags).map_err(|_| Error::TooLarge)?;
        vh.extend_from_slice(&keep_alive_secs.to_be_bytes())
            .map_err(|_| Error::TooLarge)?;
        push_string(&mut vh, client_id).map_err(|_| Error::TooLarge)?;
        push_string(&mut vh, username).map_err(|_| Error::TooLarge)?;
        push_string(&mut vh, password).map_err(|_| Error::TooLarge)?;

        write_packet(&mut self.transport, 0x10, &vh).await?;

        // CONNACK: fixed header 0x20, then 2-byte variable header.
        let mut head = [0u8; 2];
        self.transport.read_exact(&mut head).await?; // 0x20, remaining len 2
        if head[0] != 0x20 {
            return Err(Error::Protocol);
        }
        let mut body = [0u8; 2];
        self.transport.read_exact(&mut body).await?;
        if body[0] != 0x00 {
            return Err(Error::Protocol);
        }
        if body[1] != 0x00 {
            return Err(Error::ConnectionRefused(body[1]));
        }
        Ok(())
    }

    /// SUBSCRIBE (QoS 0) and wait for SUBACK.
    pub async fn subscribe(&mut self, topic: &str) -> Result<(), Error<T::Error>> {
        self.next_packet_id = self.next_packet_id.wrapping_add(1).max(1);
        let id = self.next_packet_id;

        let mut vh: heapless::Vec<u8, 256> = heapless::Vec::new();
        vh.extend_from_slice(&id.to_be_bytes()).map_err(|_| Error::TooLarge)?;
        push_string(&mut vh, topic).map_err(|_| Error::TooLarge)?;
        vh.push(0x00).map_err(|_| Error::TooLarge)?; // requested QoS 0

        write_packet(&mut self.transport, 0x82, &vh).await?;

        // SUBACK: header byte 0x90, varint remaining length, then packet id
        // + at least one return code.
        let mut hb = [0u8; 1];
        self.transport.read_exact(&mut hb).await?;
        if hb[0] != 0x90 {
            return Err(Error::Protocol);
        }
        let rem = decode_remaining_len(&mut self.transport).await?;
        if rem < 3 {
            return Err(Error::Protocol);
        }
        let mut body = [0u8; 4];
        self.transport.read_exact(&mut body[..rem.min(4)]).await?;
        // Skip any excess return codes (multi-topic SUBACKs we never send).
        let mut skip = rem.saturating_sub(4);
        let mut scratch = [0u8; 16];
        while skip > 0 {
            let n = skip.min(scratch.len());
            self.transport.read_exact(&mut scratch[..n]).await?;
            skip -= n;
        }
        let ack_id = u16::from_be_bytes([body[0], body[1]]);
        if ack_id != id {
            return Err(Error::Protocol);
        }
        if body[2] == 0x80 {
            return Err(Error::Protocol); // subscription rejected
        }
        Ok(())
    }

    /// PUBLISH (QoS 0, no retain).
    pub async fn publish(&mut self, topic: &str, payload: &[u8]) -> Result<(), Error<T::Error>> {
        let mut packet: heapless::Vec<u8, 512> = heapless::Vec::new();
        push_string(&mut packet, topic).map_err(|_| Error::TooLarge)?;
        packet.extend_from_slice(payload).map_err(|_| Error::TooLarge)?;
        write_packet(&mut self.transport, 0x30, &packet).await
    }

    /// PINGREQ + wait for PINGRESP.
    pub async fn ping(&mut self) -> Result<(), Error<T::Error>> {
        self.transport.write_all(&[0xC0, 0x00]).await?;
        let mut resp = [0u8; 2];
        self.transport.read_exact(&mut resp).await?;
        if resp != [0xD0, 0x00] {
            return Err(Error::Protocol);
        }
        Ok(())
    }

    /// Reads the next packet. PINGRESP and other non-PUBLISH packets are
    /// consumed silently; this returns only on a PUBLISH whose payload fits
    /// `buf`. Cancellation-safe: a dropped future leaves `rx` in a state the
    /// next call resumes from (progress is never lost).
    ///
    /// Returns `None` when the packet didn't fit `buf` (it is consumed and
    /// dropped); the caller should size `buf` for the largest pushall.
    pub async fn next_message<'buf>(
        &mut self,
        buf: &'buf mut [u8],
    ) -> Result<Option<Message<'buf>>, Error<T::Error>> {
        loop {
            // Phase 1 (not cancellable mid-read internally): fixed header +
            // remaining length, when idle.
            let (packet_type, remaining) = match self.rx {
                RxState::Idle => {
                    let mut head = [0u8; 1];
                    self.transport.read_exact(&mut head).await?;
                    let t = head[0];
                    let rem = decode_remaining_len(&mut self.transport).await?;
                    self.rx = RxState::Body(t, rem, 0);
                    (t, rem)
                }
                RxState::Body(t, rem, got) => (t, rem),
            };

            // Phase 2: read `remaining` bytes into `buf`, resuming from `got`.
            let got = match self.rx {
                RxState::Body(_, _, got) => got,
                RxState::Idle => unreachable!(),
            };
            let target = remaining.min(buf.len());
            if got < target {
                // Read only what fits; excess is drained below.
                self.transport
                    .read_exact(&mut buf[got..target])
                    .await?;
            }
            let mut consumed = remaining.min(buf.len());
            let mut excess = remaining - consumed;
            let mut scratch = [0u8; 64];
            while excess > 0 {
                let n = excess.min(scratch.len());
                self.transport.read_exact(&mut scratch[..n]).await?;
                excess -= n;
            }
            self.rx = RxState::Idle;

            if packet_type != 0x30 {
                continue; // PINGRESP etc: consumed, try again.
            }

            // PUBLISH variable header: topic length, topic, [packet id], payload.
            if consumed < 2 {
                return Err(Error::Protocol);
            }
            let topic_len = u16::from_be_bytes([buf[0], buf[1]]) as usize;
            let qos = (packet_type & 0x06) >> 1;
            let payload_start = 2 + topic_len + if qos > 0 { 2 } else { 0 };
            if payload_start > consumed {
                return Err(Error::Protocol);
            }
            let topic = core::str::from_utf8(&buf[2..2 + topic_len])
                .map_err(|_| Error::Protocol)?;
            consumed -= payload_start;
            let payload = &buf[payload_start..payload_start + consumed];
            if topic_len > 0 && consumed > 0 {
                return Ok(Some(Message { topic, payload }));
            }
            return Ok(None); // truncated: caller's buffer was too small
        }
    }
}

/// Writes one MQTT packet: fixed header with varint remaining length, then
/// the variable header + payload.
async fn write_packet<T: Write>(
    transport: &mut T,
    first_byte: u8,
    body: &[u8],
) -> Result<(), Error<T::Error>> {
    let mut head = [first_byte, 0, 0, 0, 0];
    let (head_len, rem) = encode_remaining_len(body.len());
    let _ = rem;
    transport.write_all(&head[..head_len]).await?;
    transport.write_all(body).await?;
    Ok(())
}

fn encode_remaining_len(mut len: usize) -> (usize, usize) {
    let mut out = [0u8; 4];
    let mut n = 0;
    loop {
        let mut byte = (len % 128) as u8;
        len /= 128;
        if len > 0 {
            byte |= 0x80;
        }
        out[n] = byte;
        n += 1;
        if len == 0 {
            return (n, len);
        }
    }
}

async fn decode_remaining_len<T: Read>(
    transport: &mut T,
) -> Result<usize, Error<T::Error>> {
    let mut value = 0usize;
    let mut multiplier = 1usize;
    loop {
        let mut byte = [0u8; 1];
        transport.read_exact(&mut byte).await?;
        value += ((byte[0] & 0x7F) as usize) * multiplier;
        if byte[0] & 0x80 == 0 {
            return Ok(value);
        }
        multiplier *= 128;
        if multiplier > 128 * 128 * 128 {
            return Err(Error::Protocol);
        }
    }
}

fn push_string<const N: usize>(out: &mut heapless::Vec<u8, N>, s: &str) -> Result<(), ()> {
    let bytes = s.as_bytes();
    out.extend_from_slice(&(bytes.len() as u16).to_be_bytes()).map_err(|_| ())?;
    out.extend_from_slice(bytes).map_err(|_| ())
}
