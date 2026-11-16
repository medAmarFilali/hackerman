//! Developer mode, as far as the agent goes: what to tell the user, and keeping what another
//! device sends to a sensible size. The work itself is the platform's: see `hk_core::dev`.

use hk_core::dev::{self, DevReply, MAX_LISTED, MemoryAlert};

/// How many requests from one device are answered at once. Each can take minutes.
pub(crate) const MAX_AT_ONCE: usize = 2;

/// In gigabytes as Windows counts them, 1024 megabytes each.
pub(crate) fn size(bytes: u64) -> String {
    let gigabytes = bytes as f64 / (1024.0 * 1024.0 * 1024.0);
    if gigabytes >= 1.0 {
        format!("{gigabytes:.1} GB")
    } else {
        format!("{:.0} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

pub(crate) fn alert_text(alert: &MemoryAlert) -> String {
    let name = match &alert.project {
        Some(project) => format!("{} ({project})", alert.name),
        None => alert.name.clone(),
    };
    dev::shorten(&format!(
        "{name} is using {}, more than the {} set in Settings",
        size(alert.memory),
        size(alert.limit)
    ))
}

/// What to tell the user here, once another device has done something with developer mode.
/// Nothing if it didn't do anything.
pub(crate) fn notice_text(reply: &DevReply) -> Option<String> {
    match reply {
        DevReply::Done { summary } => Some(dev::shorten(summary)),
        DevReply::Cleaned { results } => {
            let freed: u64 = results.iter().map(|result| result.freed).sum();
            let cleaned = results.iter().filter(|result| result.freed > 0).count();
            let things = if cleaned == 1 { "thing" } else { "things" };
            Some(format!(
                "Cleaned up {cleaned} {things}, freeing {}",
                size(freed)
            ))
        }
        _ => None,
    }
}

/// Keeps another device's answer to what this version would send itself.
pub(crate) fn sanitized(reply: DevReply) -> DevReply {
    match reply {
        DevReply::Programs(mut report) => {
            report.programs.truncate(MAX_LISTED);
            report.build_helpers.truncate(MAX_LISTED);
            report.ports.truncate(MAX_LISTED);
            DevReply::Programs(report)
        }
        DevReply::Done { summary } => DevReply::Done {
            summary: dev::shorten(&summary),
        },
        DevReply::Versions { mut tools } => {
            tools.truncate(MAX_LISTED);
            DevReply::Versions { tools }
        }
        DevReply::Docker(mut report) => {
            report.containers.truncate(MAX_LISTED);
            report.usage.truncate(MAX_LISTED);
            DevReply::Docker(report)
        }
        DevReply::Disk(mut report) => {
            report.folders.truncate(MAX_LISTED);
            report.drives.truncate(MAX_LISTED);
            report.caches.truncate(MAX_LISTED);
            report.roots.truncate(MAX_LISTED);
            DevReply::Disk(report)
        }
        DevReply::Cleaned { mut results } => {
            results.truncate(MAX_LISTED);
            DevReply::Cleaned { results }
        }
        DevReply::Refused { reason } => DevReply::Refused {
            reason: dev::shorten(&reason),
        },
    }
}

#[cfg(test)]
mod tests {
    use hk_core::dev::{CacheKind, CleanItem, CleanResult};

    use super::*;

    #[test]
    fn sizes_are_in_gigabytes_as_windows_counts_them() {
        assert_eq!(size(4 * 1024 * 1024 * 1024), "4.0 GB");
        assert_eq!(size(5_583_457_484), "5.2 GB");
        assert_eq!(size(300 * 1024 * 1024), "300 MB");
    }

    #[test]
    fn an_alert_says_what_and_how_much() {
        let alert = MemoryAlert {
            name: "Next.js".into(),
            project: Some("site".into()),
            memory: 5_583_457_484,
            limit: 4 * 1024 * 1024 * 1024,
        };
        assert_eq!(
            alert_text(&alert),
            "Next.js (site) is using 5.2 GB, more than the 4.0 GB set in Settings"
        );
    }

    #[test]
    fn only_what_was_done_is_told() {
        assert_eq!(
            notice_text(&DevReply::Done {
                summary: "Stopped Next.js (site)".into()
            })
            .as_deref(),
            Some("Stopped Next.js (site)")
        );
        let cleaned = DevReply::Cleaned {
            results: vec![
                CleanResult {
                    item: CleanItem::Cache {
                        kind: CacheKind::Npm,
                    },
                    freed: 2 * 1024 * 1024 * 1024,
                    problem: None,
                },
                CleanResult {
                    item: CleanItem::Folder { path: "x".into() },
                    freed: 0,
                    problem: Some("in use".into()),
                },
            ],
        };
        assert_eq!(
            notice_text(&cleaned).as_deref(),
            Some("Cleaned up 1 thing, freeing 2.0 GB")
        );
        assert_eq!(
            notice_text(&DevReply::Refused {
                reason: "no".into()
            }),
            None
        );
    }
}
