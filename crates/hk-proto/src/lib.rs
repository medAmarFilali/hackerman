//! Messages exchanged between Hackerman peers, and their binary encoding.

#![forbid(unsafe_code)]

use hk_core::clipboard::{ClipVersion, ClipboardData, SharedFile};
use hk_core::input::InputEvent;
use hk_core::layout::{LayoutDoc, Monitor};
use serde::{Deserialize, Serialize};

/// Peers exchange this in [`Message::Hello`] and refuse any other number, so the
/// version must stay the first thing in `Hello`, and `Hello` the first variant.
///
/// Raising it cuts off every copy of Hackerman already out there, so it stays put
/// unless there's truly no other way. New abilities are added like this instead, which
/// keeps old and new versions working together:
///
/// - Anything new goes at the very end of `Hello` or [`ClipboardBlob`]. Decoding stops
///   when it has what it knows of, so an older peer doesn't see it, and it's optional
///   when reading, because an older peer doesn't send it.
/// - A new message or stream gets a bit in [`FEATURES`], and only goes to a peer that
///   announced that bit. Version 4 peers drop the connection on a message they don't
///   know. Peers from this version on just skip it.
/// - What's already inside a message can't change, since nothing marks where it ends.
pub const PROTOCOL_VERSION: u16 = 4;

/// Can receive files on the clipboard, and supply their content on request.
pub const FEATURE_FILES: u32 = 1 << 0;

/// Can be sent files to save in a folder, and report how that goes.
pub const FEATURE_SEND_FILES: u32 = 1 << 1;

/// Everything this version can do beyond the base protocol, for [`Message::Hello`].
pub const FEATURES: u32 = FEATURE_FILES | FEATURE_SEND_FILES;

/// A message sent from one peer to another.
///
/// The encoding identifies variants by position, so new variants must only ever be
/// appended at the end. See [`PROTOCOL_VERSION`] for how to add things.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Message {
    /// First message on every new connection, sent by both sides.
    Hello {
        protocol_version: u16,
        device_name: String,
        monitors: Vec<Monitor>,
        layout: LayoutDoc,
        /// The make and model of the sender, like "Dell Latitude 7420", if it says.
        model: Option<String>,
        /// Whether the sender has sharing turned on. See [`Message::Enabled`].
        enabled: bool,
        /// What the sender can do beyond the base protocol. See [`FEATURES`].
        features: u32,
    },
    /// Input for the receiver to replay as if it came from its own devices.
    Input(InputEvent),
    /// The sender started controlling the receiver.
    Enter,
    /// The sender stopped controlling the receiver, which should release any
    /// keys and buttons still held.
    Leave,
    /// The sender's monitors changed.
    Monitors(Vec<Monitor>),
    /// Someone edited the layout. The receiver keeps whichever copy is newer.
    Layout(LayoutDoc),
    /// The sender has a new name.
    Renamed(String),
    /// The sender's clipboard changed. The content stays there until asked for.
    ClipboardOffer(ClipVersion),
    /// Asks the owner of this version to send it, as a [`ClipboardBlob`].
    ClipboardRequest(ClipVersion),
    /// The sender's user turned sharing on or off. While it's off, the sender ignores
    /// input and clipboards, and the cursor should pass over it as if it were offline.
    Enabled(bool),
    /// How saving files is going that the receiver was sent to save. Needs
    /// [`FEATURE_SEND_FILES`].
    Transfer(ClipVersion, TransferUpdate),
    /// The sender of those files no longer wants them saved. Needs
    /// [`FEATURE_SEND_FILES`].
    TransferCancel(ClipVersion),
}

/// See [`Message::Transfer`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TransferUpdate {
    /// Saving has started.
    Accepted,
    /// How many bytes of it are done.
    Progress(u64),
    /// It's all there, in this folder.
    Saved {
        folder: String,
    },
    /// It didn't work, or was turned down, for this reason.
    Failed(String),
    Cancelled,
}

/// Clipboard content. It can be large, so it travels on a stream of its own
/// rather than as a [`Message`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClipboardBlob {
    pub version: ClipVersion,
    pub data: ClipboardData,
    /// Files, whose content is asked for with a [`FileRequest`] when they're pasted.
    pub files: Vec<SharedFile>,
    /// These files are to be saved in the receiver's folder for that, rather than going
    /// on its clipboard. Only for a peer with [`FEATURE_SEND_FILES`].
    pub save_to_folder: bool,
}

/// Asks the owner of a clipboard for the content of one of its files. Sent on a stream
/// of its own, which the answer then comes back on. Needs [`FEATURE_FILES`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileRequest {
    pub version: ClipVersion,
    /// The position in [`ClipboardBlob::files`].
    pub index: u32,
    /// How far into the file to start.
    pub offset: u64,
}

/// The first thing back on the stream of a [`FileRequest`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FileAnswer {
    /// The file's content follows, until the stream ends.
    Content,
    /// That won't work however often it's tried, for this reason.
    Refused(String),
}

#[derive(Debug, thiserror::Error)]
#[error("invalid message encoding")]
pub struct CodecError(#[from] postcard::Error);

pub fn encode(message: &Message) -> Result<Vec<u8>, CodecError> {
    Ok(postcard::to_allocvec(message)?)
}

pub fn decode(bytes: &[u8]) -> Result<Message, CodecError> {
    match hello_version(bytes) {
        // Another version's `Hello` may hold other things, but its version comes first.
        // Reporting that is much more useful than failing to read the rest.
        Some(version) if version != PROTOCOL_VERSION => Ok(Message::Hello {
            protocol_version: version,
            device_name: String::new(),
            monitors: Vec::new(),
            layout: LayoutDoc::default(),
            model: None,
            enabled: true,
            features: 0,
        }),
        Some(_) => decode_hello(&bytes[1..]),
        None => Ok(postcard::from_bytes(bytes)?),
    }
}

fn hello_version(bytes: &[u8]) -> Option<u16> {
    let (&variant, rest) = bytes.split_first()?;
    if variant != 0 {
        return None;
    }
    let (version, _) = postcard::take_from_bytes::<u16>(rest).ok()?;
    Some(version)
}

/// Reads one field after another, since an older peer stops earlier than this version
/// does, and that's not an error for the fields it has never heard of.
fn decode_hello(bytes: &[u8]) -> Result<Message, CodecError> {
    let (protocol_version, rest) = postcard::take_from_bytes(bytes)?;
    let (device_name, rest) = postcard::take_from_bytes(rest)?;
    let (monitors, rest) = postcard::take_from_bytes(rest)?;
    let (layout, rest) = postcard::take_from_bytes(rest)?;
    let (model, rest) = postcard::take_from_bytes(rest)?;
    let (enabled, rest) = postcard::take_from_bytes(rest)?;
    let (features, _) = take_if_present(rest)?;
    Ok(Message::Hello {
        protocol_version,
        device_name,
        monitors,
        layout,
        model,
        enabled,
        features,
    })
}

/// The next field, or its default value at the end of what an older peer sent.
fn take_if_present<'a, T>(bytes: &'a [u8]) -> Result<(T, &'a [u8]), CodecError>
where
    T: Default + Deserialize<'a>,
{
    if bytes.is_empty() {
        return Ok((T::default(), bytes));
    }
    Ok(postcard::take_from_bytes(bytes)?)
}

pub fn encode_clipboard(blob: &ClipboardBlob) -> Result<Vec<u8>, CodecError> {
    Ok(postcard::to_allocvec(blob)?)
}

pub fn decode_clipboard(bytes: &[u8]) -> Result<ClipboardBlob, CodecError> {
    let (version, rest) = postcard::take_from_bytes(bytes)?;
    let (data, rest) = postcard::take_from_bytes(rest)?;
    let (files, rest) = take_if_present(rest)?;
    let (save_to_folder, _) = take_if_present(rest)?;
    Ok(ClipboardBlob {
        version,
        data,
        files,
        save_to_folder,
    })
}

pub fn encode_file_request(request: &FileRequest) -> Result<Vec<u8>, CodecError> {
    Ok(postcard::to_allocvec(request)?)
}

pub fn decode_file_request(bytes: &[u8]) -> Result<FileRequest, CodecError> {
    Ok(postcard::from_bytes(bytes)?)
}

pub fn encode_file_answer(answer: &FileAnswer) -> Result<Vec<u8>, CodecError> {
    Ok(postcard::to_allocvec(answer)?)
}

pub fn decode_file_answer(bytes: &[u8]) -> Result<FileAnswer, CodecError> {
    Ok(postcard::from_bytes(bytes)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hk_core::geometry::Rect;
    use hk_core::input::{KeyCode, MouseButton};

    #[test]
    fn clipboard_content_round_trips() {
        let blob = ClipboardBlob {
            version: ClipVersion {
                counter: 7,
                owner: hk_core::device::Fingerprint::from_bytes([3; 32]),
            },
            data: ClipboardData {
                text: Some("héllo".into()),
                html: Some("<b>héllo</b>".into()),
                image: Some(hk_core::clipboard::ClipboardImage {
                    width: 2,
                    height: 1,
                    png: vec![1, 2, 3],
                }),
            },
            files: vec![SharedFile {
                path: "photos/été.jpg".into(),
                size: 5_000_000_000,
                is_dir: false,
                modified_ms: Some(1_700_000_000_000),
            }],
            save_to_folder: true,
        };
        let bytes = encode_clipboard(&blob).unwrap();
        assert_eq!(decode_clipboard(&bytes).unwrap(), blob);
    }

    #[test]
    fn file_requests_and_answers_round_trip() {
        let request = FileRequest {
            version: ClipVersion {
                counter: 1_700_000_000_000,
                owner: hk_core::device::Fingerprint::from_bytes([3; 32]),
            },
            index: 70_000,
            offset: 5_000_000_000,
        };
        let bytes = encode_file_request(&request).unwrap();
        assert_eq!(decode_file_request(&bytes).unwrap(), request);
        for answer in [FileAnswer::Content, FileAnswer::Refused("gone".into())] {
            let bytes = encode_file_answer(&answer).unwrap();
            assert_eq!(decode_file_answer(&bytes).unwrap(), answer);
        }
    }

    /// Exact copies of what version 0.4.0 sends and understands. These must never be
    /// changed to match the code: they're what's out there on people's computers.
    mod version_0_4 {
        use super::*;

        #[derive(Debug, PartialEq, Serialize, Deserialize)]
        pub enum Message {
            Hello {
                protocol_version: u16,
                device_name: String,
                monitors: Vec<Monitor>,
                layout: LayoutDoc,
                model: Option<String>,
                enabled: bool,
            },
            Input(InputEvent),
            Enter,
            Leave,
            Monitors(Vec<Monitor>),
            Layout(LayoutDoc),
            Renamed(String),
            ClipboardOffer(ClipVersion),
            ClipboardRequest(ClipVersion),
            Enabled(bool),
        }

        #[derive(Debug, PartialEq, Serialize, Deserialize)]
        pub struct ClipboardBlob {
            pub version: ClipVersion,
            pub data: ClipboardData,
        }

        pub fn hello() -> Message {
            Message::Hello {
                protocol_version: 4,
                device_name: "old-pc".into(),
                monitors: vec![Monitor {
                    id: "DEL41AB".into(),
                    name: "DELL U2720Q".into(),
                    bounds: Rect::new(0.0, 0.0, 3840.0, 2160.0),
                    scale: 1.5,
                    primary: true,
                }],
                layout: LayoutDoc::default(),
                model: Some("Dell Latitude 7420".into()),
                enabled: false,
            }
        }
    }

    #[test]
    fn this_version_understands_a_hello_from_0_4() {
        let bytes = postcard::to_allocvec(&version_0_4::hello()).unwrap();
        match decode(&bytes).unwrap() {
            Message::Hello {
                protocol_version,
                device_name,
                model,
                enabled,
                features,
                monitors,
                ..
            } => {
                assert_eq!(protocol_version, 4);
                assert_eq!(device_name, "old-pc");
                assert_eq!(model.as_deref(), Some("Dell Latitude 7420"));
                assert!(!enabled);
                assert_eq!(monitors.len(), 1);
                assert_eq!(features, 0, "it announced no features, so it has none");
            }
            other => panic!("expected Hello, got {other:?}"),
        }
    }

    #[test]
    fn version_0_4_understands_a_hello_from_this_version() {
        let bytes = encode(&Message::Hello {
            protocol_version: PROTOCOL_VERSION,
            device_name: "new-pc".into(),
            monitors: vec![],
            layout: LayoutDoc::default(),
            model: None,
            enabled: true,
            features: FEATURES,
        })
        .unwrap();
        // This is how it decodes, and what it checks first.
        let decoded: version_0_4::Message = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(
            decoded,
            version_0_4::Message::Hello {
                protocol_version: 4,
                device_name: "new-pc".into(),
                monitors: vec![],
                layout: LayoutDoc::default(),
                model: None,
                enabled: true,
            }
        );
    }

    #[test]
    fn every_message_0_4_knows_still_means_the_same() {
        let owner = hk_core::device::Fingerprint::from_bytes([3; 32]);
        let version = ClipVersion { counter: 7, owner };
        let event = InputEvent::PointerPosition { x: 1.0, y: 2.0 };
        let pairs = [
            (version_0_4::Message::Input(event), Message::Input(event)),
            (version_0_4::Message::Enter, Message::Enter),
            (version_0_4::Message::Leave, Message::Leave),
            (
                version_0_4::Message::Monitors(vec![]),
                Message::Monitors(vec![]),
            ),
            (
                version_0_4::Message::Layout(LayoutDoc::default()),
                Message::Layout(LayoutDoc::default()),
            ),
            (
                version_0_4::Message::Renamed("a".into()),
                Message::Renamed("a".into()),
            ),
            (
                version_0_4::Message::ClipboardOffer(version),
                Message::ClipboardOffer(version),
            ),
            (
                version_0_4::Message::ClipboardRequest(version),
                Message::ClipboardRequest(version),
            ),
            (version_0_4::Message::Enabled(true), Message::Enabled(true)),
        ];
        for (old, new) in pairs {
            let old_bytes = postcard::to_allocvec(&old).unwrap();
            assert_eq!(old_bytes, encode(&new).unwrap(), "{old:?}");
            assert_eq!(decode(&old_bytes).unwrap(), new);
        }
    }

    #[test]
    fn clipboards_travel_both_ways_with_0_4() {
        let version = ClipVersion {
            counter: 7,
            owner: hk_core::device::Fingerprint::from_bytes([3; 32]),
        };
        let data = ClipboardData {
            text: Some("hello".into()),
            ..Default::default()
        };

        let from_old = postcard::to_allocvec(&version_0_4::ClipboardBlob {
            version,
            data: data.clone(),
        })
        .unwrap();
        let read = decode_clipboard(&from_old).unwrap();
        assert_eq!(read.data, data);
        assert!(read.files.is_empty());

        let from_new = encode_clipboard(&ClipboardBlob {
            version,
            data: data.clone(),
            files: vec![],
            save_to_folder: false,
        })
        .unwrap();
        let read: version_0_4::ClipboardBlob = postcard::from_bytes(&from_new).unwrap();
        assert_eq!(read.data, data);
    }

    /// Exactly what version 0.5.0 sends with files on the clipboard. It must never be
    /// taken for files to save, which it knows nothing about.
    #[test]
    fn files_from_0_5_go_on_the_clipboard() {
        #[derive(Serialize)]
        struct Blob0_5 {
            version: ClipVersion,
            data: ClipboardData,
            files: Vec<SharedFile>,
        }
        let file = SharedFile {
            path: "report.pdf".into(),
            size: 10,
            is_dir: false,
            modified_ms: None,
        };
        let bytes = postcard::to_allocvec(&Blob0_5 {
            version: ClipVersion {
                counter: 7,
                owner: hk_core::device::Fingerprint::from_bytes([3; 32]),
            },
            data: ClipboardData::default(),
            files: vec![file.clone()],
        })
        .unwrap();
        let read = decode_clipboard(&bytes).unwrap();
        assert_eq!(read.files, [file]);
        assert!(!read.save_to_folder);
    }

    #[test]
    fn nothing_new_was_added_to_what_0_5_can_read_except_at_the_end() {
        assert_eq!(FEATURE_FILES, 1, "0.5.0 announces this bit");
        assert_eq!(FEATURES & FEATURE_FILES, FEATURE_FILES);
    }

    /// The same trick has to keep working for whatever comes after this version.
    #[test]
    fn extra_fields_from_a_future_version_are_skipped() {
        let mut bytes = encode(&Message::Hello {
            protocol_version: PROTOCOL_VERSION,
            device_name: "future-pc".into(),
            monitors: vec![],
            layout: LayoutDoc::default(),
            model: None,
            enabled: true,
            features: u32::MAX,
        })
        .unwrap();
        bytes.extend_from_slice(b"something added later");
        assert!(matches!(
            decode(&bytes).unwrap(),
            Message::Hello {
                features: u32::MAX,
                ..
            }
        ));
    }

    /// An older peer must still be able to read this to report the version
    /// mismatch, rather than failing with a confusing decoding error.
    #[test]
    fn hello_stays_the_first_variant() {
        let bytes = encode(&Message::Hello {
            protocol_version: 9,
            device_name: "x".into(),
            monitors: vec![],
            layout: LayoutDoc::default(),
            model: None,
            enabled: true,
            features: FEATURES,
        })
        .unwrap();
        assert_eq!(bytes[0], 0, "Hello must stay first");
        assert_eq!(bytes[1], 9, "the version must come straight after");
    }

    #[test]
    fn another_versions_hello_is_read_as_far_as_the_version() {
        // Variant 0, version 2, then something this version can't make sense of.
        let older = [0, 2, 0xff, 0xff, 0xff, 0xff, 0xff];
        match decode(&older).unwrap() {
            Message::Hello {
                protocol_version, ..
            } => assert_eq!(protocol_version, 2),
            other => panic!("expected Hello, got {other:?}"),
        }
    }

    #[test]
    fn garbage_with_this_versions_number_is_still_garbage() {
        let version = u8::try_from(PROTOCOL_VERSION).unwrap();
        assert!(decode(&[0, version, 0xff, 0xff, 0xff]).is_err());
    }

    fn round_trip(message: Message) {
        let bytes = encode(&message).unwrap();
        assert_eq!(decode(&bytes).unwrap(), message);
    }

    #[test]
    fn every_message_round_trips() {
        round_trip(Message::Hello {
            protocol_version: PROTOCOL_VERSION,
            device_name: "office-pc".into(),
            monitors: vec![Monitor {
                id: "DEL41AB".into(),
                name: "DELL U2720Q".into(),
                bounds: Rect::new(-1920.0, 0.0, 3840.0, 2160.0),
                scale: 1.5,
                primary: true,
            }],
            layout: LayoutDoc::default(),
            model: Some("Dell Latitude 7420".into()),
            enabled: false,
            features: FEATURES,
        });
        round_trip(Message::Enabled(false));
        let id = ClipVersion {
            counter: 7,
            owner: hk_core::device::Fingerprint::from_bytes([3; 32]),
        };
        for update in [
            TransferUpdate::Accepted,
            TransferUpdate::Progress(5_000_000_000),
            TransferUpdate::Saved {
                folder: "D:\\Bureau".into(),
            },
            TransferUpdate::Failed("no room".into()),
            TransferUpdate::Cancelled,
        ] {
            round_trip(Message::Transfer(id, update));
        }
        round_trip(Message::TransferCancel(id));
        round_trip(Message::Enter);
        round_trip(Message::Leave);
        round_trip(Message::Monitors(vec![]));
        round_trip(Message::Layout(LayoutDoc::default()));
        round_trip(Message::Renamed("desk".into()));
        round_trip(Message::Input(InputEvent::PointerPosition {
            x: 2559.0,
            y: 719.5,
        }));
        round_trip(Message::Input(InputEvent::PointerMotion {
            dx: -3.5,
            dy: 12.0,
        }));
        round_trip(Message::Input(InputEvent::Button {
            button: MouseButton::Forward,
            pressed: true,
        }));
        round_trip(Message::Input(InputEvent::Scroll { dx: 0.0, dy: -0.25 }));
        round_trip(Message::Input(InputEvent::Key {
            code: KeyCode::ControlLeft,
            pressed: false,
        }));
    }

    #[test]
    fn input_messages_stay_small() {
        let motion = encode(&Message::Input(InputEvent::PointerMotion {
            dx: 1.0,
            dy: 1.0,
        }))
        .unwrap();
        assert!(
            motion.len() <= 18,
            "pointer motion is {} bytes",
            motion.len()
        );
    }

    #[test]
    fn garbage_is_rejected() {
        assert!(decode(&[0xff, 0xff, 0xff]).is_err());
        assert!(decode(&[]).is_err());
    }
}
