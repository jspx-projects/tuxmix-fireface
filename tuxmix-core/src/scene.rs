use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::channel::{InputChannel, OutputChannel, PlaybackChannel};
use crate::device::DeviceSettings;
use crate::error::Error;

/// Directory for saved scenes/presets (~/.local/share/tuxmix/scenes).
pub fn scenes_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    let dir = PathBuf::from(home).join(".local/share/tuxmix/scenes");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// Path of the SHARED auto-saved state. Both the GUI and the TUI load
/// this on open and save to it on change/exit — the device has no
/// gain/volume readback, so this single file is how the two UIs stay in
/// sync (the last writer wins; saving on exit avoids restoring a stale
/// state).
pub fn auto_scene_path() -> PathBuf {
    scenes_dir().join("auto.json")
}

/// Load the last auto-saved mixer state, if any.
pub fn load_auto_scene() -> Option<Scene> {
    let content = std::fs::read_to_string(auto_scene_path()).ok()?;
    Scene::from_json(&content).ok()
}

/// Save the mixer state to the shared auto file.
pub fn save_auto_scene(scene: &Scene) -> Result<(), String> {
    std::fs::write(
        auto_scene_path(),
        scene.to_json().map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())
}

/// Per-model auto-scene path.  Used when switching between devices so
/// each backend keeps its own last TuxMix setup instead of sharing one
/// `auto.json` across UCX/UC/Babyface.
pub fn auto_scene_path_for(model: &str) -> PathBuf {
    let safe: String = model
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .collect();
    scenes_dir().join(format!("auto-{safe}.json"))
}

/// Load the last auto-saved state for one model.
pub fn load_auto_scene_for(model: &str) -> Option<Scene> {
    let content = std::fs::read_to_string(auto_scene_path_for(model)).ok()?;
    Scene::from_json(&content).ok()
}

/// Save one model's auto scene.
pub fn save_auto_scene_for(model: &str, scene: &Scene) -> Result<(), String> {
    std::fs::write(
        auto_scene_path_for(model),
        scene.to_json().map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())
}

/// Read the shared auto file back WITHOUT re-applying it, returning the
/// raw JSON string (or `None` if absent/unreadable).
pub fn read_auto_scene_raw() -> Option<String> {
    std::fs::read_to_string(auto_scene_path()).ok()
}

/// Detect whether the OTHER UI (GUI ↔ TUI) has written the shared auto
/// file since our last write.
///
/// Both UIs auto-save their in-memory state every few seconds and on
/// exit, and neither has a hardware readback to re-derive the other's
/// changes — so without a check the last UI to save would clobber the
/// other's state with its own (possibly stale) copy. Before saving,
/// call this: if it returns `Some`, re-apply that scene (the other UI's
/// changes) to the device first, THEN save your own capture.
///
/// `our_last_json` = the JSON string of our most recent write (pass the
/// UI's `last_saved_json`). If the file on disk differs, another UI has
/// written since — return the parsed scene.
pub fn auto_scene_written_by_other(our_last_json: Option<&str>) -> Option<Scene> {
    let raw = read_auto_scene_raw()?;
    if our_last_json.is_some_and(|ours| ours == raw) {
        return None; // the file still holds OUR last write
    }
    // Either we never wrote (fresh start, file pre-exists) or another UI
    // wrote after us — reload their state.
    Scene::from_json(&raw).ok()
}

/// A snapshot of the full device state, serializable for
/// save/restore (scenes / presets).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Scene {
    /// Name of the scene (user-defined).
    pub name: String,

    /// The `RmeDevice::model_name()` this scene was captured from.
    /// Empty = legacy scene, captured before this field existed —
    /// treated as "unknown, skip the compatibility check" rather than
    /// a hard mismatch. Applying a scene by blind positional index to
    /// a different model's channel layout would silently write wrong
    /// values, so this is checked before `apply_scene` mutates anything.
    #[serde(default)]
    pub model: String,

    /// Mono matrix cells that the per-pair channel model cannot express:
    /// one source (hardware input 0–17, then playback 0–17 at 18–35) to
    /// one physical output block (0–17).  The UC/UCX matrix is 36×18 and
    /// monaural, so a route can exist on only one side of an output pair.
    /// Empty for the stereo-pair-only backends and for legacy scenes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub matrix_overrides: Vec<MatrixOverride>,

    /// Hardware input channels.
    pub inputs: Vec<InputChannel>,

    /// Software playback channels.
    pub playbacks: Vec<PlaybackChannel>,

    /// Physical output channels.
    pub outputs: Vec<OutputChannel>,

    /// Global device-level settings.
    pub settings: DeviceSettings,
}

/// One mono matrix cell that is not implied by the per-pair channel model.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct MatrixOverride {
    /// 0–17 hardware input, 18–35 software playback.
    pub source: usize,
    /// Physical output block 0–17.
    pub block: usize,
    /// Linear gain, 1.0 = 0 dB.
    pub gain: f32,
}

impl Scene {
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            model: String::new(),
            matrix_overrides: Vec::new(),
            inputs: Vec::new(),
            playbacks: Vec::new(),
            outputs: Vec::new(),
            settings: DeviceSettings {
                clock_source: "Internal".into(),
                clock_sources: Vec::new(),
                spdif_optical: false,
                spdif_emphasis: false,
                spdif_professional: false,
                spdif_enabled: false,
                pitch_percent: 0.0,
                ms_proc: false,
                an12: false,
                eq_for_record: false,
                optical_out_spdif: false,
                dim: false,
                speaker_b: false,
                main_out: None,
                main_b_out: None,
                mono: false,
                mute_fx: false,
                fx_send_db: None,
                reverb: None,
                echo: None,
                width: 0.0,
                sample_rate: 48_000,
                line_in_ref: 0,
                line_in_ref_58: 0,
                line_out_ref: 2,
                phones_ref: 1,
                word_clock_single_speed: false,
                word_clock_termination: false,
                input_link: true,
                output_link: Vec::new(),
                input_pair_link: Vec::new(),
            },
        }
    }

    /// Serialize the scene to a JSON string.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }

    /// Deserialize a scene from a JSON string.
    pub fn from_json(json: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json)
    }

    /// Merge the host-originated part of a `before` → `after` transition
    /// into this saved scene.
    ///
    /// `before` and `after` are captures of the live model around one
    /// GUI/OSC message.  Values a front-panel encoder changed earlier are
    /// present in both captures, so they do not appear as differences; only
    /// the fields the host actually changed are copied.  Device-owned fields
    /// (preamp gain, switches, references, clock/rate/format) are never
    /// copied, matching the Windows TotalMix state file.
    pub fn merge_host_changes(&mut self, before: &Scene, after: &Scene) {
        if self.inputs.len() == after.inputs.len() {
            for (host, (b, a)) in self
                .inputs
                .iter_mut()
                .zip(before.inputs.iter().zip(after.inputs.iter()))
            {
                if a.volumes != b.volumes {
                    host.volumes = a.volumes.clone();
                }
                if a.pans != b.pans {
                    host.pans = a.pans.clone();
                }
                if a.mute != b.mute {
                    host.mute = a.mute;
                }
                if a.solo != b.solo {
                    host.solo = a.solo;
                }
                if a.trim != b.trim {
                    host.trim = a.trim;
                }
                if a.width != b.width {
                    host.width = a.width;
                }
                if a.phase != b.phase {
                    host.phase = a.phase;
                }
                if a.eq != b.eq {
                    host.eq = a.eq;
                }
                if a.autoset != b.autoset {
                    host.autoset = a.autoset;
                }
                if a.dynamics != b.dynamics {
                    host.dynamics = a.dynamics;
                }
                if a.name != b.name {
                    host.name = a.name.clone();
                }
            }
        } else {
            self.inputs = after.inputs.clone();
        }

        if self.playbacks.len() == after.playbacks.len() {
            for (host, (b, a)) in self
                .playbacks
                .iter_mut()
                .zip(before.playbacks.iter().zip(after.playbacks.iter()))
            {
                if a != b {
                    *host = a.clone();
                }
            }
        } else {
            self.playbacks = after.playbacks.clone();
        }

        if self.outputs.len() == after.outputs.len() {
            for (host, (b, a)) in self
                .outputs
                .iter_mut()
                .zip(before.outputs.iter().zip(after.outputs.iter()))
            {
                if a.volume != b.volume {
                    host.volume = a.volume;
                }
                if a.mute != b.mute {
                    host.mute = a.mute;
                }
                if a.solo != b.solo {
                    host.solo = a.solo;
                }
                if a.eq != b.eq {
                    host.eq = a.eq;
                }
                if a.dynamics != b.dynamics {
                    host.dynamics = a.dynamics;
                }
                if a.balance != b.balance {
                    host.balance = a.balance;
                }
                if a.fx_return_db != b.fx_return_db {
                    host.fx_return_db = a.fx_return_db;
                }
                if a.loopback != b.loopback {
                    host.loopback = a.loopback;
                }
                if a.phase_l != b.phase_l {
                    host.phase_l = a.phase_l;
                }
                if a.phase_r != b.phase_r {
                    host.phase_r = a.phase_r;
                }
                if a.no_trim != b.no_trim {
                    host.no_trim = a.no_trim;
                }
                if a.name != b.name {
                    host.name = a.name.clone();
                }
            }
        } else {
            self.outputs = after.outputs.clone();
        }

        // Host-owned global settings only.  Clock source, rate, references,
        // digital formats and word-clock options are device-owned and stay
        // as the unit reported them.
        let (b, a) = (&before.settings, &after.settings);
        let h = &mut self.settings;
        if a.spdif_optical != b.spdif_optical {
            h.spdif_optical = a.spdif_optical;
        }
        if a.spdif_emphasis != b.spdif_emphasis {
            h.spdif_emphasis = a.spdif_emphasis;
        }
        if a.spdif_enabled != b.spdif_enabled {
            h.spdif_enabled = a.spdif_enabled;
        }
        if a.ms_proc != b.ms_proc {
            h.ms_proc = a.ms_proc;
        }
        if a.an12 != b.an12 {
            h.an12 = a.an12;
        }
        if a.eq_for_record != b.eq_for_record {
            h.eq_for_record = a.eq_for_record;
        }
        if a.dim != b.dim {
            h.dim = a.dim;
        }
        if a.speaker_b != b.speaker_b {
            h.speaker_b = a.speaker_b;
        }
        if a.mono != b.mono {
            h.mono = a.mono;
        }
        if a.mute_fx != b.mute_fx {
            h.mute_fx = a.mute_fx;
        }
        if a.fx_send_db != b.fx_send_db {
            h.fx_send_db = a.fx_send_db;
        }
        if a.reverb != b.reverb {
            h.reverb = a.reverb;
        }
        if a.echo != b.echo {
            h.echo = a.echo;
        }
        if a.width != b.width {
            h.width = a.width;
        }
        if a.input_link != b.input_link {
            h.input_link = a.input_link;
        }
        if a.output_link != b.output_link {
            h.output_link = a.output_link.clone();
        }
        if a.input_pair_link != b.input_pair_link {
            h.input_pair_link = a.input_pair_link.clone();
        }

        if after.matrix_overrides != before.matrix_overrides {
            self.matrix_overrides = after.matrix_overrides.clone();
        }
    }

    /// Returns an error if this scene was captured on a different
    /// model than `device_model`. A blank `self.model` (legacy scene,
    /// or one built via `Scene::new`) is always treated as compatible.
    pub fn check_compatible(&self, device_model: &str) -> Result<(), Error> {
        if !self.model.is_empty() && self.model != device_model {
            return Err(Error::SceneModelMismatch {
                scene_model: self.model.clone(),
                device_model: device_model.to_string(),
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channel::ChannelType;

    #[test]
    fn merge_host_changes_ignores_device_owned_fields() {
        let mut host = Scene::new("auto");
        let mut input = InputChannel::new(0, "AN1", ChannelType::Mic, 1);
        input.volumes[0] = 0.5;
        input.gain = Some(10);
        let mut playback = PlaybackChannel::new(0, "PB1", 1);
        playback.volumes[0] = 0.5;
        let mut output = OutputChannel::new(0, "PH");
        output.volume = 0.5;
        host.inputs = vec![input];
        host.playbacks = vec![playback];
        host.outputs = vec![output];
        host.settings.sample_rate = 48_000;

        let before = host.clone();
        let mut after = host.clone();
        // Device-originated: front-panel gain and reference change.
        after.inputs[0].gain = Some(30);
        after.settings.line_in_ref = 3;
        // Host-originated mixer changes.
        after.inputs[0].volumes[0] = 0.8;
        after.outputs[0].volume = 0.7;
        after.playbacks[0].volumes[0] = 0.9;
        after.settings.dim = true;

        host.merge_host_changes(&before, &after);

        assert_eq!(host.inputs[0].volumes[0], 0.8);
        assert_eq!(host.outputs[0].volume, 0.7);
        assert_eq!(host.playbacks[0].volumes[0], 0.9);
        assert!(host.settings.dim);
        assert_eq!(host.inputs[0].gain, Some(10), "input gain is device-owned");
        assert_eq!(host.settings.line_in_ref, 0, "reference is device-owned");
    }

    #[test]
    fn merge_host_changes_does_not_copy_device_clock_or_rate() {
        let mut host = Scene::new("auto");
        let before = host.clone();
        let mut after = host.clone();
        after.settings.sample_rate = 44_100;
        after.settings.clock_source = "SPDIF Coaxial".into();
        after.settings.optical_out_spdif = true;
        host.merge_host_changes(&before, &after);
        assert_eq!(host.settings.sample_rate, 48_000);
        assert_eq!(host.settings.clock_source, "Internal");
        assert!(!host.settings.optical_out_spdif);
    }

    /// Point the shared auto file at a temp path so tests never touch
    /// the real `~/.local/share/tuxmix`. `scenes_dir()` is fixed, so
    /// instead we override the HOME env var for the duration.
    struct TempHome;
    impl TempHome {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("tuxmix-test-{}", std::process::id()));
            std::env::set_var("HOME", &dir);
            let _ = std::fs::remove_dir_all(&dir);
            Self
        }
    }
    impl Drop for TempHome {
        fn drop(&mut self) {
            // Restore a sane HOME for the rest of the test process.
            std::env::remove_var("HOME");
        }
    }

    #[test]
    fn auto_scene_other_write_detection() {
        let _home = TempHome::new();
        let ours = Scene::new("ours");
        let ours_json = ours.to_json().unwrap();
        let theirs = Scene::new("theirs");

        // No file yet: nothing written by "the other" (nothing to
        // clobber) — but a pre-existing file from a previous run IS
        // "other" until we've written once.
        assert!(auto_scene_written_by_other(None).is_none());

        // We write, then the other UI overwrites with a different scene:
        // our last_json no longer matches the disk → detected.
        save_auto_scene(&ours).unwrap();
        save_auto_scene(&theirs).unwrap();
        let detected = auto_scene_written_by_other(Some(&ours_json));
        assert!(detected.is_some());
        assert_eq!(detected.unwrap().name, "theirs");

        // Disk still holds OUR write → not "other".
        save_auto_scene(&ours).unwrap();
        assert!(auto_scene_written_by_other(Some(&ours_json)).is_none());

        // Our last_json unknown (UI restarted) but disk exists → other.
        save_auto_scene(&theirs).unwrap();
        assert!(auto_scene_written_by_other(None).is_some());
    }
}
