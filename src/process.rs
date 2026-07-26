use tokio::io::{AsyncRead, AsyncReadExt};

use crate::{AppError, Result};

pub(crate) async fn read_limited(
    mut reader: impl AsyncRead + Unpin,
    limit: usize,
    program: String,
    stream: &'static str,
) -> Result<Vec<u8>> {
    let mut bytes = Vec::with_capacity(limit.min(64 * 1024));
    let mut chunk = [0_u8; 16 * 1024];
    let mut exceeded = false;
    loop {
        let read = reader.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        let remaining = limit.saturating_sub(bytes.len());
        bytes.extend_from_slice(&chunk[..read.min(remaining)]);
        exceeded |= read > remaining;
    }
    if exceeded {
        Err(AppError::ExternalOutputTooLarge {
            program,
            stream,
            limit,
        })
    } else {
        Ok(bytes)
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::AsyncWriteExt;

    use super::*;

    #[tokio::test]
    async fn rejects_output_over_the_capture_limit() {
        let (mut writer, reader) = tokio::io::duplex(16);
        writer.write_all(b"1234").await.unwrap();
        drop(writer);

        let result = read_limited(reader, 3, "test".to_owned(), "stdout").await;
        assert!(matches!(
            result,
            Err(AppError::ExternalOutputTooLarge {
                stream: "stdout",
                limit: 3,
                ..
            })
        ));
    }
}
