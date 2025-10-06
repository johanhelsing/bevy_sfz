//! # SFZ Asset Loader
//!
//! Bevy asset loader for SFZ instrument files. This loader parses SFZ files
//! and automatically loads all referenced audio samples, creating a complete
//! [`SfzInstrumentAsset`] asset ready for use in an audio engine.

use bevy::{
    asset::{AssetLoader, AsyncReadExt, LoadContext, io::Reader},
    prelude::*,
};
use sofiza::{Instrument as SofizaInstrument, Opcode, Region};
use std::collections::HashMap;
use std::path::PathBuf;
use thiserror::Error;

/// Represents a fully loaded SFZ instrument with all sample dependencies
#[derive(Asset, TypePath, Debug)]
pub struct SfzInstrumentAsset {
    /// The parsed SFZ instrument data
    pub instrument: SofizaInstrument,
    /// Map of relative sample paths to loaded audio handles
    pub samples: HashMap<PathBuf, Handle<AudioSource>>,
    /// Instrument metadata
    pub name: String,
}

impl SfzInstrumentAsset {
    /// Get all regions with their indices that should trigger for a given MIDI note and velocity
    pub fn regions_with_indices_for_note(
        &self,
        midi_note: u8,
        velocity: u8,
    ) -> Vec<(usize, &Region)> {
        self.instrument
            .regions
            .iter()
            .enumerate()
            .filter(|(_, region)| self.region_matches_note(region, midi_note, velocity))
            .collect()
    }

    /// Check if a region should trigger for the given note and velocity
    fn region_matches_note(&self, region: &Region, midi_note: u8, velocity: u8) -> bool {
        // Get the effective opcodes for this region (with group and global fallbacks)
        let region_opcodes = &region.opcodes;

        // Check if region belongs to a group and get group opcodes
        let group_opcodes = if let Some(group_idx) = region.group() {
            if group_idx < self.instrument.groups.len() {
                Some(&self.instrument.groups[group_idx].opcodes)
            } else {
                None
            }
        } else {
            None
        };

        // Helper function to get opcode value from region, then group, then default
        let get_opcode_value = |opcode_name: &str, default: u8| -> u8 {
            // Check region first
            if let Some(opcode) = region_opcodes.get(opcode_name) {
                match (opcode_name, opcode) {
                    ("lokey", Opcode::lokey(val)) => return *val,
                    ("hikey", Opcode::hikey(val)) => return *val,
                    ("lovel", Opcode::lovel(val)) => return *val,
                    ("hivel", Opcode::hivel(val)) => return *val,
                    _ => {}
                }
            }
            // Check group second
            if let Some(group_ops) = group_opcodes
                && let Some(opcode) = group_ops.get(opcode_name)
            {
                match (opcode_name, opcode) {
                    ("lokey", Opcode::lokey(val)) => return *val,
                    ("hikey", Opcode::hikey(val)) => return *val,
                    ("lovel", Opcode::lovel(val)) => return *val,
                    ("hivel", Opcode::hivel(val)) => return *val,
                    _ => {}
                }
            }
            default
        };

        // Check key range
        let lokey = get_opcode_value("lokey", 0);
        let hikey = get_opcode_value("hikey", 127);

        if midi_note < lokey || midi_note > hikey {
            return false;
        }

        // Check velocity range
        let lovel = get_opcode_value("lovel", 0);
        let hivel = get_opcode_value("hivel", 127);

        if velocity < lovel || velocity > hivel {
            return false;
        }

        true
    }

    /// Get the sample handle for a region
    pub fn sample_for_region(&self, region: &Region) -> Option<&Handle<AudioSource>> {
        // Get the sample path from the region
        let sample_path = region.opcodes.get("sample").and_then(|op| {
            if let Opcode::sample(path) = op {
                Some(path)
            } else {
                None
            }
        })?;

        self.samples.get(sample_path)
    }

    /// Get the root key (pitch center) for a region
    pub fn pitch_keycenter_for_region(&self, region: &Region) -> u8 {
        region
            .opcodes
            .get("pitch_keycenter")
            .and_then(|op| {
                if let Opcode::pitch_keycenter(val) = op {
                    Some(*val)
                } else {
                    None
                }
            })
            .unwrap_or(60) // Default to C4
    }

    /// Get the volume for a region in dB
    pub fn volume_for_region(&self, region: &Region) -> f32 {
        region
            .opcodes
            .get("volume")
            .and_then(|op| {
                if let Opcode::volume(val) = op {
                    Some(*val)
                } else {
                    None
                }
            })
            .unwrap_or(0.0) // Default to 0 dB
    }

    /// Get the pan for a region (-100 to 100)
    pub fn pan_for_region(&self, region: &Region) -> f32 {
        region
            .opcodes
            .get("pan")
            .and_then(|op| {
                if let Opcode::pan(val) = op {
                    Some(*val)
                } else {
                    None
                }
            })
            .unwrap_or(0.0) // Default to center
    }

    /// Get the tune offset for a region in cents
    pub fn tune_for_region(&self, region: &Region) -> i8 {
        region
            .opcodes
            .get("tune")
            .and_then(|op| {
                if let Opcode::tune(val) = op {
                    Some(*val)
                } else {
                    None
                }
            })
            .unwrap_or(0) // Default to no tuning offset
    }

    /// Get the amplitude envelope release time for a region in seconds
    pub fn ampeg_release_for_region(&self, region: &Region) -> f32 {
        // Check region-specific release first, then global
        if let Some(release) = region.opcodes.get("ampeg_release").and_then(|op| {
            if let Opcode::ampeg_release(val) = op {
                Some(*val)
            } else {
                None
            }
        }) {
            return release;
        }

        // If no region-specific release, check global release from the instrument
        // Look through all regions to find any global ampeg_release values
        for region in &self.instrument.regions {
            if let Some(release) = region.opcodes.get("ampeg_release").and_then(|op| {
                if let Opcode::ampeg_release(val) = op {
                    Some(*val)
                } else {
                    None
                }
            }) {
                return release;
            }
        }

        // Default release time if not specified in SFZ
        0.3 // 300ms - reasonable default for sustained instruments
    }
}

/// Custom asset loader for SFZ files
#[derive(Default)]
pub struct SfzAssetLoader;

/// Errors that can occur during SFZ loading
#[derive(Debug, Error)]
pub enum SfzLoaderError {
    #[error("Failed to read SFZ file: {0}")]
    Io(#[from] std::io::Error),
    #[error("Failed to parse SFZ: {0}")]
    Parse(String),
    #[error("Sample file not found: {0}")]
    #[allow(dead_code)] // May be used for enhanced error reporting in the future
    SampleNotFound(String),
}

impl AssetLoader for SfzAssetLoader {
    type Asset = SfzInstrumentAsset;
    type Settings = ();
    type Error = SfzLoaderError;

    async fn load(
        &self,
        reader: &mut dyn Reader,
        _settings: &Self::Settings,
        load_context: &mut LoadContext<'_>,
    ) -> Result<Self::Asset, Self::Error> {
        // Read the SFZ file content
        let mut contents = String::new();
        reader.read_to_string(&mut contents).await?;

        // Extract paths before we start borrowing load_context mutably
        let sfz_path_buf = load_context.path().to_path_buf();
        let sfz_dir = sfz_path_buf
            .parent()
            .unwrap_or(std::path::Path::new(""))
            .to_path_buf();
        let file_name = sfz_path_buf
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("Unknown SFZ Instrument")
            .to_string();

        // Parse the SFZ file using sofiza with panic protection
        // Pass the SFZ file's directory as the base path for sofiza
        let instrument = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            SofizaInstrument::from_sfz(&contents, &sfz_dir)
        }))
        .map_err(|_| SfzLoaderError::Parse("Sofiza library panicked during parsing".to_string()))?
        .map_err(|e| {
            // More detailed error handling for common issues
            let error_msg = format!("{:?}", e);
            if error_msg.contains("No such file") || error_msg.contains("cannot find") {
                SfzLoaderError::SampleNotFound(format!("Sample file not found: {}", error_msg))
            } else {
                SfzLoaderError::Parse(format!("Failed to parse SFZ: {}", error_msg))
            }
        })?;

        // Log what we parsed for debugging
        info!(
            "SFZ Loaded '{}': {} regions",
            file_name,
            instrument.regions.len()
        );
        if !instrument.regions.is_empty() {
            let sample_region = &instrument.regions[0];
            info!(
                "SFZ First region sample: {:?}, opcodes: {}",
                sample_region.opcodes.get("sample"),
                sample_region.opcodes.len()
            );

            // Log some key opcodes from first region
            for (key, opcode) in sample_region.opcodes.iter().take(5) {
                info!("SFZ First region opcode: {} = {:?}", key, opcode);
            }
        }

        // Collect all unique sample paths from regions
        let mut sample_paths = std::collections::HashSet::new();
        for region in &instrument.regions {
            if let Some(Opcode::sample(path)) = region.opcodes.get("sample") {
                sample_paths.insert(path.clone());
            }
        }

        // Convert to Vec to avoid borrow checker issues
        let sample_paths_vec: Vec<PathBuf> = sample_paths.into_iter().collect();

        // Load all samples and build the handle map
        let mut samples = HashMap::new();
        for sample_path in sample_paths_vec {
            // Combine default_path with sample path for relative paths
            let final_path = if sample_path.is_relative() {
                instrument.default_path.join(&sample_path)
            } else {
                sample_path.clone()
            };

            // For WASM builds, URL-encode the file path to handle special characters like #
            #[cfg(target_arch = "wasm32")]
            let encoded_path = {
                use percent_encoding::{AsciiSet, CONTROLS, utf8_percent_encode};

                // Characters that need to be percent-encoded for URL safety in WASM
                // Focus on characters that cause URL parsing issues, especially # which becomes a fragment
                const FRAGMENT_ENCODE_SET: &AsciiSet = &CONTROLS.add(b'#');

                // Convert path to string and URL-encode it
                let path_str = final_path.to_string_lossy();
                let encoded_str = utf8_percent_encode(&path_str, FRAGMENT_ENCODE_SET).to_string();
                PathBuf::from(encoded_str)
            };

            #[cfg(not(target_arch = "wasm32"))]
            let encoded_path = final_path.clone();

            // Load the sample as a dependency using the encoded path
            let sample_handle: Handle<AudioSource> = load_context.load(encoded_path);
            samples.insert(sample_path, sample_handle);
        } // Extract instrument name from filename or global opcodes
        let name = file_name;

        Ok(SfzInstrumentAsset {
            instrument,
            samples,
            name,
        })
    }

    fn extensions(&self) -> &[&str] {
        &["sfz"]
    }
}

/// Plugin to register the SFZ asset loader
pub struct SfzLoaderPlugin;

impl Plugin for SfzLoaderPlugin {
    fn build(&self, app: &mut App) {
        app.init_asset::<SfzInstrumentAsset>()
            .register_asset_loader(SfzAssetLoader);
    }
}
