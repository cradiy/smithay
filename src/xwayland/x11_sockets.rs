use rustix::net::SocketAddrUnix;
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    os::unix::{
        fs::{MetadataExt, OpenOptionsExt},
        net::UnixStream,
    },
    path::PathBuf,
};
use tracing::warn;

pub(crate) fn prepare_x11_sockets(
    display: Option<u32>,
    open_abstract_socket: bool,
) -> io::Result<(X11Lock, Vec<UnixStream>)> {
    let displays = display.map_or(0..=32, |number| number..=number);
    for number in displays {
        let mut lock = match X11Lock::grab(number) {
            Ok(lock) => lock,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists && display.is_none() => continue,
            Err(error) => return Err(error),
        };
        match lock.open_sockets(open_abstract_socket) {
            Ok(sockets) => return Ok((lock, sockets)),
            Err(error) if error.kind() == io::ErrorKind::AddrInUse && display.is_none() => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AddrInUse,
        "No unused X11 display socket is available",
    ))
}

#[derive(Debug)]
struct OwnedPath {
    path: PathBuf,
    device: u64,
    inode: u64,
}
impl OwnedPath {
    fn new(path: PathBuf, metadata: fs::Metadata) -> Self {
        Self {
            path,
            device: metadata.dev(),
            inode: metadata.ino(),
        }
    }
}
impl Drop for OwnedPath {
    fn drop(&mut self) {
        match fs::symlink_metadata(&self.path) {
            Ok(metadata) if metadata.dev() == self.device && metadata.ino() == self.inode => {
                if let Err(error) = fs::remove_file(&self.path) {
                    warn!(%error, path=?self.path, "Failed to remove owned X11 endpoint");
                }
            }
            Ok(_) => (), // Another owner replaced the path.
            Err(error) if error.kind() == io::ErrorKind::NotFound => (),
            Err(error) => warn!(%error, path=?self.path, "Failed to inspect owned X11 endpoint"),
        }
    }
}

#[derive(Debug)]
pub(crate) struct X11Lock {
    // Release the socket before publishing the display number as available.
    socket: Option<OwnedPath>,
    _lock: OwnedPath,
    _file: File,
    display: u32,
}
impl X11Lock {
    fn grab(number: u32) -> io::Result<Self> {
        let path = PathBuf::from(format!("/tmp/.X{number}-lock"));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)?;
        let guard = OwnedPath::new(path, file.metadata()?);
        writeln!(
            file,
            "{:>10}",
            rustix::process::Pid::as_raw(Some(rustix::process::getpid()))
        )?;
        Ok(Self {
            socket: None,
            _lock: guard,
            _file: file,
            display: number,
        })
    }
    pub(crate) fn display_number(&self) -> u32 {
        self.display
    }
    fn open_sockets(&mut self, open_abstract: bool) -> io::Result<Vec<UnixStream>> {
        let path = PathBuf::from(format!("/tmp/.X11-unix/X{}", self.display));
        let address = SocketAddrUnix::new(&path)?;
        let socket = open_socket(address)?;
        self.socket = Some(OwnedPath::new(path.clone(), fs::symlink_metadata(&path)?));
        rustix::net::listen(&socket, 1)?;
        let mut sockets = vec![socket];
        #[cfg(target_os = "linux")]
        if open_abstract {
            let socket = open_socket(SocketAddrUnix::new_abstract_name(
                path.as_os_str().as_encoded_bytes(),
            )?)?;
            rustix::net::listen(&socket, 1)?;
            sockets.push(socket);
        }
        #[cfg(not(target_os = "linux"))]
        let _ = open_abstract;
        Ok(sockets)
    }
}
fn open_socket(address: SocketAddrUnix) -> io::Result<UnixStream> {
    let fd = rustix::net::socket_with(
        rustix::net::AddressFamily::UNIX,
        rustix::net::SocketType::STREAM,
        rustix::net::SocketFlags::CLOEXEC,
        None,
    )?;
    rustix::net::bind(&fd, &address)?;
    Ok(UnixStream::from(fd))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs::File, os::unix::net::UnixListener};

    #[test]
    fn occupied_endpoints_and_replaced_paths_are_not_removed() {
        let number = 100_000 + std::process::id();
        let socket_path = PathBuf::from(format!("/tmp/.X11-unix/X{number}"));
        let lock_path = PathBuf::from(format!("/tmp/.X{number}-lock"));
        let listener = UnixListener::bind(&socket_path).unwrap();
        let socket = OwnedPath::new(socket_path.clone(), fs::symlink_metadata(&socket_path).unwrap());
        assert_eq!(
            prepare_x11_sockets(Some(number), false).unwrap_err().kind(),
            io::ErrorKind::AddrInUse
        );
        UnixStream::connect(&socket_path).unwrap();
        assert!(!lock_path.exists());
        drop(listener);
        drop(socket);
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock_path)
            .unwrap();
        file.write_all(b"999999999\n").unwrap();
        let existing = OwnedPath::new(lock_path.clone(), file.metadata().unwrap());
        assert_eq!(
            prepare_x11_sockets(Some(number), false).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(fs::read(&lock_path).unwrap(), b"999999999\n");
        drop(existing);
        let (lock, sockets) = prepare_x11_sockets(Some(number), false).unwrap();
        fs::remove_file(&socket_path).unwrap();
        let replacement = File::create(&socket_path).unwrap();
        let replacement_guard = OwnedPath::new(socket_path.clone(), replacement.metadata().unwrap());
        drop(sockets);
        drop(lock);
        assert!(socket_path.is_file());
        assert!(!lock_path.exists());
        drop(replacement_guard);
    }
}
