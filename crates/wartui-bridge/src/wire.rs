//! Host frames as the bytes either link writes, shared by USB and Bluetooth.

use wartui_proto::link::{HostToBridge, LinkError, encode_host_frame};

/// Encode `cmd` into `buf`, behind one `0x00` when `lead` is set, and return the length.
///
/// The writer leads a connection's first frame with that byte. Whatever reached the
/// board before this connection ends there, so the first frame decodes: a fresh tty
/// echoes what the bridge sent before raw mode was set, with no terminator in the echo,
/// and another program may have left bytes of its own. The byte shares the frame's
/// buffer, so the two stay one write and one packet (`serial::connect`).
///
/// # Errors
/// [`LinkError::BufferTooSmall`] if `buf` cannot hold the frame, or any error
/// [`encode_host_frame`] returns.
pub fn encode_for_wire(cmd: &HostToBridge, lead: bool, buf: &mut [u8]) -> Result<usize, LinkError> {
    let start = usize::from(lead);
    if lead {
        *buf.first_mut().ok_or(LinkError::BufferTooSmall)? = 0x00;
    }
    Ok(start + encode_host_frame(cmd, &mut buf[start..])?)
}

#[cfg(test)]
mod tests {
    use wartui_proto::link::{HostToBridge, MAX_HOST_FRAME, encode_host_frame};

    use super::encode_for_wire;

    #[test]
    fn wire_leads_with_terminator_when_frame_is_first_on_connection() {
        let mut expected = [0u8; MAX_HOST_FRAME];
        let len = encode_host_frame(&HostToBridge::Identify, &mut expected).unwrap();
        let mut buf = [0xFF_u8; MAX_HOST_FRAME + 1];
        let n = encode_for_wire(&HostToBridge::Identify, true, &mut buf).unwrap();
        assert_eq!(buf[0], 0x00);
        assert_eq!(&buf[1..n], &expected[..len]);
    }

    #[test]
    fn wire_writes_frame_alone_when_frame_is_not_first() {
        let mut expected = [0u8; MAX_HOST_FRAME];
        let len = encode_host_frame(&HostToBridge::Identify, &mut expected).unwrap();
        let mut buf = [0xFF_u8; MAX_HOST_FRAME + 1];
        let n = encode_for_wire(&HostToBridge::Identify, false, &mut buf).unwrap();
        assert_eq!(&buf[..n], &expected[..len]);
    }
}
