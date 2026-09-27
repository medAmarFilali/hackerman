//! Tells the user when files from another PC have arrived. That can happen with the
//! window closed, and nothing else would show it.

use std::path::Path;

use hk_ipc::{TransferStage, TransferView, UiState};
use tauri::AppHandle;

/// Other devices choose these names, and a notification only has room for so much.
const MAX_TEXT: usize = 200;

/// What to say about files that have just stopped arriving.
#[derive(Debug, PartialEq, Eq)]
pub struct Arrival {
    pub title: String,
    pub text: String,
    /// Where the files are, to go and look. Without one, there's nothing to see there.
    pub folder: Option<String>,
}

/// Transfers to this PC that have ended since the state before. One the user stopped
/// isn't news to them, and they know about anything they sent themselves.
pub fn arrivals(previous: &UiState, state: &UiState) -> Vec<Arrival> {
    let was_over = |transfer: &TransferView| {
        previous
            .transfers
            .iter()
            .find(|earlier| earlier.id == transfer.id)
            .is_some_and(|earlier| earlier.stage.is_over())
    };
    state
        .transfers
        .iter()
        .filter(|transfer| !transfer.sending && !was_over(transfer))
        .filter_map(|transfer| {
            let from = state
                .devices
                .iter()
                .find(|device| device.fingerprint == transfer.peer)
                .map_or("another PC", |device| device.name.as_str());
            match &transfer.stage {
                TransferStage::Done => Some(Arrival {
                    title: shortened(&format!("Files from {from}")),
                    text: shortened(&saved_text(transfer)),
                    folder: transfer.folder.clone(),
                }),
                TransferStage::Failed { reason } => Some(Arrival {
                    title: shortened(&format!("Files from {from} weren't saved")),
                    text: shortened(&format!("{}: {reason}", transfer.label)),
                    folder: None,
                }),
                _ => None,
            }
        })
        .collect()
}

fn saved_text(transfer: &TransferView) -> String {
    let folder = transfer
        .folder
        .as_deref()
        .and_then(|folder| Path::new(folder).file_name())
        .map(|name| name.to_string_lossy());
    match folder {
        Some(folder) => format!("{} saved in {folder}", transfer.label),
        None => format!("{} saved", transfer.label),
    }
}

fn shortened(text: &str) -> String {
    text.chars().take(MAX_TEXT).collect()
}

#[cfg(windows)]
pub fn show(app: &AppHandle, arrival: Arrival) {
    use tauri_winrt_notification::Toast;

    let app = app.clone();
    // This waits on Windows, and the app's messages from the agent shouldn't wait for it.
    tauri::async_runtime::spawn_blocking(move || {
        let id = app.config().identifier.clone();
        let opens_folder = arrival.folder.is_some();
        let mut toast = Toast::new(&id).title(&arrival.title).text1(&arrival.text);
        if opens_folder {
            toast = toast.add_button("Open folder", "open");
        }
        let toast = toast.on_activated(move |_| {
            match &arrival.folder {
                Some(folder) => crate::open_folder(folder.clone()),
                None => crate::show_window(&app),
            }
            Ok(())
        });
        if let Err(error) = toast.show() {
            tracing::warn!(%error, "can't show a notification");
        }
    });
}

#[cfg(not(windows))]
pub fn show(_app: &AppHandle, arrival: Arrival) {
    tracing::info!(title = arrival.title, "no notifications on this system yet");
}

#[cfg(test)]
mod tests {
    use hk_core::device::Fingerprint;
    use hk_core::rules::Rules;
    use hk_ipc::{ControlView, DeviceStatus, DeviceView, ThisDevice};

    use super::*;

    const ME: Fingerprint = Fingerprint::from_bytes([1; 32]);
    const OTHER: Fingerprint = Fingerprint::from_bytes([2; 32]);

    fn transfer(id: &str, sending: bool, stage: TransferStage) -> TransferView {
        TransferView {
            id: id.into(),
            peer: OTHER,
            sending,
            label: "report.pdf and 2 more".into(),
            total_bytes: 10,
            done_bytes: 10,
            stage,
            folder: Some(r"C:\Users\me\Bureau".into()),
        }
    }

    fn state_with(transfers: Vec<TransferView>) -> UiState {
        UiState {
            me: ThisDevice {
                fingerprint: ME,
                name: "here".into(),
                version: "1".into(),
                model: None,
                addresses: vec![],
                port: 1,
            },
            devices: vec![DeviceView {
                fingerprint: OTHER,
                name: "MAIN".into(),
                status: DeviceStatus::Connected,
                model: None,
                addresses: vec![],
                needs_update_for_files: false,
                can_receive_files: true,
                files_allowed: false,
            }],
            screens: vec![],
            pairing: None,
            control: ControlView::Home,
            rules: Rules::default(),
            enabled: true,
            locked: false,
            held_by_fullscreen: false,
            capture_error: None,
            desktop_folder: None,
            transfers,
            incoming_files: None,
            layout_saved: true,
            saved_layouts: 1,
        }
    }

    #[test]
    fn files_that_have_arrived_are_announced_once() {
        let running = state_with(vec![transfer("1", false, TransferStage::Running)]);
        let done = state_with(vec![transfer("1", false, TransferStage::Done)]);

        assert_eq!(arrivals(&running, &running), vec![]);
        assert_eq!(
            arrivals(&running, &done),
            vec![Arrival {
                title: "Files from MAIN".into(),
                text: "report.pdf and 2 more saved in Bureau".into(),
                folder: Some(r"C:\Users\me\Bureau".into()),
            }]
        );
        assert_eq!(arrivals(&done, &done), vec![], "it's old news by then");
    }

    /// A small file can start and finish between two states.
    #[test]
    fn files_that_arrive_all_at_once_are_announced_too() {
        let nothing = state_with(vec![]);
        let done = state_with(vec![transfer("1", false, TransferStage::Done)]);
        assert_eq!(arrivals(&nothing, &done).len(), 1);
    }

    #[test]
    fn failing_is_announced_with_the_reason_and_nowhere_to_look() {
        let running = state_with(vec![transfer("1", false, TransferStage::Running)]);
        let reason = "There isn't enough room".to_string();
        let failed = state_with(vec![transfer("1", false, TransferStage::Failed { reason })]);
        let found = arrivals(&running, &failed);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].title, "Files from MAIN weren't saved");
        assert!(found[0].text.contains("isn't enough room"));
        assert_eq!(found[0].folder, None);
    }

    #[test]
    fn what_the_user_did_themselves_is_not_announced() {
        let before = state_with(vec![
            transfer("sent", true, TransferStage::Running),
            transfer("stopped", false, TransferStage::Running),
        ]);
        let after = state_with(vec![
            transfer("sent", true, TransferStage::Done),
            transfer("stopped", false, TransferStage::Cancelled),
        ]);
        assert_eq!(arrivals(&before, &after), vec![]);
    }

    #[test]
    fn a_very_long_name_from_another_pc_is_cut_short() {
        let long = "é".repeat(5000);
        let mut arriving = transfer("1", false, TransferStage::Done);
        arriving.label = long;
        let found = arrivals(&state_with(vec![]), &state_with(vec![arriving]));
        assert_eq!(found[0].text.chars().count(), MAX_TEXT);
    }
}
