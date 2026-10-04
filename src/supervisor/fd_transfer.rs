//! Small bounded SCM_RIGHTS helpers for generation control streams.
#![allow(unsafe_code)]

use std::{
    io, mem,
    os::{
        fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd},
        unix::net::UnixStream,
    },
    ptr,
};

const MARKER: u8 = 0xa7;

pub fn send_marker(stream: &UnixStream, descriptor: Option<RawFd>) -> io::Result<()> {
    let byte = MARKER;
    let mut iov = libc::iovec {
        iov_base: (&byte as *const u8).cast_mut().cast(),
        iov_len: 1,
    };
    let mut message: libc::msghdr = unsafe { mem::zeroed() };
    message.msg_iov = &mut iov;
    message.msg_iovlen = 1;

    let mut control = descriptor.map(|_| vec![0_usize; cmsg_words(mem::size_of::<RawFd>())]);
    if let (Some(fd), Some(control)) = (descriptor, control.as_mut()) {
        message.msg_control = control.as_mut_ptr().cast();
        message.msg_controllen = control.len() * mem::size_of::<usize>();
        // SAFETY: the control buffer is CMSG_SPACE-aligned and large enough for one fd.
        let header = unsafe { libc::CMSG_FIRSTHDR(&message) };
        if header.is_null() {
            return Err(io::Error::other("SCM_RIGHTS control header unavailable"));
        }
        unsafe {
            (*header).cmsg_level = libc::SOL_SOCKET;
            (*header).cmsg_type = libc::SCM_RIGHTS;
            (*header).cmsg_len = libc::CMSG_LEN(mem::size_of::<RawFd>() as _) as _;
            ptr::copy_nonoverlapping(
                (&fd as *const RawFd).cast::<u8>(),
                libc::CMSG_DATA(header),
                mem::size_of::<RawFd>(),
            );
        }
    }

    loop {
        // SAFETY: message pointers refer to live stack/buffer storage for this call.
        let sent = unsafe { libc::sendmsg(stream.as_raw_fd(), &message, libc::MSG_NOSIGNAL) };
        if sent == 1 {
            return Ok(());
        }
        if sent < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
            continue;
        }
        return Err(io::Error::last_os_error());
    }
}

pub fn receive_marker(stream: &UnixStream) -> io::Result<Option<OwnedFd>> {
    let mut byte = 0_u8;
    let mut iov = libc::iovec {
        iov_base: (&mut byte as *mut u8).cast(),
        iov_len: 1,
    };
    let mut control = vec![0_usize; cmsg_words(mem::size_of::<RawFd>() * 8)];
    let mut message: libc::msghdr = unsafe { mem::zeroed() };
    message.msg_iov = &mut iov;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast();
    message.msg_controllen = control.len() * mem::size_of::<usize>();
    let flags = cmsg_cloexec_flag();
    let received = loop {
        // SAFETY: message pointers refer to writable stack/buffer storage for this call.
        let count = unsafe { libc::recvmsg(stream.as_raw_fd(), &mut message, flags) };
        if count >= 0 {
            break count;
        }
        if io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            return Err(io::Error::last_os_error());
        }
    };
    if received == 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "control stream closed before its request marker",
        ));
    }
    if byte != MARKER || message.msg_flags & libc::MSG_CTRUNC != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid or truncated control marker",
        ));
    }

    let mut descriptors = Vec::new();
    // SAFETY: kernel populated the aligned control buffer and reports its bounds.
    let mut header = unsafe { libc::CMSG_FIRSTHDR(&message) };
    while !header.is_null() {
        let current = unsafe { &*header };
        let base_len = unsafe { libc::CMSG_LEN(0) as usize };
        let header_len = current.cmsg_len as usize;
        if current.cmsg_level != libc::SOL_SOCKET
            || current.cmsg_type != libc::SCM_RIGHTS
            || header_len < base_len
            || !(header_len - base_len).is_multiple_of(mem::size_of::<RawFd>())
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unexpected control message",
            ));
        }
        let count = (header_len - base_len) / mem::size_of::<RawFd>();
        for index in 0..count {
            let raw =
                unsafe { ptr::read_unaligned(libc::CMSG_DATA(header).cast::<RawFd>().add(index)) };
            if raw < 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid received descriptor",
                ));
            }
            let owned = unsafe { OwnedFd::from_raw_fd(raw) };
            set_close_on_exec(owned.as_raw_fd())?;
            descriptors.push(owned);
        }
        // SAFETY: the current header came from this same kernel-bounded message.
        header = unsafe { libc::CMSG_NXTHDR(&message, header) };
    }
    if descriptors.len() > 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "control request carried more than one descriptor",
        ));
    }
    Ok(descriptors.pop())
}

pub fn is_listening_stream(fd: BorrowedFd<'_>) -> io::Result<bool> {
    let mut accepting = 0_i32;
    let mut accepting_len = mem::size_of::<i32>() as libc::socklen_t;
    let mut socket_type = 0_i32;
    let mut socket_type_len = mem::size_of::<i32>() as libc::socklen_t;
    // SAFETY: getsockopt writes fixed-size integer options for a live socket fd.
    let accepting_result = unsafe {
        libc::getsockopt(
            fd.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_ACCEPTCONN,
            (&mut accepting as *mut i32).cast(),
            &mut accepting_len,
        )
    };
    if accepting_result < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: getsockopt writes fixed-size integer options for a live socket fd.
    let type_result = unsafe {
        libc::getsockopt(
            fd.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_TYPE,
            (&mut socket_type as *mut i32).cast(),
            &mut socket_type_len,
        )
    };
    if type_result < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(accepting != 0 && socket_type == libc::SOCK_STREAM)
}

/// Take ownership of a systemd socket-activation descriptor.
///
/// The caller must only pass a descriptor in the activation range confirmed by
/// LISTEN_FDS and LISTEN_PID.
pub(crate) fn take_inherited_fd(fd: RawFd) -> io::Result<OwnedFd> {
    if fd < 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "negative inherited descriptor",
        ));
    }
    // SAFETY: the socket-activation contract transfers ownership of this live fd.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn cmsg_words(data_len: usize) -> usize {
    let bytes = unsafe { libc::CMSG_SPACE(data_len as _) as usize };
    bytes.div_ceil(mem::size_of::<usize>())
}

fn set_close_on_exec(fd: RawFd) -> io::Result<()> {
    // SAFETY: fcntl operates on the newly received descriptor.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fcntl updates descriptor flags on the newly received descriptor.
    if unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn cmsg_cloexec_flag() -> libc::c_int {
    libc::MSG_CMSG_CLOEXEC
}

#[cfg(not(target_os = "linux"))]
fn cmsg_cloexec_flag() -> libc::c_int {
    0
}

#[cfg(test)]
mod tests {
    use super::{is_listening_stream, receive_marker, send_marker};
    use std::{
        net::TcpListener,
        os::fd::{AsFd, AsRawFd},
        os::unix::net::UnixStream,
    };

    #[test]
    fn scm_rights_transfers_exactly_one_listening_socket() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let (sender, receiver) = UnixStream::pair().unwrap();
        send_marker(&sender, Some(listener.as_raw_fd())).unwrap();
        let received = receive_marker(&receiver).unwrap().unwrap();
        assert!(is_listening_stream(received.as_fd()).unwrap());
        send_marker(&sender, None).unwrap();
        assert!(receive_marker(&receiver).unwrap().is_none());
    }
}
