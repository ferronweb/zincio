mod listener;
mod stream;

pub use listener::*;
pub use stream::*;

#[cfg(test)]
mod tests {
    use std::io::{self as std_io};
    use std::net::{Shutdown, SocketAddr};

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use crate::io::{AsyncRead, AsyncWrite};
    use crate::{driver::AnyDriver, executor::spawn};

    use super::{PollTcpStream, TcpListener, TcpStream};

    #[inline]
    fn try_bind_listener(address: SocketAddr) -> Option<TcpListener> {
        match TcpListener::bind(address) {
            Ok(listener) => Some(listener),
            Err(err) if err.kind() == std_io::ErrorKind::PermissionDenied => None,
            Err(err) => panic!("listener should bind: {err}"),
        }
    }

    #[test]
    fn tcp_listener_and_stream_exchange_data() {
        let runtime = crate::executor::Runtime::new(
            #[cfg(unix)]
            AnyDriver::new_mio().expect("mio driver should initialize"),
            #[cfg(windows)]
            AnyDriver::new_iocp().expect("iocp driver should initialize"),
        );
        runtime.block_on(async {
            let address = "127.0.0.1:0"
                .parse::<SocketAddr>()
                .expect("address should parse");
            let Some(listener) = try_bind_listener(address) else {
                return;
            };
            let server_address = listener
                .local_addr()
                .expect("listener should expose address");

            let server = spawn(async move {
                let (mut stream, _) = listener.accept().await?;
                let buffer = [0u8; 4];
                let (read, buffer) = stream.read(buffer).await;
                let read = read?;
                assert_eq!(&buffer[..read], b"ping");
                stream.write(b"pong".to_vec()).await.0?;
                stream.shutdown(Shutdown::Both)?;
                Ok::<(), std_io::Error>(())
            });

            let mut client = TcpStream::connect(server_address)
                .await
                .expect("client should connect");
            client
                .write(b"ping".to_vec())
                .await
                .0
                .expect("client should write");
            let response = [0u8; 4];
            let (read, response) = client.read(response).await;
            let read = read.expect("client should read");
            assert_eq!(&response[..read], b"pong");
            assert_eq!(
                client
                    .peer_addr()
                    .expect("peer address should be available"),
                server_address
            );
            client
                .shutdown(Shutdown::Both)
                .expect("shutdown should succeed");

            server.await.expect("server task should complete");
        });
    }

    #[test]
    fn poll_tcp_stream_uses_readiness_path() {
        let runtime = crate::executor::Runtime::new(
            #[cfg(unix)]
            AnyDriver::new_mio().expect("mio driver should initialize"),
            #[cfg(windows)]
            AnyDriver::new_iocp().expect("iocp driver should initialize"),
        );
        runtime.block_on(async {
            let address = "127.0.0.1:0"
                .parse::<SocketAddr>()
                .expect("address should parse");
            let Some(listener) = try_bind_listener(address) else {
                return;
            };
            let server_address = listener
                .local_addr()
                .expect("listener should expose address");

            let server = spawn(async move {
                let (mut stream, _) = listener.accept().await?;
                let received = [0u8; 4];
                let (read, received) = stream.read(received).await;
                let read = read?;
                assert_eq!(&received[..read], b"mio!");
                stream.write(b"ok".to_vec()).await.0?;
                Ok::<(), std_io::Error>(())
            });

            let mut client = PollTcpStream::connect(server_address)
                .await
                .expect("client should connect");
            AsyncWriteExt::write_all(&mut client, b"mio!")
                .await
                .expect("tokio write_all should succeed");
            let mut response = [0u8; 2];
            AsyncReadExt::read_exact(&mut client, &mut response)
                .await
                .expect("tokio read_exact should succeed");
            assert_eq!(&response, b"ok");

            server.await.expect("server task should complete");
        });
    }

    /// `io_uring` with a read poll and a write poll in flight on one socket: a
    /// writer blocked on a full socket, then a reader waiting on the same one.
    /// When the peer drains the socket, the writer must be woken, not the
    /// reader that registered after it.
    #[cfg(all(target_os = "linux", feature = "time"))]
    #[cfg_attr(miri, ignore)]
    #[test]
    fn io_uring_wakes_a_blocked_writer_while_a_read_waits() {
        let Ok(driver) = AnyDriver::new_uring() else {
            return; // no io_uring here
        };
        a_blocked_writer_is_woken_while_a_read_waits(driver);
    }

    /// The same on the readiness driver (epoll).
    #[cfg(all(unix, feature = "time"))]
    #[test]
    fn mio_wakes_a_blocked_writer_while_a_read_waits() {
        a_blocked_writer_is_woken_while_a_read_waits(
            AnyDriver::new_mio().expect("mio driver should initialize"),
        );
    }

    #[cfg(all(unix, feature = "time"))]
    fn a_blocked_writer_is_woken_while_a_read_waits(driver: AnyDriver) {
        const SIZE: usize = 16 << 20;
        let runtime = crate::executor::Runtime::new(driver);
        runtime.block_on(async {
            let std_listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
            let address = std_listener.local_addr().expect("address");
            let (go, wait) = std::sync::mpsc::channel::<()>();
            let peer = std::thread::spawn(move || {
                let (mut socket, _) = std_listener.accept().expect("accept");
                wait.recv().expect("go");
                let mut buf = vec![0u8; 1 << 20];
                let mut total = 0;
                while let Ok(n) = std::io::Read::read(&mut socket, &mut buf) {
                    if n == 0 {
                        break;
                    }
                    total += n;
                }
                total
            });
            let stream = PollTcpStream::connect(address)
                .await
                .expect("client should connect");
            let (mut reader, mut writer) = tokio::io::split(stream);
            let write = spawn(async move {
                writer.write_all(&vec![7u8; SIZE]).await?;
                writer.shutdown().await
            });
            // The write fills the socket and waits, then a read waits too.
            crate::time::sleep(std::time::Duration::from_millis(200)).await;
            let read = spawn(async move {
                let mut byte = [0u8; 1];
                reader.read(&mut byte).await
            });
            crate::time::sleep(std::time::Duration::from_millis(200)).await;
            go.send(()).expect("peer waits");
            let written = crate::time::timeout(std::time::Duration::from_secs(10), write)
                .await
                .expect("the blocked writer was never woken");
            written.expect("write_all");
            assert_eq!(peer.join().expect("peer"), SIZE);
            let read = crate::time::timeout(std::time::Duration::from_secs(10), read)
                .await
                .expect("the reader was never woken");
            assert_eq!(read.expect("read"), 0, "end of stream");
        });
    }

    /// A tokio read into spare capacity the reader has not initialized
    /// (`read_buf` on an empty `Vec`): the bytes the kernel wrote, and no more.
    #[test]
    fn poll_tcp_stream_reads_into_uninitialized_capacity() {
        let runtime = crate::executor::Runtime::new(
            #[cfg(unix)]
            AnyDriver::new_mio().expect("mio driver should initialize"),
            #[cfg(windows)]
            AnyDriver::new_iocp().expect("iocp driver should initialize"),
        );
        runtime.block_on(async {
            let address = "127.0.0.1:0"
                .parse::<SocketAddr>()
                .expect("address should parse");
            let Some(listener) = try_bind_listener(address) else {
                return;
            };
            let server_address = listener
                .local_addr()
                .expect("listener should expose address");
            let server = spawn(async move {
                let (mut stream, _) = listener.accept().await?;
                stream.write(b"uninit".to_vec()).await.0?;
                stream.shutdown(Shutdown::Both)?;
                Ok::<(), std_io::Error>(())
            });
            let mut client = PollTcpStream::connect(server_address)
                .await
                .expect("client should connect");
            let mut received = Vec::with_capacity(64);
            while AsyncReadExt::read_buf(&mut client, &mut received)
                .await
                .expect("tokio read_buf should succeed")
                > 0
            {}
            assert_eq!(received, b"uninit");
            server.await.expect("server task should complete");
        });
    }
    /// A runtime dropped with a task still waiting on a stream drops the task, and with
    /// it the stream: the peer sees its end.
    #[cfg_attr(miri, ignore)]
    #[test]
    fn a_dropped_runtime_closes_the_streams_its_tasks_held() {
        for driver in [
            AnyDriver::new_mio().expect("mio driver should initialize"),
            #[cfg(target_os = "linux")]
            AnyDriver::new_uring().expect("io_uring driver should initialize"),
        ] {
            let std_listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
            let address = std_listener.local_addr().expect("address");
            let peer = std::thread::spawn(move || {
                let (mut socket, _) = std_listener.accept().expect("accept");
                socket
                    .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                    .expect("timeout");
                let mut byte = [0u8; 1];
                std::io::Read::read(&mut socket, &mut byte)
            });
            let runtime = crate::executor::Runtime::new(driver);
            runtime.block_on(async move {
                let mut stream = PollTcpStream::connect(address)
                    .await
                    .expect("client should connect");
                let _task = spawn(async move {
                    let mut byte = [0u8; 1];
                    let _ = AsyncReadExt::read(&mut stream, &mut byte).await;
                });
                // Let the task start and wait on the socket.
                crate::time::sleep(std::time::Duration::from_millis(50)).await;
            });
            drop(runtime);
            let read = peer.join().expect("peer");
            assert_eq!(read.expect("the peer should see the end, not time out"), 0);
        }
    }
}
