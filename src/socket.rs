use core::time;
use nix::sys::socket::{self, AddressFamily, RecvMsg, SockFlag, SockType, UnixAddr};
use nix::sys::stat;
use std::collections::HashMap;
use std::io::{IoSlice, IoSliceMut};
use std::os::fd::RawFd;
use std::sync::Arc;
use tokio::sync::Mutex;

const MAX_RETRY: usize = 5;
const RETRY_INTERVAL: time::Duration = time::Duration::from_secs(1);

pub struct FileDescriptorsMap {
    pub map: HashMap<String, RawFd>,
}

impl FileDescriptorsMap {
    pub fn new() -> Self {
        Self {
            map: HashMap::new(),
        }
    }

    pub fn add(&mut self, bind: String, fd: RawFd) {
        self.map.insert(bind, fd);
    }

    pub fn get(&self, bind: &str) -> Option<&RawFd> {
        self.map.get(bind)
    }

    pub fn serialize(&self) -> (Vec<String>, Vec<RawFd>) {
        let serialized: Vec<(String, RawFd)> = self
            .map
            .iter()
            .map(|(key, value)| (key.clone(), *value))
            .collect();

        (
            serialized.iter().map(|v| v.0.clone()).collect(),
            serialized.iter().map(|v| v.1).collect(),
        )
        // Surely there is a better way of doing this
    }

    pub fn deserialize(&mut self, binds: Vec<String>, fds: Vec<RawFd>) {
        assert!(binds.len() == fds.len());
        // TODO: use zip()
        for i in 0..binds.len() {
            self.map.insert(binds[i].clone(), fds[i]);
        }
    }

    pub fn block_socket_and_send_to_new_server<P>(
        &self,
        upgrade_path: &P,
    ) -> Result<usize, nix::Error>
    where
        P: ?Sized + nix::NixPath + std::fmt::Display,
    {
        let (vec_key, vec_fds) = self.serialize();
        let mut ser_buf: [u8; 2048] = [0; 2048];
        let ser_key_size = serialize_vec_string(&vec_key, &mut ser_buf);
        send_fds_to(vec_fds, &ser_buf[..ser_key_size], upgrade_path)
    }

    pub fn get_from_sock<P>(&mut self, path: &P) -> Result<(), nix::Error>
    where
        P: ?Sized + nix::NixPath + std::fmt::Display,
    {
        let mut de_buf: [u8; 2048] = [0; 2048];
        let (fds, bytes) = get_fds_from(path, &mut de_buf)?;
        let keys = deserialize_vec_string(&de_buf[..bytes]);
        self.deserialize(keys, fds);
        Ok(())
    }
}

pub type FileDescriptors = Arc<Mutex<FileDescriptorsMap>>;

pub fn send_fds_to<P>(fds: Vec<RawFd>, payload: &[u8], path: &P) -> Result<usize, nix::Error>
where
    P: ?Sized + nix::NixPath + std::fmt::Display,
{
    use std::thread;

    use nix::errno::Errno;

    const MAX_NONBLOCKING_POLLS: usize = 20;
    const NONBLOCKING_POLL_INTERVAL: time::Duration = time::Duration::from_millis(500);

    let send_fd = socket::socket(
        AddressFamily::Unix,
        SockType::Stream,
        SockFlag::SOCK_NONBLOCK,
        None,
    )?;
    let unix_addr = UnixAddr::new(path)?;
    let mut retried = 0;
    let mut nonblocking_polls = 0;

    let conn_result: Result<usize, nix::Error> = loop {
        match socket::connect(send_fd, &unix_addr) {
            Ok(_) => break Ok(0),
            Err(e) => match e {
                /* If the new process hasn't created the upgrade sock we'll get an ENOENT.
                ECONNREFUSED may happen if the sock wasn't cleaned up
                and the old process tries sending before the new one is listening.
                EACCES may happen if connect() happen before the correct permission is set */
                Errno::ENOENT | Errno::ECONNREFUSED | Errno::EACCES => {
                    /*the server is not ready yet*/
                    retried += 1;
                    if retried > MAX_RETRY {
                        log::error!(
                            "Max retry: {} reached. Giving up sending socket to: {}, error: {:?}",
                            MAX_RETRY,
                            path,
                            e
                        );
                        break Err(e);
                    }
                    log::warn!("server not ready, will try again in {RETRY_INTERVAL:?}");
                    thread::sleep(RETRY_INTERVAL);
                }
                /* handle nonblocking IO */
                Errno::EINPROGRESS => {
                    nonblocking_polls += 1;
                    if nonblocking_polls >= MAX_NONBLOCKING_POLLS {
                        log::error!(
                            "Connect() not ready after retries when sending socket to: {path}",
                        );
                        break Err(e);
                    }
                    log::warn!(
                        "Connect() not ready, will try again in {NONBLOCKING_POLL_INTERVAL:?}",
                    );
                    thread::sleep(NONBLOCKING_POLL_INTERVAL);
                }
                _ => {
                    log::error!("Error sending socket to: {path}, error: {e:?}");
                    break Err(e);
                }
            },
        }
    };

    let result = match conn_result {
        Ok(_) => {
            let io_vec = [IoSlice::new(payload); 1];
            let scm = socket::ControlMessage::ScmRights(fds.as_slice());
            let cmsg = [scm; 1];
            loop {
                match socket::sendmsg(
                    send_fd,
                    &io_vec,
                    &cmsg,
                    socket::MsgFlags::empty(),
                    None::<&UnixAddr>,
                ) {
                    Ok(result) => break Ok(result),
                    Err(e) => match e {
                        /* handle nonblocking IO */
                        Errno::EAGAIN => {
                            nonblocking_polls += 1;
                            if nonblocking_polls >= MAX_NONBLOCKING_POLLS {
                                log::error!(
                                    "Sendmsg() not ready after retries when sending socket to: {}",
                                    path
                                );
                                break Err(e);
                            }
                            log::warn!(
                                "Sendmsg() not ready, will try again in {:?}",
                                NONBLOCKING_POLL_INTERVAL
                            );
                            thread::sleep(NONBLOCKING_POLL_INTERVAL);
                        }
                        _ => break Err(e),
                    },
                }
            }
        }
        Err(_) => conn_result,
    };

    nix::unistd::close(send_fd).unwrap();
    result
}

fn serialize_vec_string(vec_string: &[String], mut buffer: &mut [u8]) -> usize {
    use std::io::Write;

    // There are many way to do this. serde is probably the way to go
    // But let's start with something simple: space separated strings
    let joined = vec_string.join(" ");
    // TODO: check the buf is large enough
    buffer.write(joined.as_bytes()).unwrap()
}

fn deserialize_vec_string(buffer: &[u8]) -> Vec<String> {
    let joined = std::str::from_utf8(buffer).unwrap(); // TODO: handle error
    let mut results: Vec<String> = Vec::new();
    for iter in joined.split_ascii_whitespace() {
        results.push(String::from(iter));
    }
    results
}

pub fn get_fds_from<P>(path: &P, payload: &mut [u8]) -> Result<(Vec<RawFd>, usize), nix::Error>
where
    P: ?Sized + nix::NixPath + std::fmt::Display,
{
    const MAX_FDS: usize = 32;

    let listen_fd = socket::socket(
        AddressFamily::Unix,
        SockType::Stream,
        SockFlag::SOCK_NONBLOCK,
        None,
    )
    .unwrap();
    let unix_addr = UnixAddr::new(path).unwrap();
    // clean up old sock
    match nix::unistd::unlink(path) {
        Ok(()) => {
            log::debug!("unlink {} done", path);
        }
        Err(e) => {
            // Normal if file does not exist
            log::debug!("unlink {} failed: {}", path, e);
            // TODO: warn if exist but not able to unlink
        }
    };
    socket::bind(listen_fd, &unix_addr).unwrap();

    /* sock is created before we change user, need to give permission to all */
    stat::fchmodat(
        None,
        path,
        stat::Mode::all(),
        stat::FchmodatFlags::FollowSymlink,
    )
    .unwrap();

    socket::listen(listen_fd, 8).unwrap();

    let fd = match accept_with_retry(listen_fd) {
        Ok(fd) => fd,
        Err(e) => {
            log::error!("Giving up reading socket from: {path}, error: {e:?}");
            //cleanup
            if nix::unistd::close(listen_fd).is_ok() {
                nix::unistd::unlink(path).unwrap();
            }
            return Err(e);
        }
    };

    let mut io_vec = [IoSliceMut::new(payload); 1];
    let mut cmsg_buf = nix::cmsg_space!([RawFd; MAX_FDS]);
    let msg: RecvMsg<UnixAddr> = socket::recvmsg(
        fd,
        &mut io_vec,
        Some(&mut cmsg_buf),
        socket::MsgFlags::empty(),
    )
    .unwrap();

    let mut fds: Vec<RawFd> = Vec::new();
    for cmsg in msg.cmsgs() {
        if let socket::ControlMessageOwned::ScmRights(mut vec_fds) = cmsg {
            fds.append(&mut vec_fds)
        } else {
            log::warn!("Unexpected control messages: {cmsg:?}")
        }
    }

    //cleanup
    if nix::unistd::close(listen_fd).is_ok() {
        nix::unistd::unlink(path).unwrap();
    }

    Ok((fds, msg.bytes))
}

fn accept_with_retry(listen_fd: i32) -> Result<i32, nix::Error> {
    use std::thread;

    use nix::errno::Errno;

    let mut retried = 0;
    loop {
        match socket::accept(listen_fd) {
            Ok(fd) => return Ok(fd),
            Err(e) => {
                if retried > MAX_RETRY {
                    return Err(e);
                }
                match e {
                    Errno::EAGAIN => {
                        log::error!(
                            "No incoming socket transfer, sleep {RETRY_INTERVAL:?} and try again (FD: {listen_fd})"
                        );
                        retried += 1;
                        thread::sleep(RETRY_INTERVAL);
                    }
                    _ => {
                        log::error!("Error accepting socket transfer: {e}");
                        return Err(e);
                    }
                }
            }
        }
    }
}
