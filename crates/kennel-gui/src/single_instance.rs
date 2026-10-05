use crate::commands::{Commands, UiCommand};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;

/// Outcome of trying to become *the* kennel GUI process.
pub enum Acquired {
    /// We own the show-socket. Hand the listener to [`serve`] so later
    /// launches can be answered.
    Primary(UnixListener),
    /// A GUI is already running and has just been told to show its window.
    /// This process has nothing left to do.
    AlreadyRunning,
}

/// A GUI that lives in the tray has no window to click on, so launching
/// `Kennel.app` again is the natural way to bring it back -- but a second
/// process would mean a second tray icon and two pollers on the control
/// socket. Connecting to this socket is the whole message: the running GUI
/// takes any connection as "show yourself".
pub fn acquire(path: &Path) -> std::io::Result<Acquired> {
    if UnixStream::connect(path).is_ok() {
        return Ok(Acquired::AlreadyRunning);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Nothing answered, so either the socket never existed or it is a leftover
    // from a GUI that died without cleaning up. Both mean the path is ours.
    let _ = std::fs::remove_file(path);
    Ok(Acquired::Primary(UnixListener::bind(path)?))
}

/// Answers later `Kennel.app` launches for as long as this GUI runs.
pub fn serve(listener: UnixListener, commands: Commands) {
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            if stream.is_err() {
                continue;
            }
            commands.send(UiCommand::Open);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::commands;

    #[test]
    fn second_launch_asks_the_first_to_open_instead_of_starting_up() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gui.sock");

        let listener = match acquire(&path).unwrap() {
            Acquired::Primary(listener) => listener,
            Acquired::AlreadyRunning => panic!("nothing was listening yet"),
        };
        let (commands, rx) = commands(egui::Context::default());
        serve(listener, commands);

        assert!(matches!(acquire(&path).unwrap(), Acquired::AlreadyRunning));
        assert_eq!(rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap(), UiCommand::Open);
    }

    #[test]
    fn a_leftover_socket_file_from_a_dead_gui_is_taken_over() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gui.sock");
        drop(UnixListener::bind(&path).unwrap()); // file stays behind, nothing listening

        assert!(matches!(acquire(&path).unwrap(), Acquired::Primary(_)));
    }
}
