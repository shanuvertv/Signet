//! Framing for POP3: a CRLF line codec with dot-unstuffing, over an optionally-TLS stream.

use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;
use tokio_rustls::rustls::pki_types::ServerName;

use crate::{Error, Result};

/// RFC 1939 caps status lines at 512 octets. Data lines have no stated limit, but a line longer
/// than this in a message body means something is very wrong, and an unbounded buffer here is a
/// trivial memory-exhaustion vector on a phone.
const MAX_LINE: usize = 1024 * 1024;

const READ_CHUNK: usize = 16 * 1024;

/// Compact the read buffer once this much has been consumed, so long messages do not grow it
/// without bound.
const COMPACT_THRESHOLD: usize = 64 * 1024;

/// A TCP stream that may or may not have been upgraded to TLS.
///
/// Modelled as an enum rather than `Box<dyn AsyncRead + AsyncWrite>` specifically so that
/// [`Transport::upgrade`] can recover the raw `TcpStream` for the STARTTLS handshake.
#[derive(Debug)]
pub(crate) enum Transport {
    Plain(TcpStream),
    Tls(Box<TlsStream<TcpStream>>),
    /// In-memory stream used by the protocol tests, so framing and error paths can be exercised
    /// without a listening socket.
    #[cfg(test)]
    Mock(tokio::io::DuplexStream),
}

impl Transport {
    pub(crate) async fn upgrade(
        self,
        connector: &TlsConnector,
        server: ServerName<'static>,
    ) -> Result<Self> {
        match self {
            Transport::Plain(tcp) => {
                let tls = connector
                    .connect(server, tcp)
                    .await
                    .map_err(|e| Error::Tls(e.to_string()))?;
                Ok(Transport::Tls(Box::new(tls)))
            }
            already => Ok(already),
        }
    }

    pub(crate) fn is_encrypted(&self) -> bool {
        matches!(self, Transport::Tls(_))
    }
}

macro_rules! delegate {
    ($self:expr, $s:ident => $body:expr) => {
        match $self {
            Transport::Plain($s) => $body,
            Transport::Tls(boxed) => {
                let $s = boxed.as_mut();
                $body
            }
            #[cfg(test)]
            Transport::Mock($s) => $body,
        }
    };
}

impl AsyncRead for Transport {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        delegate!(self.get_mut(), s => Pin::new(s).poll_read(cx, buf))
    }
}

impl AsyncWrite for Transport {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        delegate!(self.get_mut(), s => Pin::new(s).poll_write(cx, buf))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        delegate!(self.get_mut(), s => Pin::new(s).poll_flush(cx))
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        delegate!(self.get_mut(), s => Pin::new(s).poll_shutdown(cx))
    }
}

/// The verb of a command line, i.e. everything a log is allowed to see.
fn verb_of(cmd: &str) -> &str {
    cmd.split(' ').next().unwrap_or(cmd)
}

/// A framed POP3 connection.
#[derive(Debug)]
pub(crate) struct Conn {
    pub(crate) stream: Transport,
    buf: Vec<u8>,
    pos: usize,
    pub(crate) timeout: Duration,
}

impl Conn {
    pub(crate) fn new(stream: Transport, timeout: Duration) -> Self {
        Self {
            stream,
            buf: Vec::with_capacity(READ_CHUNK),
            pos: 0,
            timeout,
        }
    }

    /// Bytes received but not yet consumed. Used by the STARTTLS injection check.
    pub(crate) fn pending(&self) -> usize {
        self.buf.len() - self.pos
    }

    pub(crate) fn is_encrypted(&self) -> bool {
        self.stream.is_encrypted()
    }

    /// Consume `self`, keeping only the stream and settings.
    ///
    /// Callers must check [`Conn::pending`] first: any buffered bytes are silently dropped, which
    /// is only safe once we have established there are none.
    pub(crate) fn into_parts(self) -> (Transport, Duration) {
        (self.stream, self.timeout)
    }

    /// Send a command and read its status line, returning the text after `+OK`.
    ///
    /// `cmd` may contain credentials, so only the verb is ever logged. See PLAN.md §7.8.
    pub(crate) async fn command(&mut self, cmd: &str) -> Result<String> {
        let verb = verb_of(cmd);
        tracing::trace!(verb, "pop3 >>");
        self.write_line(cmd).await?;
        let status = self.read_status(verb).await;
        match &status {
            Ok(_) => tracing::trace!(verb, "pop3 << +OK"),
            Err(e) => tracing::debug!(verb, error = %e, "pop3 << error"),
        }
        status
    }

    /// Read a `+OK` / `-ERR` status line. `ctx` names the command for the error message.
    pub(crate) async fn read_status(&mut self, ctx: &str) -> Result<String> {
        let line = self.read_line().await?;
        if let Some(rest) = line.strip_prefix("+OK") {
            Ok(rest.trim_start().to_string())
        } else if let Some(rest) = line.strip_prefix("-ERR") {
            Err(Error::Rejected {
                command: ctx.to_string(),
                message: rest.trim_start().to_string(),
            })
        } else {
            Err(Error::Protocol(format!(
                "expected +OK or -ERR from {ctx}, got {line:?}"
            )))
        }
    }

    pub(crate) async fn write_line(&mut self, line: &str) -> Result<()> {
        let deadline = self.timeout;
        let fut = async {
            self.stream.write_all(line.as_bytes()).await?;
            self.stream.write_all(b"\r\n").await?;
            self.stream.flush().await?;
            Ok::<_, std::io::Error>(())
        };
        tokio::time::timeout(deadline, fut)
            .await
            .map_err(|_| Error::Timeout(deadline))??;
        Ok(())
    }

    async fn fill(&mut self) -> Result<()> {
        use tokio::io::AsyncReadExt;

        let start = self.buf.len();
        self.buf.resize(start + READ_CHUNK, 0);
        let n = tokio::time::timeout(self.timeout, self.stream.read(&mut self.buf[start..]))
            .await
            .map_err(|_| Error::Timeout(self.timeout))??;
        self.buf.truncate(start + n);
        if n == 0 {
            return Err(Error::Closed);
        }
        Ok(())
    }

    fn compact(&mut self) {
        if self.pos > 0 {
            self.buf.drain(..self.pos);
            self.pos = 0;
        }
    }

    fn find_crlf(&self) -> Option<usize> {
        let hay = &self.buf[self.pos..];
        hay.windows(2).position(|w| w == b"\r\n")
    }

    /// Read one CRLF-terminated line, without the terminator.
    ///
    /// Lossy UTF-8: header lines can legitimately carry raw 8-bit bytes from a non-conforming
    /// server, and dropping the whole connection over one bad octet in a status message helps
    /// nobody. Message bodies never go through here.
    pub(crate) async fn read_line(&mut self) -> Result<String> {
        loop {
            if let Some(idx) = self.find_crlf() {
                let start = self.pos;
                let end = start + idx;
                self.pos = end + 2;
                let line = String::from_utf8_lossy(&self.buf[start..end]).into_owned();
                if self.pos >= self.buf.len() {
                    self.buf.clear();
                    self.pos = 0;
                }
                return Ok(line);
            }
            if self.pending() > MAX_LINE {
                return Err(Error::Protocol(format!(
                    "response line exceeded {MAX_LINE} bytes with no CRLF"
                )));
            }
            self.compact();
            self.fill().await?;
        }
    }

    /// Stream a dot-terminated multi-line response into `out`, undoing byte-stuffing.
    ///
    /// Writes straight through rather than buffering, so a 200 MB attachment never lands in
    /// memory — the constraint that matters on Android.
    pub(crate) async fn read_multiline_to<W>(&mut self, out: &mut W) -> Result<u64>
    where
        W: AsyncWrite + Unpin + ?Sized,
    {
        let mut written = 0u64;
        loop {
            let Some(idx) = self.find_crlf() else {
                if self.pending() > MAX_LINE {
                    return Err(Error::Protocol(format!(
                        "data line exceeded {MAX_LINE} bytes with no CRLF"
                    )));
                }
                if self.pos > COMPACT_THRESHOLD {
                    self.compact();
                }
                self.fill().await?;
                continue;
            };

            let start = self.pos;
            let end = start + idx;
            self.pos = end + 2;

            let line = &self.buf[start..end];
            if line == b"." {
                self.compact();
                return Ok(written);
            }
            // Byte-stuffing: a data line starting with '.' had one prepended by the sender.
            let payload = match line.first() {
                Some(b'.') => &line[1..],
                _ => line,
            };
            out.write_all(payload).await?;
            out.write_all(b"\r\n").await?;
            written += payload.len() as u64 + 2;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `Conn` fed by a canned server script.
    fn conn_over(server_says: &[u8]) -> Conn {
        let (client, mut server) = tokio::io::duplex(1024 * 1024);
        let script = server_says.to_vec();
        tokio::spawn(async move {
            let _ = server.write_all(&script).await;
            let _ = server.flush().await;
            // Hold the half open briefly so reads see EOF only after the script is drained.
            tokio::time::sleep(Duration::from_millis(50)).await;
        });
        Conn::new(Transport::Mock(client), Duration::from_secs(5))
    }

    #[tokio::test]
    async fn reads_crlf_delimited_status_lines() {
        let mut c = conn_over(b"+OK POP3 ready\r\n-ERR nope\r\n");
        assert_eq!(c.read_line().await.unwrap(), "+OK POP3 ready");
        assert_eq!(c.read_line().await.unwrap(), "-ERR nope");
    }

    #[tokio::test]
    async fn line_split_across_reads_is_reassembled() {
        // 40 KiB of 'x' forces the codec across several 16 KiB read chunks.
        let mut script = b"+OK ".to_vec();
        script.extend(std::iter::repeat_n(b'x', 40 * 1024));
        script.extend_from_slice(b"\r\n");
        let mut c = conn_over(&script);
        let line = c.read_line().await.unwrap();
        assert_eq!(line.len(), 4 + 40 * 1024);
    }

    #[tokio::test]
    async fn multiline_unstuffs_and_terminates_on_lone_dot() {
        let mut c = conn_over(b"line one\r\n..dotted\r\n...\r\n.\r\nAFTER\r\n");
        let mut out = Vec::new();
        let n = c.read_multiline_to(&mut out).await.unwrap();
        assert_eq!(out, b"line one\r\n.dotted\r\n..\r\n");
        assert_eq!(n, out.len() as u64);
        // The terminator must not swallow what follows it.
        assert_eq!(c.read_line().await.unwrap(), "AFTER");
    }

    #[tokio::test]
    async fn empty_body_is_just_the_terminator() {
        let mut c = conn_over(b".\r\n");
        let mut out = Vec::new();
        assert_eq!(c.read_multiline_to(&mut out).await.unwrap(), 0);
        assert!(out.is_empty());
    }

    #[tokio::test]
    async fn truncated_body_reports_closed_not_success() {
        // A server that dies mid-transfer must not look like a complete short message.
        let mut c = conn_over(b"partial line\r\n");
        let mut out = Vec::new();
        let err = c.read_multiline_to(&mut out).await.unwrap_err();
        assert!(matches!(err, Error::Closed), "got {err:?}");
    }

    #[tokio::test]
    async fn pending_reports_unconsumed_bytes() {
        let mut c = conn_over(b"+OK\r\nLEFTOVER\r\n");
        assert_eq!(c.read_line().await.unwrap(), "+OK");
        assert_eq!(c.pending(), b"LEFTOVER\r\n".len());
    }
}
