//! # SFZ Instrument Component
//!
//! Implements a multi-sample instrument component that plays samples based on
//! SFZ instrument definitions. Supports velocity layers, key ranges, round-robin
//! sampling, sustain/release functionality, and other advanced sampling features.

use bevy::{audio::Volume, prelude::*, time::common_conditions::on_timer};
use bevy_sfz::{SfzInstrumentAsset, SfzLoaderPlugin};
use std::{collections::HashMap, time::Duration};

/// Event triggered on an instrument entity when it should play a note
#[derive(EntityEvent, Debug, Clone)]
pub struct PlayNote {
    pub entity: Entity,
    pub midi_note: u8,
    /// Volume/loudness of the note (0.0 = silent, 1.0 = maximum volume).
    /// In MIDI terminology, this is called "velocity" - how hard the key is pressed.
    pub velocity: f32,
}

/// Trigger sent when a note should be released
#[derive(EntityEvent, Debug, Clone)]
pub struct ReleaseNote {
    pub entity: Entity,
    pub midi_note: u8,
}

#[derive(Component)]
pub struct Instrument {
    pub volume: f32,
    pub muted: bool,
}

/// Convert MIDI note number (0-127) to frequency in Hz.
///
/// Uses the standard 12-tone equal temperament tuning system where each semitone
/// is a factor of 2^(1/12) ≈ 1.059463 higher than the previous.
///
/// # Arguments
/// * `midi_note` - MIDI note number where 60 = Middle C (C4) = 261.63 Hz
///
/// # Returns
/// The frequency in Hz corresponding to the MIDI note
///
/// # Examples
/// ```
/// let middle_c_freq = pitch_for_midi_note(60); // Returns ~261.63 Hz
/// let a4_freq = pitch_for_midi_note(69);       // Returns 440.0 Hz
/// ```
pub fn pitch_for_midi_note(midi_note: u8) -> f32 {
    // MIDI note 60 is Middle C (C4)
    // Each semitone is a factor of 2^(1/12) ≈ 1.059463
    const SEMITONE_RATIO: f32 = 1.059463;
    const MIDDLE_C_FREQ: f32 = 261.63; // C4 frequency
    const MIDDLE_C_MIDI: i16 = 60;

    let semitones_from_middle_c = midi_note as i16 - MIDDLE_C_MIDI;
    MIDDLE_C_FREQ * SEMITONE_RATIO.powi(semitones_from_middle_c as i32)
}

/// Component that implements an SFZ-based multi-sample instrument
#[derive(Component)]
pub struct SfzInstrument {
    /// Handle to the loaded SFZ instrument asset
    pub sfz_handle: Handle<SfzInstrumentAsset>,
    /// Round-robin state for regions that support it
    round_robin_state: HashMap<RegionId, RoundRobinState>,
    /// Currently playing notes: MIDI note -> Vec of (audio entity, region index)
    /// We store region index to know which region's release time to use
    pub active_notes: HashMap<u8, Vec<(Entity, usize)>>,
}

/// Unique identifier for a region within an SFZ instrument
type RegionId = usize;

/// State tracking for round-robin sampling
#[derive(Debug)]
struct RoundRobinState {
    /// Sequence of samples in the round-robin group
    samples: Vec<usize>,
    /// Current position in the sequence
    current_index: usize,
    /// When this group was last triggered
    last_triggered: std::time::Instant,
}

impl RoundRobinState {
    fn new(samples: Vec<usize>) -> Self {
        Self {
            samples,
            current_index: 0,
            last_triggered: std::time::Instant::now(),
        }
    }

    fn next_sample(&mut self) -> usize {
        let sample = self.samples[self.current_index];
        self.current_index = (self.current_index + 1) % self.samples.len();
        self.last_triggered = std::time::Instant::now();
        sample
    }
}

/// Component to mark audio entities that should fade out
#[derive(Component)]
pub struct SfzFadeOut {
    pub duration: Duration,
    pub start_time: Duration,
    pub start_volume: f32,
}

impl SfzInstrument {
    /// Create a new SFZ instrument component
    pub fn new(sfz_handle: Handle<SfzInstrumentAsset>) -> Self {
        Self {
            sfz_handle,
            round_robin_state: HashMap::new(),
            active_notes: HashMap::new(),
        }
    }

    /// Initialize round-robin groups for the instrument
    fn initialize_round_robin(&mut self, sfz_instrument: &SfzInstrumentAsset) {
        // Group regions by their round-robin configuration
        let mut round_robin_groups: HashMap<u32, Vec<usize>> = HashMap::new();

        for (region_idx, region) in sfz_instrument.instrument.regions.iter().enumerate() {
            // Check if region has round-robin configuration
            if let Some(_seq_length) = region.opcodes.get("seq_length").and_then(|op| {
                if let sofiza::Opcode::seq_length(val) = op {
                    Some(val)
                } else {
                    None
                }
            }) && let Some(_seq_position) = region.opcodes.get("seq_position").and_then(|op| {
                if let sofiza::Opcode::seq_position(val) = op {
                    Some(val)
                } else {
                    None
                }
            }) {
                // Create a unique group ID based on key range and other properties
                let group_id = self.calculate_round_robin_group_id(region);
                round_robin_groups
                    .entry(group_id)
                    .or_default()
                    .push(region_idx);
            }
        }

        // Initialize round-robin state for each group
        for (_group_id, region_indices) in round_robin_groups {
            if region_indices.len() > 1 {
                // Sort by sequence position to ensure correct order
                let mut sorted_indices = region_indices;
                sorted_indices.sort_by_key(|&idx| {
                    sfz_instrument.instrument.regions[idx]
                        .opcodes
                        .get("seq_position")
                        .and_then(|op| {
                            if let sofiza::Opcode::seq_position(val) = op {
                                Some(val)
                            } else {
                                None
                            }
                        })
                        .copied()
                        .unwrap_or(1)
                });

                for &region_idx in &sorted_indices {
                    self.round_robin_state
                        .insert(region_idx, RoundRobinState::new(sorted_indices.clone()));
                }
            }
        }
    }

    /// Calculate a unique ID for grouping round-robin regions
    fn calculate_round_robin_group_id(&self, region: &sofiza::Region) -> u32 {
        let mut id = 0u32;

        // Hash key range
        if let Some(sofiza::Opcode::lokey(val)) = region.opcodes.get("lokey") {
            id = id.wrapping_mul(31).wrapping_add(*val as u32);
        }
        if let Some(sofiza::Opcode::hikey(val)) = region.opcodes.get("hikey") {
            id = id.wrapping_mul(31).wrapping_add(*val as u32);
        }

        // Hash velocity range
        if let Some(sofiza::Opcode::lovel(val)) = region.opcodes.get("lovel") {
            id = id.wrapping_mul(31).wrapping_add(*val as u32);
        }
        if let Some(sofiza::Opcode::hivel(val)) = region.opcodes.get("hivel") {
            id = id.wrapping_mul(31).wrapping_add(*val as u32);
        }

        // Hash group number if present
        if let Some(sofiza::Opcode::group(val)) = region.opcodes.get("group") {
            id = id.wrapping_mul(31).wrapping_add(*val);
        }

        id
    }
}

/// Plugin that provides SFZ instrument functionality
pub struct SfzInstrumentPlugin;

impl Plugin for SfzInstrumentPlugin {
    fn build(&self, app: &mut App) {
        app.add_observer(play_sfz_sound)
            .add_observer(release_sfz_sound)
            .add_systems(Update, (on_sfz_asset_loaded, fade_out_sfz_audio));
    }
}

/// System to initialize SFZ instruments when their assets are loaded
fn on_sfz_asset_loaded(
    mut asset_events: MessageReader<AssetEvent<SfzInstrumentAsset>>,
    mut sfz_query: Query<&mut SfzInstrument>,
    sfz_assets: Res<Assets<SfzInstrumentAsset>>,
) {
    for event in asset_events.read() {
        if let AssetEvent::LoadedWithDependencies { id } = event {
            // Find all SfzInstrument components that use this asset
            for mut sfz_component in sfz_query.iter_mut() {
                if sfz_component.sfz_handle.id() == *id
                    && let Some(sfz_instrument) = sfz_assets.get(&sfz_component.sfz_handle)
                {
                    sfz_component.initialize_round_robin(sfz_instrument);
                }
            }
        }
    }
}

/// Observer that handles [`PlayNote`] events for SFZ instruments
fn play_sfz_sound(
    play_note: On<PlayNote>,
    mut commands: Commands,
    mut sfz_query: Query<(&mut SfzInstrument, &Instrument)>,
    sfz_assets: Res<Assets<SfzInstrumentAsset>>,
) {
    let play_note = play_note.event();
    let entity = play_note.entity;

    if let Ok((mut sfz_component, instrument)) = sfz_query.get_mut(entity) {
        // Check if the instrument is muted
        if instrument.muted {
            return;
        }

        // Get the SFZ instrument data
        let sfz_instrument = match sfz_assets.get(&sfz_component.sfz_handle) {
            Some(sfz) => sfz,
            None => return, // Asset not loaded yet
        };

        // Find all regions that should trigger for this note
        let matching_regions = sfz_instrument
            .regions_with_indices_for_note(play_note.midi_note, (play_note.velocity * 127.0) as u8);

        info!(
            "SFZ Note {}: velocity {:.2} -> MIDI vel {}, found {} matching regions",
            play_note.midi_note,
            play_note.velocity,
            (play_note.velocity * 127.0) as u8,
            matching_regions.len()
        );

        for (region_idx, region) in matching_regions.iter() {
            // Get the sample for this region
            let sample_handle = match sfz_instrument.sample_for_region(region) {
                Some(handle) => handle.clone(),
                None => {
                    warn!("No sample found for SFZ region");
                    continue;
                }
            };

            // Log which sample is being used for debugging
            if let Some(sofiza::Opcode::sample(path)) = region.opcodes.get("sample") {
                info!("SFZ Using sample: {:?}", path);
            }

            // Calculate pitch adjustment
            let root_key = sfz_instrument.pitch_keycenter_for_region(region);
            let target_pitch = pitch_for_midi_note(play_note.midi_note);
            let root_pitch = pitch_for_midi_note(root_key);
            let tune_cents = sfz_instrument.tune_for_region(region) as f32;
            let tune_ratio = 2.0_f32.powf(tune_cents / 1200.0); // Convert cents to ratio
            let speed = (target_pitch / root_pitch) * tune_ratio;

            // Calculate base volume (combine instrument volume, region volume, and note velocity)
            let region_volume_db = sfz_instrument.volume_for_region(region);
            let region_volume_linear = 10.0_f32.powf(region_volume_db / 20.0); // Convert dB to linear
            let base_volume = instrument.volume * region_volume_linear * play_note.velocity;

            // Get pan value
            let pan = sfz_instrument.pan_for_region(region) / 100.0; // Convert to -1.0 to 1.0 range

            info!(
                "SFZ Volume calculation: region_vol_db={:.1}, region_vol_linear={:.3}, instrument_vol={:.2}, note_vel={:.2}, base_vol={:.3}",
                region_volume_db,
                region_volume_linear,
                instrument.volume,
                play_note.velocity,
                base_volume
            );
            info!(
                "SFZ Pitch: root_key={}, target_pitch={:.1}Hz, root_pitch={:.1}Hz, tune_cents={}, speed={:.3}",
                root_key, target_pitch, root_pitch, tune_cents, speed
            );

            // Handle round-robin sampling
            let should_play =
                if let Some(round_robin) = sfz_component.round_robin_state.get_mut(region_idx) {
                    // For round-robin regions, only play if it's the next in sequence
                    let next_sample_idx = round_robin.next_sample();
                    next_sample_idx == *region_idx
                } else {
                    // For non-round-robin regions, always play
                    true
                };

            if should_play {
                // Spawn the audio entity
                let audio_entity = commands
                    .spawn((
                        Name::new(format!(
                            "SFZ sound: {} (note {})",
                            sfz_instrument.name, play_note.midi_note
                        )),
                        AudioPlayer::new(sample_handle),
                        PlaybackSettings {
                            speed,
                            ..PlaybackSettings::REMOVE.with_volume(Volume::Linear(base_volume))
                        },
                    ))
                    .id();

                // Track this note for release handling (store entity and region index)
                sfz_component
                    .active_notes
                    .entry(play_note.midi_note)
                    .or_insert_with(Vec::new)
                    .push((audio_entity, *region_idx));

                info!(
                    "Playing SFZ note {} on {}, velocity: {:.2}",
                    play_note.midi_note, sfz_instrument.name, play_note.velocity
                );

                // Add spatial audio if pan is not center
                if pan.abs() > 0.01 {
                    // Convert pan to spatial position
                    // This is a simple stereo pan - for true spatial audio, you'd use Transform
                    let left_gain = if pan <= 0.0 { 1.0 } else { 1.0 - pan };
                    let right_gain = if pan >= 0.0 { 1.0 } else { 1.0 + pan };

                    // Note: Bevy's current audio system doesn't have built-in pan control
                    // This would need to be implemented with custom audio processing
                    // For now, we'll just log the pan value
                    debug!(
                        "SFZ region pan: {} (L:{:.2}, R:{:.2})",
                        pan, left_gain, right_gain
                    );
                }
            }
        }
    }
}

/// Observer that handles [`ReleaseNote`] events for SFZ instruments
fn release_sfz_sound(
    release_note: On<ReleaseNote>,
    mut sfz_query: Query<&mut SfzInstrument>,
    mut commands: Commands,
    audio_query: Query<&AudioSink>,
    sfz_assets: Res<Assets<SfzInstrumentAsset>>,
) {
    let entity = release_note.entity;

    if let Ok(mut sfz_component) = sfz_query.get_mut(entity) {
        // Get the SFZ instrument data to access region information
        let sfz_instrument = match sfz_assets.get(&sfz_component.sfz_handle) {
            Some(sfz) => sfz,
            None => return, // Asset not loaded yet
        };

        // Release all audio entities for this MIDI note
        if let Some(audio_entities_with_regions) =
            sfz_component.active_notes.remove(&release_note.midi_note)
        {
            for (audio_entity, region_idx) in audio_entities_with_regions {
                // Get the region to determine its release time
                if let Some(region) = sfz_instrument.instrument.regions.get(region_idx) {
                    let release_time = sfz_instrument.ampeg_release_for_region(region);

                    // Start fade-out for this note using region-specific release time
                    if let Ok(sink) = audio_query.get(audio_entity) {
                        commands.entity(audio_entity).insert(SfzFadeOut {
                            duration: Duration::from_secs_f32(release_time),
                            start_time: Duration::from_secs(0),
                            start_volume: sink.volume().to_linear(),
                        });
                    }
                }
            }
            info!(
                "Released SFZ note {} with region-specific release times",
                release_note.midi_note
            );
        }
    }
}

/// System that handles audio fade-out for SFZ instruments
fn fade_out_sfz_audio(
    mut commands: Commands,
    mut fade_query: Query<(Entity, &mut SfzFadeOut, &mut AudioSink)>,
    time: Res<Time>,
) {
    for (entity, mut fade, mut sink) in fade_query.iter_mut() {
        // Initialize start time if not set
        if fade.start_time == Duration::from_secs(0) {
            fade.start_time = time.elapsed();
        }

        let elapsed = time.elapsed() - fade.start_time;
        let progress = elapsed.as_secs_f32() / fade.duration.as_secs_f32();

        if progress >= 1.0 {
            // Fade complete, remove the audio entity
            commands.entity(entity).despawn();
        } else {
            // Update volume based on fade progress
            let current_volume = fade.start_volume * (1.0 - progress);
            sink.set_volume(Volume::Linear(current_volume));
        }
    }
}

fn main() {
    App::new()
        .add_plugins((DefaultPlugins, SfzLoaderPlugin, SfzInstrumentPlugin))
        .add_systems(Startup, setup)
        .add_systems(
            Update,
            play_random_notes.run_if(on_timer(Duration::from_secs(2))),
        )
        .run();
}

fn setup(mut commands: Commands, asset_server: Res<AssetServer>) {
    // Load the SFZ instrument
    let sfz_handle: Handle<SfzInstrumentAsset> =
        asset_server.load("examples/sfz/piano_velocity_layers.sfz");

    // Create an entity with the SfzInstrument component
    commands.spawn((
        SfzInstrument::new(sfz_handle),
        Instrument {
            volume: 0.5,
            muted: false,
        },
    ));
}

fn play_random_notes(
    mut commands: Commands,
    instrument: Single<Entity, With<SfzInstrument>>,
    current_note: Local<Option<u8>>,
) {
    let instrument_entity = *instrument;

    // release previous note if any
    if let Some(note) = *current_note {
        info!("Releasing note {}", note);
        commands.trigger(ReleaseNote {
            entity: instrument_entity,
            midi_note: note,
        });
    }

    let midi_note = fastrand::u8(40..100);
    let velocity = fastrand::f32() * 0.5 + 0.5; // Random velocity between 0.5 and 1.0

    info!("Playing note {} with velocity {:.2}", midi_note, velocity);
    commands.trigger(PlayNote {
        entity: instrument_entity,
        midi_note,
        velocity,
    });
}
