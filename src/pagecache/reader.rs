use std::fs::File;

use crate::pagecache::*;
use crate::*;

pub(crate) fn read_segment_header(
    file: &File,
    lid: LogOffset,
) -> Result<SegmentHeader> {
    trace!("reading segment header at {}", lid);

    let mut seg_header_buf = [0; SEG_HEADER_LEN];
    file.pread_exact(&mut seg_header_buf, lid)?;
    let segment_header = SegmentHeader::from(seg_header_buf);

    if segment_header.lsn < Lsn::try_from(lid).unwrap() {
        debug!(
            "segment had lsn {} but we expected something \
             greater, as the base lid is {}",
            segment_header.lsn, lid
        );
    }

    Ok(segment_header)
}

/// read a buffer from the disk
pub(crate) fn read_message(
    file: &File,
    lid: LogOffset,
    expected_lsn: Lsn,
    config: &Config,
) -> Result<LogRead> {
    let mut msg_header_buf = [0; MSG_HEADER_LEN];

    file.pread_exact(&mut msg_header_buf, lid)?;
    let header: MessageHeader = msg_header_buf.into();

    // we set the crc bytes to 0 because we will
    // calculate the crc32 over all bytes other
    // than the crc itfile, including the bytes
    // in the header.
    #[allow(unsafe_code)]
    unsafe {
        std::ptr::write_bytes(
            msg_header_buf
                .as_mut_ptr()
                .add(MSG_HEADER_LEN - std::mem::size_of::<u32>()),
            0xFF,
            std::mem::size_of::<u32>(),
        );
    }

    let _measure = Measure::new(&M.read);
    let segment_len = config.segment_size;
    let seg_start = lid / segment_len as LogOffset * segment_len as LogOffset;
    trace!("reading message from segment: {} at lid: {}", seg_start, lid);
    assert!(seg_start + SEG_HEADER_LEN as LogOffset <= lid);

    let ceiling = seg_start + segment_len as LogOffset;

    assert!(lid + MSG_HEADER_LEN as LogOffset <= ceiling);

    if header.lsn % segment_len as Lsn
        != Lsn::try_from(lid).unwrap() % segment_len as Lsn
    {
        let hb: [u8; MSG_HEADER_LEN] = header.into();
        // our message lsn was not aligned to our segment offset
        trace!(
            "read a message whose header lsn \
             is not aligned to its position \
             within its segment. header: {:?} \
             expected: relative offset {} bytes: {:?}",
            header,
            lid % segment_len as LogOffset,
            hb
        );
        return Ok(LogRead::Corrupted(header.len));
    }

    if header.lsn != expected_lsn {
        debug!(
            "header {:?} does not contain expected lsn {}",
            header, expected_lsn
        );
        return Ok(LogRead::Corrupted(header.len));
    }

    let max_possible_len =
        assert_usize(ceiling - lid - MSG_HEADER_LEN as LogOffset);

    if usize::try_from(header.len).unwrap() > max_possible_len {
        trace!(
            "read a corrupted message with impossibly long length {:?}",
            header
        );
        return Ok(LogRead::Corrupted(header.len));
    }

    if header.kind == MessageKind::Corrupted {
        trace!(
            "read a corrupted message with Corrupted MessageKind: {:?}",
            header
        );
        return Ok(LogRead::Corrupted(header.len));
    }

    // perform crc check on everything that isn't Corrupted
    let mut buf = Vec::with_capacity(usize::try_from(header.len).unwrap());
    #[allow(unsafe_code)]
    unsafe {
        buf.set_len(usize::try_from(header.len).unwrap());
    }
    file.pread_exact(&mut buf, lid + MSG_HEADER_LEN as LogOffset)?;

    // calculate the CRC32, calculating the hash on the
    // header afterwards
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(&buf);
    hasher.update(&msg_header_buf);

    let crc32 = hasher.finalize();

    if crc32 != header.crc32 {
        trace!("read a message with a bad checksum with header {:?}", header);
        return Ok(LogRead::Corrupted(header.len));
    }

    match header.kind {
        MessageKind::Cancelled => {
            trace!("read failed of len {}", header.len);
            Ok(LogRead::Failed(header.lsn, header.len))
        }
        MessageKind::Pad => {
            trace!("read pad at lsn {}", header.lsn);
            Ok(LogRead::Pad(header.lsn))
        }
        MessageKind::BlobLink
        | MessageKind::BlobNode
        | MessageKind::BlobMeta => {
            let id = arr_to_lsn(&buf);

            match read_blob(id, config) {
                Ok((kind, buf)) => {
                    assert_eq!(header.kind, kind);
                    trace!(
                        "read a successful blob message for Blob({}, {})",
                        header.lsn,
                        id,
                    );

                    Ok(LogRead::Blob(header, buf, id))
                }
                Err(Error::Io(ref e))
                    if e.kind() == std::io::ErrorKind::NotFound =>
                {
                    debug!(
                        "underlying blob file not found for Blob({}, {})",
                        header.lsn, id,
                    );
                    Ok(LogRead::DanglingBlob(header, id))
                }
                Err(other_e) => {
                    debug!("failed to read blob: {:?}", other_e);
                    Err(other_e)
                }
            }
        }
        MessageKind::InlineLink
        | MessageKind::InlineNode
        | MessageKind::InlineMeta
        | MessageKind::Free
        | MessageKind::Counter => {
            trace!("read a successful inline message");
            let buf = if config.use_compression {
                maybe_decompress(buf)?
            } else {
                buf
            };

            Ok(LogRead::Inline(header, buf, header.len))
        }
        MessageKind::BatchManifest => {
            assert_eq!(buf.len(), std::mem::size_of::<Lsn>());
            let max_lsn = arr_to_lsn(&buf);
            Ok(LogRead::BatchManifest(max_lsn))
        }
        MessageKind::Corrupted => panic!(
            "corrupted should have been handled \
             before reading message length above"
        ),
    }
}
