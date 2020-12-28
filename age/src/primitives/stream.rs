//! I/O helper structs for age file encryption and decryption.

use chacha20poly1305::{
    aead::{
        self,
        generic_array::{typenum::U12, GenericArray},
        stream::{Decryptor, Encryptor, StreamPrimitive},
        Aead, AeadInPlace, NewAead,
    },
    ChaChaPoly1305,
};
use pin_project::pin_project;
use secrecy::{ExposeSecret, SecretVec};
use std::cmp;
use std::io::{self, Read, Seek, SeekFrom, Write};
use zeroize::Zeroize;

#[cfg(feature = "async")]
use futures::{
    io::{AsyncRead, AsyncWrite, Error},
    ready,
    task::{Context, Poll},
};
#[cfg(feature = "async")]
use std::pin::Pin;

const CHUNK_SIZE: usize = 64 * 1024;
const TAG_SIZE: usize = 16;
const ENCRYPTED_CHUNK_SIZE: usize = CHUNK_SIZE + TAG_SIZE;

type AgeEncryptor = Encryptor<ChaChaPoly1305<c2_chacha::Ietf>, Stream>;
type AgeDecryptor = Decryptor<ChaChaPoly1305<c2_chacha::Ietf>, Stream>;

pub(crate) struct PayloadKey(
    pub(crate) GenericArray<u8, <ChaChaPoly1305<c2_chacha::Ietf> as NewAead>::KeySize>,
);

impl Drop for PayloadKey {
    fn drop(&mut self) {
        self.0.as_mut_slice().zeroize();
    }
}

#[cfg(feature = "async")]
struct EncryptedChunk {
    bytes: Vec<u8>,
    offset: usize,
}

/// `STREAM[key](plaintext)`
///
/// The [STREAM] construction for online authenticated encryption, instantiated with
/// ChaCha20-Poly1305 in 64KiB chunks, and a nonce structure of 11 bytes of big endian
/// counter, and 1 byte of last block flag (0x00 / 0x01).
///
/// [STREAM]: https://eprint.iacr.org/2015/189.pdf
pub(crate) struct Stream {
    aead: ChaChaPoly1305<c2_chacha::Ietf>,
}

impl Stream {
    fn new(key: PayloadKey) -> Self {
        Stream {
            aead: ChaChaPoly1305::new(&key.0),
        }
    }

    /// Wraps `STREAM` encryption under the given `key` around a writer.
    ///
    /// `key` must **never** be repeated across multiple streams. In `age` this is
    /// achieved by deriving the key with [`HKDF`] from both a random file key and a
    /// random nonce.
    ///
    /// [`HKDF`]: age_core::primitives::hkdf
    pub(crate) fn encrypt<W: Write>(key: PayloadKey, inner: W) -> StreamWriter<W> {
        StreamWriter {
            stream: Self::new(key).encryptor(),
            inner,
            chunk: Vec::with_capacity(CHUNK_SIZE),
            #[cfg(feature = "async")]
            encrypted_chunk: None,
        }
    }

    /// Wraps `STREAM` encryption under the given `key` around a writer.
    ///
    /// `key` must **never** be repeated across multiple streams. In `age` this is
    /// achieved by deriving the key with [`HKDF`] from both a random file key and a
    /// random nonce.
    ///
    /// [`HKDF`]: age_core::primitives::hkdf
    #[cfg(feature = "async")]
    pub(crate) fn encrypt_async<W: AsyncWrite>(key: PayloadKey, inner: W) -> StreamWriter<W> {
        StreamWriter {
            stream: Self::new(key).encryptor(),
            inner,
            chunk: Vec::with_capacity(CHUNK_SIZE),
            encrypted_chunk: None,
        }
    }

    /// Wraps `STREAM` decryption under the given `key` around a reader.
    ///
    /// `key` must **never** be repeated across multiple streams. In `age` this is
    /// achieved by deriving the key with [`HKDF`] from both a random file key and a
    /// random nonce.
    ///
    /// [`HKDF`]: age_core::primitives::hkdf
    pub(crate) fn decrypt<R: Read>(key: PayloadKey, inner: R) -> StreamReader<R> {
        StreamReader {
            stream: Self::new(key).decryptor(),
            inner,
            encrypted_chunk: vec![0; ENCRYPTED_CHUNK_SIZE],
            encrypted_pos: 0,
            start: StartPos::Implicit(0),
            cur_plaintext_pos: 0,
            chunk: None,
        }
    }

    /// Wraps `STREAM` decryption under the given `key` around a reader.
    ///
    /// `key` must **never** be repeated across multiple streams. In `age` this is
    /// achieved by deriving the key with [`HKDF`] from both a random file key and a
    /// random nonce.
    ///
    /// [`HKDF`]: age_core::primitives::hkdf
    #[cfg(feature = "async")]
    pub(crate) fn decrypt_async<R: AsyncRead>(key: PayloadKey, inner: R) -> StreamReader<R> {
        StreamReader {
            stream: Self::new(key).decryptor(),
            inner,
            encrypted_chunk: vec![0; ENCRYPTED_CHUNK_SIZE],
            encrypted_pos: 0,
            start: StartPos::Implicit(0),
            cur_plaintext_pos: 0,
            chunk: None,
        }
    }

    /// Computes the nonce used in age's STREAM encryption.
    ///
    /// Structured as an 11 bytes of big endian counter, and 1 byte of last block flag
    /// (`0x00 / 0x01`). We store this in the lower 12 bytes of a `u128`.
    fn aead_nonce(
        &self,
        position: u128,
        last_block: bool,
    ) -> Result<aead::Nonce<<ChaChaPoly1305<c2_chacha::Ietf> as AeadInPlace>::NonceSize>, aead::Error>
    {
        if position > Self::COUNTER_MAX {
            return Err(aead::Error);
        }

        let position_with_flag = position | (last_block as u128);

        let mut result = GenericArray::default();
        result.copy_from_slice(&position_with_flag.to_be_bytes()[4..]);

        Ok(result)
    }
}

impl StreamPrimitive<ChaChaPoly1305<c2_chacha::Ietf>> for Stream {
    type NonceOverhead = U12;
    type Counter = u128;
    const COUNTER_INCR: u128 = 1 << 8;
    const COUNTER_MAX: u128 = 0xffffffff_ffffffff_ffffff00;

    fn encrypt_in_place(
        &self,
        position: Self::Counter,
        last_block: bool,
        associated_data: &[u8],
        buffer: &mut dyn aead::Buffer,
    ) -> Result<(), aead::Error> {
        let nonce = self.aead_nonce(position, last_block)?;
        self.aead.encrypt_in_place(&nonce, associated_data, buffer)
    }

    fn decrypt_in_place(
        &self,
        position: Self::Counter,
        last_block: bool,
        associated_data: &[u8],
        buffer: &mut dyn aead::Buffer,
    ) -> Result<(), aead::Error> {
        let nonce = self.aead_nonce(position, last_block)?;
        self.aead.decrypt_in_place(&nonce, associated_data, buffer)
    }
}

/// Writes an encrypted age file.
#[pin_project(project = StreamWriterProj)]
pub struct StreamWriter<W> {
    stream: AgeEncryptor,
    #[pin]
    inner: W,
    chunk: Vec<u8>,
    #[cfg(feature = "async")]
    encrypted_chunk: Option<EncryptedChunk>,
}

impl<W: Write> StreamWriter<W> {
    /// Writes the final chunk of the age file.
    ///
    /// You **MUST** call `finish` when you are done writing, in order to finish the
    /// encryption process. Failing to call `finish` will result in a truncated file that
    /// that will fail to decrypt.
    pub fn finish(mut self) -> io::Result<W> {
        self.stream
            .encrypt_last_in_place(&[], &mut self.chunk)
            .map_err(|_| {
                // We will never hit chacha20::MAX_BLOCKS because of the chunk
                // size, so this is the only possible error.
                io::Error::new(io::ErrorKind::WriteZero, "last chunk has been processed")
            })?;
        self.inner.write_all(&self.chunk)?;
        Ok(self.inner)
    }
}

impl<W: Write> Write for StreamWriter<W> {
    fn write(&mut self, mut buf: &[u8]) -> io::Result<usize> {
        let mut bytes_written = 0;

        while !buf.is_empty() {
            let to_write = cmp::min(CHUNK_SIZE - self.chunk.len(), buf.len());
            self.chunk.extend_from_slice(&buf[..to_write]);
            bytes_written += to_write;
            buf = &buf[to_write..];

            // At this point, either buf is empty, or we have a full chunk.
            assert!(buf.is_empty() || self.chunk.len() == CHUNK_SIZE);

            // Only encrypt the chunk if we have more data to write, as the last
            // chunk must be written in finish().
            if !buf.is_empty() {
                self.stream
                    .encrypt_next_in_place(&[], &mut self.chunk)
                    .map_err(|_| {
                        // We will never hit chacha20::MAX_BLOCKS because of the chunk
                        // size, so this is the only possible error.
                        io::Error::new(io::ErrorKind::WriteZero, "last chunk has been processed")
                    })?;
                self.inner.write_all(&self.chunk)?;
                self.chunk.clear();
            }
        }

        Ok(bytes_written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(feature = "async")]
impl<W: AsyncWrite> StreamWriter<W> {
    fn poll_flush_chunk(self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<()>> {
        let StreamWriterProj {
            mut inner,
            encrypted_chunk,
            ..
        } = self.project();

        if let Some(chunk) = encrypted_chunk {
            loop {
                chunk.offset +=
                    ready!(inner.as_mut().poll_write(cx, &chunk.bytes[chunk.offset..]))?;
                if chunk.offset == chunk.bytes.len() {
                    break;
                }
            }
        }
        *encrypted_chunk = None;

        Poll::Ready(Ok(()))
    }
}

#[cfg(feature = "async")]
impl<W: AsyncWrite> AsyncWrite for StreamWriter<W> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context,
        mut buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        ready!(self.as_mut().poll_flush_chunk(cx))?;

        let to_write = cmp::min(CHUNK_SIZE - self.chunk.len(), buf.len());

        self.as_mut()
            .project()
            .chunk
            .extend_from_slice(&buf[..to_write]);
        buf = &buf[to_write..];

        // At this point, either buf is empty, or we have a full chunk.
        assert!(buf.is_empty() || self.chunk.len() == CHUNK_SIZE);

        // Only encrypt the chunk if we have more data to write, as the last
        // chunk must be written in poll_close().
        if !buf.is_empty() {
            let this = self.as_mut().project();
            let mut bytes = this.chunk.clone();
            this.stream
                .encrypt_next_in_place(&[], &mut bytes)
                .map_err(|_| {
                    // We will never hit chacha20::MAX_BLOCKS because of the chunk
                    // size, so this is the only possible error.
                    io::Error::new(io::ErrorKind::WriteZero, "last chunk has been processed")
                })?;
            *this.encrypted_chunk = Some(EncryptedChunk { bytes, offset: 0 });
            this.chunk.clear();
        }

        Poll::Ready(Ok(to_write))
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<()>> {
        ready!(self.as_mut().poll_flush_chunk(cx))?;
        self.project().inner.poll_flush(cx)
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // Flush any remaining encrypted chunk bytes.
        ready!(self.as_mut().poll_flush_chunk(cx))?;

        if !self.chunk.is_empty() {
            // Finish the stream.
            let this = self.as_mut().project();
            let mut bytes = this.chunk.clone();
            this.stream
                .encrypt_last_in_place(&[], &mut bytes)
                .map_err(|_| {
                    // We will never hit chacha20::MAX_BLOCKS because of the chunk
                    // size, so this is the only possible error.
                    io::Error::new(io::ErrorKind::WriteZero, "last chunk has been processed")
                })?;
            *this.encrypted_chunk = Some(EncryptedChunk { bytes, offset: 0 });
            this.chunk.clear();
        }

        // Flush the final chunk (if we didn't in the first call).
        ready!(self.as_mut().poll_flush_chunk(cx))?;
        self.project().inner.poll_close(cx)
    }
}

/// The position in the underlying reader corresponding to the start of the stream.
///
/// To impl Seek for StreamReader, we need to know the point in the reader corresponding
/// to the first byte of the stream. But we can't query the reader for its current
/// position without having a specific constructor for `R: Read + Seek`, which makes the
/// higher-level API more complex. Instead, we count the number of bytes that have been
/// read from the reader until we first need to seek, and then inside `impl Seek` we can
/// query the reader's current position and figure out where the start was.
enum StartPos {
    /// An offset that we can subtract from the current position.
    Implicit(u64),
    /// The precise start position.
    Explicit(u64),
}

/// Provides access to a decrypted age file.
#[pin_project]
pub struct StreamReader<R> {
    stream: AgeDecryptor,
    #[pin]
    inner: R,
    encrypted_chunk: Vec<u8>,
    encrypted_pos: usize,
    start: StartPos,
    cur_plaintext_pos: u64,
    chunk: Option<SecretVec<u8>>,
}

impl<R> StreamReader<R> {
    fn count_bytes(&mut self, read: usize) {
        // We only need to count if we haven't yet worked out the start position.
        if let StartPos::Implicit(offset) = &mut self.start {
            *offset += read as u64;
        }
    }

    fn decrypt_chunk(&mut self) -> io::Result<()> {
        self.count_bytes(self.encrypted_pos);
        let chunk = &self.encrypted_chunk[..self.encrypted_pos];

        if chunk.is_empty() {
            // TODO
            // if !self.stream.is_complete() {
            //     // Stream has ended before seeing the last chunk.
            //     return Err(io::Error::new(
            //         io::ErrorKind::UnexpectedEof,
            //         "age file is truncated",
            //     ));
            // }
        } else {
            // This check works for all cases except when the age file is an integer
            // multiple of the chunk size. In that case, we try decrypting twice on a
            // decryption failure.
            let last = chunk.len() < ENCRYPTED_CHUNK_SIZE;

            let mut buffer = chunk.to_owned();
            let res = if last {
                self.stream.decrypt_last_in_place(&[], &mut buffer)
            } else {
                self.stream.decrypt_next_in_place(&[], &mut buffer)
            };

            self.chunk = match (res, last) {
                (Ok(()), _) => Some(SecretVec::new(buffer)),
                (Err(_), false) => {
                    // We need to re-clone the encrypted bytes, because the buffer is
                    // clobbered in case of an error.
                    let mut buffer = chunk.to_owned();
                    self.stream
                        .decrypt_last_in_place(&[], &mut buffer)
                        .map_err(|_| {
                            io::Error::new(
                                io::ErrorKind::InvalidData,
                                "last chunk has been processed",
                            )
                        })?;
                    Some(SecretVec::new(buffer))
                }
                (Err(_), true) => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "last chunk has been processed",
                    ))
                }
            };
        }

        // We've finished with this encrypted chunk.
        self.encrypted_pos = 0;

        Ok(())
    }

    fn read_from_chunk(&mut self, buf: &mut [u8]) -> usize {
        if self.chunk.is_none() {
            return 0;
        }

        let chunk = self.chunk.as_ref().unwrap();
        let cur_chunk_offset = self.cur_plaintext_pos as usize % CHUNK_SIZE;

        let to_read = cmp::min(chunk.expose_secret().len() - cur_chunk_offset, buf.len());

        buf[..to_read]
            .copy_from_slice(&chunk.expose_secret()[cur_chunk_offset..cur_chunk_offset + to_read]);
        self.cur_plaintext_pos += to_read as u64;
        if self.cur_plaintext_pos % CHUNK_SIZE as u64 == 0 {
            // We've finished with the current chunk.
            self.chunk = None;
        }

        to_read
    }
}

impl<R: Read> Read for StreamReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.chunk.is_none() {
            while self.encrypted_pos < ENCRYPTED_CHUNK_SIZE {
                match self
                    .inner
                    .read(&mut self.encrypted_chunk[self.encrypted_pos..])
                {
                    Ok(0) => break,
                    Ok(n) => self.encrypted_pos += n,
                    Err(e) => match e.kind() {
                        io::ErrorKind::Interrupted => (),
                        _ => return Err(e),
                    },
                }
            }
            self.decrypt_chunk()?;
        }

        Ok(self.read_from_chunk(buf))
    }
}

#[cfg(feature = "async")]
impl<R: AsyncRead + Unpin> AsyncRead for StreamReader<R> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context,
        buf: &mut [u8],
    ) -> Poll<Result<usize, Error>> {
        if self.chunk.is_none() {
            while self.encrypted_pos < ENCRYPTED_CHUNK_SIZE {
                let this = self.as_mut().project();
                match ready!(this
                    .inner
                    .poll_read(cx, &mut this.encrypted_chunk[*this.encrypted_pos..]))
                {
                    Ok(0) => break,
                    Ok(n) => self.encrypted_pos += n,
                    Err(e) => match e.kind() {
                        io::ErrorKind::Interrupted => (),
                        _ => return Poll::Ready(Err(e)),
                    },
                }
            }
            self.decrypt_chunk()?;
        }

        Poll::Ready(Ok(self.read_from_chunk(buf)))
    }
}

impl<R: Read + Seek> StreamReader<R> {
    fn start(&mut self) -> io::Result<u64> {
        match self.start {
            StartPos::Implicit(offset) => {
                let current = self.inner.seek(SeekFrom::Current(0))?;
                let start = current - offset;

                // Cache the start for future calls.
                self.start = StartPos::Explicit(start);

                Ok(start)
            }
            StartPos::Explicit(start) => Ok(start),
        }
    }
}

impl<R: Read + Seek> Seek for StreamReader<R> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        // Convert the offset into the target position within the plaintext
        let start = self.start()?;
        let target_pos = match pos {
            SeekFrom::Start(offset) => offset,
            SeekFrom::Current(offset) => {
                let res = (self.cur_plaintext_pos as i64) + offset;
                if res >= 0 {
                    res as u64
                } else {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "cannot seek before the start",
                    ));
                }
            }
            SeekFrom::End(offset) => {
                let cur_pos = self.inner.seek(SeekFrom::Current(0))?;
                let ct_end = self.inner.seek(SeekFrom::End(0))?;
                self.inner.seek(SeekFrom::Start(cur_pos))?;

                let num_chunks = (ct_end / ENCRYPTED_CHUNK_SIZE as u64) + 1;
                let total_tag_size = num_chunks * TAG_SIZE as u64;
                let pt_end = ct_end - start - total_tag_size;

                let res = (pt_end as i64) + offset;
                if res >= 0 {
                    res as u64
                } else {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "cannot seek before the start",
                    ));
                }
            }
        };

        let cur_chunk_index = self.cur_plaintext_pos / CHUNK_SIZE as u64;

        let target_chunk_index = target_pos / CHUNK_SIZE as u64;
        let target_chunk_offset = target_pos % CHUNK_SIZE as u64;

        if target_chunk_index == cur_chunk_index {
            // We just need to reposition ourselves within the current chunk.
            self.cur_plaintext_pos = target_pos;
        } else {
            // Clear the current chunk
            self.chunk = None;

            // Seek to the beginning of the target chunk
            self.inner.seek(SeekFrom::Start(
                start + (target_chunk_index * ENCRYPTED_CHUNK_SIZE as u64),
            ))?;
            // TODO: Fix once aead::stream is seekable
            // self.stream.nonce.set_counter(target_chunk_index);
            self.cur_plaintext_pos = target_chunk_index * CHUNK_SIZE as u64;

            // Read and drop bytes from the chunk to reach the target position.
            if target_chunk_offset > 0 {
                let mut to_drop = vec![0; target_chunk_offset as usize];
                self.read_exact(&mut to_drop)?;
            }
        }

        // All done!
        Ok(target_pos)
    }
}

#[cfg(test)]
mod tests {
    use chacha20poly1305::aead::stream::StreamPrimitive;
    use secrecy::ExposeSecret;
    use std::io::{self, Cursor, Read, Seek, SeekFrom, Write};

    use super::{PayloadKey, Stream, CHUNK_SIZE};

    #[cfg(feature = "async")]
    use futures::{
        io::{AsyncRead, AsyncWrite},
        pin_mut,
        task::Poll,
    };
    #[cfg(feature = "async")]
    use futures_test::task::noop_context;

    #[test]
    fn chunk_round_trip() {
        let data = vec![42; CHUNK_SIZE];

        let mut encrypted = data.clone();
        {
            let mut s = Stream::new(PayloadKey([7; 32].into())).encryptor();
            s.encrypt_next_in_place(&[], &mut encrypted).unwrap()
        };

        let decrypted = encrypted.clone();
        {
            let mut s = Stream::new(PayloadKey([7; 32].into())).decryptor();
            s.decrypt_next_in_place(&[], &mut decrypted).unwrap();
        }

        assert_eq!(&decrypted, &data);
    }

    // #[test]
    // fn last_chunk_round_trip() {
    //     let data = vec![42; CHUNK_SIZE];

    //     let encrypted = {
    //         let mut s = Stream::new(PayloadKey([7; 32].into()));
    //         let res = s.encrypt_chunk(&data, true).unwrap();

    //         // Further calls return an error
    //         assert_eq!(
    //             s.encrypt_chunk(&data, false).unwrap_err().kind(),
    //             io::ErrorKind::WriteZero
    //         );
    //         assert_eq!(
    //             s.encrypt_chunk(&data, true).unwrap_err().kind(),
    //             io::ErrorKind::WriteZero
    //         );

    //         res
    //     };

    //     let decrypted = {
    //         let mut s = Stream::new(PayloadKey([7; 32].into()));
    //         let res = s.decrypt_chunk(&encrypted, true).unwrap();

    //         // Further calls return an error
    //         match s.decrypt_chunk(&encrypted, false) {
    //             Err(e) => assert_eq!(e.kind(), io::ErrorKind::UnexpectedEof),
    //             _ => panic!("Expected error"),
    //         }
    //         match s.decrypt_chunk(&encrypted, true) {
    //             Err(e) => assert_eq!(e.kind(), io::ErrorKind::UnexpectedEof),
    //             _ => panic!("Expected error"),
    //         }

    //         res
    //     };

    //     assert_eq!(decrypted.expose_secret(), &data);
    // }

    fn stream_round_trip(data: &[u8]) {
        let mut encrypted = vec![];
        {
            let mut w = Stream::encrypt(PayloadKey([7; 32].into()), &mut encrypted);
            w.write_all(&data).unwrap();
            w.finish().unwrap();
        };

        let decrypted = {
            let mut buf = vec![];
            let mut r = Stream::decrypt(PayloadKey([7; 32].into()), &encrypted[..]);
            r.read_to_end(&mut buf).unwrap();
            buf
        };

        assert_eq!(decrypted, data);
    }

    #[test]
    fn stream_round_trip_short() {
        stream_round_trip(&vec![42; 1024]);
    }

    #[test]
    fn stream_round_trip_chunk() {
        stream_round_trip(&vec![42; CHUNK_SIZE]);
    }

    #[test]
    fn stream_round_trip_long() {
        stream_round_trip(&vec![42; 100 * 1024]);
    }

    #[cfg(feature = "async")]
    fn stream_async_round_trip(data: &[u8]) {
        let mut encrypted = vec![];
        {
            let w = Stream::encrypt_async(PayloadKey([7; 32].into()), &mut encrypted);
            pin_mut!(w);

            let mut cx = noop_context();

            let mut tmp = data;
            loop {
                match w.as_mut().poll_write(&mut cx, &mut tmp) {
                    Poll::Ready(Ok(0)) => break,
                    Poll::Ready(Ok(written)) => tmp = &tmp[written..],
                    Poll::Ready(Err(e)) => panic!("Unexpected error: {}", e),
                    Poll::Pending => panic!("Unexpected Pending"),
                }
            }
            loop {
                match w.as_mut().poll_close(&mut cx) {
                    Poll::Ready(Ok(())) => break,
                    Poll::Ready(Err(e)) => panic!("Unexpected error: {}", e),
                    Poll::Pending => panic!("Unexpected Pending"),
                }
            }
        };

        let decrypted = {
            let mut buf = vec![];
            let r = Stream::decrypt_async(PayloadKey([7; 32].into()), &encrypted[..]);
            pin_mut!(r);

            let mut cx = noop_context();

            let mut tmp = [0; 4096];
            loop {
                match r.as_mut().poll_read(&mut cx, &mut tmp) {
                    Poll::Ready(Ok(0)) => break buf,
                    Poll::Ready(Ok(read)) => buf.extend_from_slice(&tmp[..read]),
                    Poll::Ready(Err(e)) => panic!("Unexpected error: {}", e),
                    Poll::Pending => panic!("Unexpected Pending"),
                }
            }
        };

        assert_eq!(decrypted, data);
    }

    #[cfg(feature = "async")]
    #[test]
    fn stream_async_round_trip_short() {
        stream_async_round_trip(&vec![42; 1024]);
    }

    #[cfg(feature = "async")]
    #[test]
    fn stream_async_round_trip_chunk() {
        stream_async_round_trip(&vec![42; CHUNK_SIZE]);
    }

    #[cfg(feature = "async")]
    #[test]
    fn stream_async_round_trip_long() {
        stream_async_round_trip(&vec![42; 100 * 1024]);
    }

    #[test]
    fn stream_fails_to_decrypt_truncated_file() {
        let data = vec![42; 2 * CHUNK_SIZE];

        let mut encrypted = vec![];
        {
            let mut w = Stream::encrypt(PayloadKey([7; 32].into()), &mut encrypted);
            w.write_all(&data).unwrap();
            // Forget to call w.finish()!
        };

        let mut buf = vec![];
        let mut r = Stream::decrypt(PayloadKey([7; 32].into()), &encrypted[..]);
        assert_eq!(
            r.read_to_end(&mut buf).unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
    }

    #[test]
    fn stream_seeking() {
        let mut data = vec![0; 100 * 1024];
        for (i, b) in data.iter_mut().enumerate() {
            *b = i as u8;
        }

        let mut encrypted = vec![];
        {
            let mut w = Stream::encrypt(PayloadKey([7; 32].into()), &mut encrypted);
            w.write_all(&data).unwrap();
            w.finish().unwrap();
        };

        let mut r = Stream::decrypt(PayloadKey([7; 32].into()), Cursor::new(encrypted));

        // Read through into the second chunk
        let mut buf = vec![0; 100];
        for i in 0..700 {
            r.read_exact(&mut buf).unwrap();
            assert_eq!(&buf[..], &data[100 * i..100 * (i + 1)]);
        }

        // Seek back into the first chunk
        r.seek(SeekFrom::Start(250)).unwrap();
        r.read_exact(&mut buf).unwrap();
        assert_eq!(&buf[..], &data[250..350]);

        // Seek forwards within this chunk
        r.seek(SeekFrom::Current(510)).unwrap();
        r.read_exact(&mut buf).unwrap();
        assert_eq!(&buf[..], &data[860..960]);

        // Seek backwards from the end
        r.seek(SeekFrom::End(-1337)).unwrap();
        r.read_exact(&mut buf).unwrap();
        assert_eq!(&buf[..], &data[data.len() - 1337..data.len() - 1237]);
    }
}
