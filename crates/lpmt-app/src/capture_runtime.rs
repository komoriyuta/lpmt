//! Session-only portal connections mapped to persistent media input slots.

use anyhow::Result;
use lpmt_core::model::{MediaAsset, MediaId, Project};
use lpmt_render::{gpu::Gpu, media::MediaCache};
use std::collections::HashMap;
#[cfg(all(feature = "pipewire", target_os = "linux"))]
use std::collections::HashSet;

#[derive(Default)]
pub struct CaptureRuntime {
    #[cfg(all(feature = "pipewire", target_os = "linux"))]
    sessions: Vec<Connection>,
    pub message: String,
}

#[cfg(all(feature = "pipewire", target_os = "linux"))]
struct Connection {
    session: lpmt_media::capture::CaptureSession,
    ids: Option<Vec<MediaId>>,
    reconnect: Option<MediaId>,
    retired: HashSet<MediaId>,
}

impl CaptureRuntime {
    pub const SUPPORTED: bool = cfg!(all(feature = "pipewire", target_os = "linux"));

    pub fn pending(&self) -> bool {
        #[cfg(all(feature = "pipewire", target_os = "linux"))]
        {
            self.sessions
                .iter()
                .any(|connection| connection.ids.is_none())
        }
        #[cfg(not(all(feature = "pipewire", target_os = "linux")))]
        {
            false
        }
    }

    pub fn start(&mut self, reconnect: Option<MediaId>) -> Result<()> {
        #[cfg(all(feature = "pipewire", target_os = "linux"))]
        {
            if self.pending() {
                return Ok(());
            }
            let session = lpmt_media::capture::CaptureSession::select_windows(reconnect.is_none())?;
            self.sessions.push(Connection {
                session,
                ids: None,
                reconnect,
                retired: HashSet::new(),
            });
            self.message = "Choose windows in the desktop sharing dialog".into();
            Ok(())
        }
        #[cfg(not(all(feature = "pipewire", target_os = "linux")))]
        {
            let _ = reconnect;
            anyhow::bail!("Window capture requires Linux and --features pipewire")
        }
    }

    pub fn cancel_pending(&mut self) {
        #[cfg(all(feature = "pipewire", target_os = "linux"))]
        self.sessions.retain(|connection| connection.ids.is_some());
        self.message = "Window selection cancelled".into();
    }

    pub fn stop(&mut self, id: &MediaId, media: &mut MediaCache) {
        #[cfg(all(feature = "pipewire", target_os = "linux"))]
        for connection in &mut self.sessions {
            if let Some(index) = connection
                .ids
                .as_ref()
                .and_then(|ids| ids.iter().position(|entry| entry == id))
            {
                connection.session.stop_window(index);
                connection.retired.insert(id.clone());
            }
        }
        media.remove(id);
    }

    pub fn poll(
        &mut self,
        project: &Project,
        gpu: &Gpu,
        media: &mut MediaCache,
    ) -> Vec<MediaAsset> {
        #[cfg(not(all(feature = "pipewire", target_os = "linux")))]
        {
            let _ = (project, gpu, media);
            Vec::new()
        }
        #[cfg(all(feature = "pipewire", target_os = "linux"))]
        {
            let mut added = Vec::new();
            for connection in &mut self.sessions {
                if connection.ids.is_none() {
                    let Some(count) = connection.session.window_count() else {
                        continue;
                    };
                    if count == 0 {
                        self.message = connection
                            .session
                            .error()
                            .unwrap_or_else(|| "No windows selected".into());
                        connection.ids = Some(Vec::new());
                        continue;
                    }
                    let mut ids = Vec::new();
                    if let Some(id) = &connection.reconnect {
                        if project.media_asset(id).is_some() {
                            ids.push(id.clone());
                        } else {
                            for index in 0..count {
                                connection.session.stop_window(index);
                            }
                        }
                    } else {
                        for _ in 0..count {
                            let asset = next_input(project, &added);
                            ids.push(asset.id.clone());
                            added.push(asset);
                        }
                    }
                    self.message = format!("Connected {} window input(s)", ids.len());
                    connection.ids = Some(ids);
                }
                for (index, id) in connection.ids.as_ref().unwrap().iter().enumerate() {
                    if connection.retired.contains(id) {
                        continue;
                    }
                    if !project
                        .media_asset(id)
                        .is_some_and(|asset| asset.kind == lpmt_core::model::MediaKind::PipeWire)
                        && !added.iter().any(|asset| &asset.id == id)
                    {
                        connection.session.stop_window(index);
                    }
                    if connection.session.is_stopped(index) {
                        media.remove(id);
                    } else if let Some(image) = connection.session.take_frame(index) {
                        if let Err(error) = media.update_image(gpu, id.clone(), &image) {
                            self.message = format!("Window upload failed: {error:#}");
                            connection.session.stop_window(index);
                            media.remove(id);
                        }
                    }
                }
            }
            // Discard failed selections and fully removed sessions. Keep stopped
            // slots while the project refers to them, so their error stays visible.
            self.sessions.retain(|connection| {
                connection.ids.as_ref().is_none_or(|ids| {
                    ids.iter().any(|id| {
                        !connection.retired.contains(id)
                            && (project.media_asset(id).is_some()
                                || added.iter().any(|asset| &asset.id == id))
                    })
                })
            });
            added
        }
    }

    pub fn statuses(&self) -> HashMap<MediaId, String> {
        #[cfg(not(all(feature = "pipewire", target_os = "linux")))]
        {
            HashMap::new()
        }
        #[cfg(all(feature = "pipewire", target_os = "linux"))]
        {
            let mut statuses = HashMap::new();
            for connection in &self.sessions {
                if let Some(ids) = &connection.ids {
                    for (index, id) in ids.iter().enumerate() {
                        if connection.retired.contains(id) {
                            continue;
                        }
                        statuses.insert(id.clone(), connection.session.status(index));
                    }
                }
            }
            statuses
        }
    }
}

#[cfg(any(test, all(feature = "pipewire", target_os = "linux")))]
fn next_input(project: &Project, added: &[MediaAsset]) -> MediaAsset {
    let mut index = 1;
    loop {
        let id = format!("pipewire-{index}");
        if !project
            .media
            .iter()
            .chain(added)
            .any(|asset| asset.id.as_str() == id)
        {
            return MediaAsset::pipewire(id, format!("Window {index}"));
        }
        index += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn multiple_windows_and_existing_slots_have_distinct_ids() {
        let mut project = Project::demo();
        project
            .media
            .push(MediaAsset::pipewire("pipewire-1", "Saved window"));
        let first = next_input(&project, &[]);
        let second = next_input(&project, std::slice::from_ref(&first));
        assert_eq!(first.id.as_str(), "pipewire-2");
        assert_eq!(second.id.as_str(), "pipewire-3");
    }
}
