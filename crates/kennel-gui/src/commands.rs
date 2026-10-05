use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};

/// A request aimed at the GUI window, raised from outside egui's event loop --
/// the tray menu (which runs on the OS event loop) and the show-socket thread
/// (which answers a second `Kennel.app` launch).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiCommand {
    /// Bring the main window back from the tray.
    Open,
    /// Actually exit the process, tray icon and all.
    Quit,
}

/// The send half. Deliberately `Send + Sync + Clone`, because `muda`'s menu
/// event handler demands `Fn + Send + Sync + 'static` -- a bare
/// `mpsc::Sender` is `Send` but not `Sync`, hence the mutex.
///
/// Every send also pokes egui: the window may be hidden in the tray, and a
/// hidden window has no reason of its own to repaint, so without this a click
/// on "Open Kennel" would sit unread in the channel.
#[derive(Clone)]
pub struct Commands {
    tx: Arc<Mutex<Sender<UiCommand>>>,
    ctx: egui::Context,
}

impl Commands {
    pub fn send(&self, command: UiCommand) {
        if let Ok(tx) = self.tx.lock() {
            let _ = tx.send(command);
        }
        self.ctx.request_repaint();
    }
}

pub fn commands(ctx: egui::Context) -> (Commands, Receiver<UiCommand>) {
    let (tx, rx) = channel();
    (Commands { tx: Arc::new(Mutex::new(tx)), ctx }, rx)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_sent_from_another_thread_arrive_in_order() {
        let (commands, rx) = commands(egui::Context::default());
        std::thread::spawn(move || {
            commands.send(UiCommand::Open);
            commands.send(UiCommand::Quit);
        })
        .join()
        .unwrap();

        assert_eq!(rx.recv().unwrap(), UiCommand::Open);
        assert_eq!(rx.recv().unwrap(), UiCommand::Quit);
    }
}
