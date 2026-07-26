use std::{
    io::{Error, ErrorKind, Read, Write},
    os::fd::{AsRawFd, RawFd},
    os::unix::net::UnixStream,
    sync::Mutex,
    thread::{self, JoinHandle},
};

use xcb::{
    Connection, Error as XcbError, Event, Extension,
    randr::{self, NotifyMask},
    x::Window,
};

use crate::{XCapError, XCapResult, video_recorder::set_current_thread_utility_priority};

/// RAII listener for X11 RandR topology notifications.
///
/// The worker blocks in `poll(2)` on both the XCB connection and a private
/// wake socket. Dropping the watcher wakes and joins the worker immediately;
/// no detached polling thread survives a recorder/service rebuild.
pub struct MonitorTopologyWatcher {
    wake_writer: Mutex<Option<UnixStream>>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl MonitorTopologyWatcher {
    pub fn new<F>(on_topology_changed: F) -> XCapResult<Self>
    where
        F: Fn() + Send + 'static,
    {
        let (connection, screen_index) =
            Connection::connect_with_extensions(None, &[], &[Extension::RandR])?;
        if !connection
            .active_extensions()
            .any(|extension| extension == Extension::RandR)
        {
            return Err(XCapError::new("X11 RandR extension is unavailable"));
        }
        let root = connection
            .get_setup()
            .roots()
            .nth(screen_index as usize)
            .ok_or_else(|| XCapError::new("X11 screen not found"))?
            .root();
        let select_cookie = connection.send_request_checked(&randr::SelectInput {
            window: root,
            enable: NotifyMask::SCREEN_CHANGE
                | NotifyMask::CRTC_CHANGE
                | NotifyMask::OUTPUT_CHANGE
                | NotifyMask::PROVIDER_CHANGE
                | NotifyMask::RESOURCE_CHANGE,
        });
        connection
            .check_request(select_cookie)
            .map_err(|error| XCapError::from(XcbError::from(error)))?;
        connection.flush()?;

        let (wake_reader, wake_writer) = UnixStream::pair()?;
        let worker = thread::Builder::new()
            .name("xcap-xrandr-topology".to_string())
            .spawn(move || {
                set_current_thread_utility_priority();
                if let Err(error) =
                    run_xrandr_watcher(connection, root, wake_reader, on_topology_changed)
                {
                    log::warn!("X11 RandR topology watcher stopped: {error}");
                }
            })
            .map_err(XCapError::from)?;

        Ok(Self {
            wake_writer: Mutex::new(Some(wake_writer)),
            worker: Mutex::new(Some(worker)),
        })
    }
}

impl Drop for MonitorTopologyWatcher {
    fn drop(&mut self) {
        if let Some(mut wake_writer) = self
            .wake_writer
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
        {
            let _ = wake_writer.write_all(&[1]);
        }
        if let Some(worker) = self
            .worker
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
        {
            let _ = worker.join();
        }
    }
}

fn run_xrandr_watcher<F>(
    connection: Connection,
    root: Window,
    mut wake_reader: UnixStream,
    on_topology_changed: F,
) -> XCapResult<()>
where
    F: Fn(),
{
    let xcb_fd = connection.as_raw_fd();
    let wake_fd = wake_reader.as_raw_fd();
    loop {
        let mut descriptors = [poll_descriptor(xcb_fd), poll_descriptor(wake_fd)];
        let poll_result = unsafe {
            libc::poll(
                descriptors.as_mut_ptr(),
                descriptors.len() as libc::nfds_t,
                -1,
            )
        };
        if poll_result < 0 {
            let error = Error::last_os_error();
            if error.kind() == ErrorKind::Interrupted {
                continue;
            }
            return Err(error.into());
        }
        if descriptors[1].revents != 0 {
            let mut wake_byte = [0_u8; 1];
            let _ = wake_reader.read(&mut wake_byte);
            break;
        }
        if descriptors[0].revents == 0 {
            continue;
        }

        while let Some(event) = connection.poll_for_event()? {
            if matches!(event, Event::RandR(_)) {
                on_topology_changed();
            }
        }
    }

    let unsubscribe = connection.send_request_checked(&randr::SelectInput {
        window: root,
        enable: NotifyMask::empty(),
    });
    let _ = connection.check_request(unsubscribe);
    let _ = connection.flush();
    Ok(())
}

fn poll_descriptor(fd: RawFd) -> libc::pollfd {
    libc::pollfd {
        fd,
        events: libc::POLLIN | libc::POLLERR | libc::POLLHUP,
        revents: 0,
    }
}
