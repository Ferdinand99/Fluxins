// SPDX-License-Identifier: AGPL-3.0-or-later
//! The notification sound: Inlet's own (`assets/sounds`, CC0, made for Inlet), never
//! Fluxer's. It plays on a thread of its own through the system's default output
//! device: the UI thread only queues a request and never waits. The device is opened
//! for each sound and closed after it, so Inlet holds no audio device while it is
//! quiet. A sound that cannot play (no device, a busy one) is logged once and skipped.
//!
//! - Before each sound the thread asks `inlet_platform::quiet_mode` whether the system
//!   is in a quiet mode (do not disturb, presentation, full screen): then it stays
//!   silent, as the system holds back the (silent) notification itself.
//! - Windows and macOS play through cpal (WASAPI, CoreAudio).
//! - Linux loads ALSA's `libasound.so.2` at run time, only when a sound plays. Inlet
//!   does not link it, so a system without it starts normally and only has no sound.
//!   ALSA's `default` device reaches PulseAudio or PipeWire on a desktop.

// Tests never play a sound: the player is only started by the app.
#![cfg_attr(test, allow(dead_code))]

use std::sync::mpsc;
use std::time::Duration;

/// The sound for a message that notifies: 48 kHz, 16-bit mono WAV.
const MESSAGE_WAV: &[u8] = include_bytes!("../../../../../assets/sounds/message.wav");
/// Sounds waiting to play; more are dropped (they would only overlap).
const QUEUE: usize = 2;
/// Longest sound accepted, in samples (2 s at 48 kHz).
const MAX_SAMPLES: usize = 96_000;

/// Samples in -1..=1 at their own rate.
#[derive(Clone, Debug, PartialEq)]
pub struct Pcm {
    pub rate: u32,
    pub samples: Vec<f32>,
}

impl Pcm {
    #[cfg_attr(target_os = "linux", allow(dead_code))]
    pub fn duration(&self) -> Duration {
        Duration::from_secs_f64(self.samples.len() as f64 / f64::from(self.rate.max(1)))
    }
}

/// Reads a mono, 16-bit PCM WAV file: the only kind Inlet ships.
pub fn parse_wav(bytes: &[u8]) -> Option<Pcm> {
    if bytes.len() < 12 || &bytes[..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return None;
    }
    let mut at = 12;
    let mut format = None;
    while at + 8 <= bytes.len() {
        let id = &bytes[at..at + 4];
        let len = u32::from_le_bytes(bytes[at + 4..at + 8].try_into().ok()?) as usize;
        let body = bytes.get(at + 8..at + 8 + len)?;
        match id {
            b"fmt " if len >= 16 => {
                let word = |i: usize| u16::from_le_bytes([body[i], body[i + 1]]);
                let rate = u32::from_le_bytes(body[4..8].try_into().ok()?);
                // PCM, one channel, 16 bits.
                if word(0) != 1 || word(2) != 1 || word(14) != 16 || rate == 0 {
                    return None;
                }
                format = Some(rate);
            }
            b"data" => {
                let rate = format?;
                let samples: Vec<f32> = body
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .take(MAX_SAMPLES)
                    .map(|s| f32::from(i16::from_le_bytes(*s)) / 32768.0)
                    .collect();
                return Some(Pcm { rate, samples });
            }
            _ => {}
        }
        // Chunks are padded to an even length.
        at += 8 + len + (len & 1);
    }
    None
}

/// The sound Inlet plays for a message that notifies.
pub fn message_sound() -> Option<Pcm> {
    parse_wav(MESSAGE_WAV)
}

/// Plays the message sound on its own thread.
pub struct SoundPlayer {
    tx: mpsc::SyncSender<()>,
}

impl SoundPlayer {
    /// Starts the sound thread; `None` when the sound or the thread is not there.
    pub fn start() -> Option<Self> {
        let pcm = message_sound()?;
        let (tx, rx) = mpsc::sync_channel::<()>(QUEUE);
        std::thread::Builder::new()
            .name("inlet-sound".into())
            .spawn(move || {
                // The same failure is logged once, not for every sound.
                let mut last_error = None;
                for () in rx {
                    if inlet_platform::quiet_mode() {
                        continue;
                    }
                    match output::play(&pcm) {
                        Ok(()) => last_error = None,
                        Err(err) => {
                            if last_error.as_ref() != Some(&err) {
                                log::info!("the notification sound did not play: {err}");
                            }
                            last_error = Some(err);
                        }
                    }
                }
            })
            .map_err(|err| log::warn!("no sound thread: {err}"))
            .ok()?;
        Some(SoundPlayer { tx })
    }

    /// Queues the sound without waiting; dropped when two are already waiting.
    pub fn play(&self) {
        let _ = self.tx.try_send(());
    }
}

/// The sound at the device's rate: linear interpolation between the samples.
#[cfg_attr(target_os = "linux", allow(dead_code))]
pub fn resample(pcm: &Pcm, rate: u32) -> Vec<f32> {
    if rate == 0 || pcm.samples.is_empty() {
        return Vec::new();
    }
    let step = f64::from(pcm.rate) / f64::from(rate);
    let len = ((pcm.samples.len() as f64) / step).floor() as usize;
    (0..len)
        .map(|i| {
            let pos = i as f64 * step;
            let ix = pos.floor() as usize;
            let frac = (pos - ix as f64) as f32;
            let a = pcm.samples[ix.min(pcm.samples.len() - 1)];
            let b = pcm.samples[(ix + 1).min(pcm.samples.len() - 1)];
            a + (b - a) * frac
        })
        .collect()
}

#[cfg(all(not(test), any(windows, target_os = "macos")))]
mod output {
    use std::sync::Arc;
    use std::time::Duration;

    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    use cpal::{FromSample, SampleFormat, SizedSample, StreamConfig};

    use super::{resample, Pcm};

    /// Plays `pcm` once on the default output device and waits until it is done.
    pub fn play(pcm: &Pcm) -> Result<(), String> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or_else(|| "no output device".to_string())?;
        let supported = device.default_output_config().map_err(|e| e.to_string())?;
        let format = supported.sample_format();
        let config: StreamConfig = supported.into();
        let samples = Arc::new(resample(pcm, config.sample_rate));
        let stream = match format {
            SampleFormat::F32 => build::<f32>(&device, config, samples),
            SampleFormat::I16 => build::<i16>(&device, config, samples),
            SampleFormat::U16 => build::<u16>(&device, config, samples),
            SampleFormat::I32 => build::<i32>(&device, config, samples),
            SampleFormat::F64 => build::<f64>(&device, config, samples),
            other => return Err(format!("sample format {other} not supported")),
        }?;
        stream.play().map_err(|e| e.to_string())?;
        std::thread::sleep(pcm.duration() + Duration::from_millis(150));
        Ok(())
    }

    fn build<T>(
        device: &cpal::Device,
        config: StreamConfig,
        samples: Arc<Vec<f32>>,
    ) -> Result<cpal::Stream, String>
    where
        T: SizedSample + FromSample<f32>,
    {
        let channels = usize::from(config.channels.max(1));
        let mut at = 0usize;
        device
            .build_output_stream(
                config,
                move |out: &mut [T], _: &cpal::OutputCallbackInfo| {
                    for frame in out.chunks_mut(channels) {
                        let v = samples.get(at).copied().unwrap_or(0.0);
                        at = at.saturating_add(1);
                        for s in frame {
                            *s = T::from_sample(v);
                        }
                    }
                },
                |err| log::info!("sound output: {err}"),
                None,
            )
            .map_err(|e| e.to_string())
    }
}

/// ALSA, loaded at run time (see the module docs).
#[cfg(all(not(test), target_os = "linux"))]
mod output {
    use std::ffi::{c_char, c_int, c_long, c_uint, c_ulong, c_void};
    use std::sync::OnceLock;

    use libloading::{Library, Symbol};

    use super::Pcm;

    const STREAM_PLAYBACK: c_int = 0;
    /// `SND_PCM_NONBLOCK`: opening never waits for a device another program holds.
    const OPEN_NONBLOCK: c_int = 1;
    /// Writes in a row that move nothing before the sound is given up.
    const MAX_STALLS: u32 = 3;
    const FORMAT_S16_LE: c_int = 2;
    const ACCESS_RW_INTERLEAVED: c_int = 3;
    /// Latency asked of ALSA, in microseconds.
    const LATENCY_US: c_uint = 100_000;

    type Open = unsafe extern "C" fn(*mut *mut c_void, *const c_char, c_int, c_int) -> c_int;
    type SetParams =
        unsafe extern "C" fn(*mut c_void, c_int, c_int, c_uint, c_uint, c_int, c_uint) -> c_int;
    type WriteI = unsafe extern "C" fn(*mut c_void, *const c_void, c_ulong) -> c_long;
    type Recover = unsafe extern "C" fn(*mut c_void, c_int, c_int) -> c_int;
    type Handle = unsafe extern "C" fn(*mut c_void) -> c_int;
    type NonBlock = unsafe extern "C" fn(*mut c_void, c_int) -> c_int;

    /// The library, loaded on the first sound; a failure is kept and not retried.
    fn alsa() -> Result<&'static Library, String> {
        static LIB: OnceLock<Result<Library, String>> = OnceLock::new();
        LIB.get_or_init(|| {
            // SAFETY: the system's own ALSA library under its versioned name; loading it
            // runs only its initialisers.
            unsafe { Library::new("libasound.so.2") }
                .map_err(|e| format!("no ALSA library (libasound.so.2): {e}"))
        })
        .as_ref()
        .map_err(Clone::clone)
    }

    /// One function of the library; `name` without the trailing NUL.
    ///
    /// # Safety
    /// `T` must be the function's type as `alsa/pcm.h` declares it.
    unsafe fn sym<T>(lib: &'static Library, name: &str) -> Result<Symbol<'static, T>, String> {
        let c = format!("{name}\0");
        // SAFETY: the caller gives the declared type.
        unsafe { lib.get::<T>(c.as_bytes()) }.map_err(|e| format!("{name}: {e}"))
    }

    /// Plays `pcm` once on ALSA's default device and waits until it is done.
    pub fn play(pcm: &Pcm) -> Result<(), String> {
        let lib = alsa()?;
        let samples: Vec<i16> = pcm
            .samples
            .iter()
            .map(|s| (s.clamp(-1.0, 1.0) * 32767.0) as i16)
            .collect();
        // SAFETY: every symbol has the type `alsa/pcm.h` declares. The handle is used
        // only between a successful open and the close, and the buffer outlives every
        // write.
        unsafe {
            let open = sym::<Open>(lib, "snd_pcm_open")?;
            let set_params = sym::<SetParams>(lib, "snd_pcm_set_params")?;
            let writei = sym::<WriteI>(lib, "snd_pcm_writei")?;
            let recover = sym::<Recover>(lib, "snd_pcm_recover")?;
            let drain = sym::<Handle>(lib, "snd_pcm_drain")?;
            let close = sym::<Handle>(lib, "snd_pcm_close")?;
            let nonblock = sym::<NonBlock>(lib, "snd_pcm_nonblock")?;

            let mut handle: *mut c_void = std::ptr::null_mut();
            // Non-blocking open: a busy device fails at once instead of holding the
            // sound thread.
            let rc = open(
                &mut handle,
                c"default".as_ptr(),
                STREAM_PLAYBACK,
                OPEN_NONBLOCK,
            );
            if rc < 0 || handle.is_null() {
                return Err(format!("no ALSA output device ({rc})"));
            }
            let rc = set_params(
                handle,
                FORMAT_S16_LE,
                ACCESS_RW_INTERLEAVED,
                1,
                pcm.rate,
                1,
                LATENCY_US,
            );
            if rc < 0 {
                close(handle);
                return Err(format!("ALSA refused the format ({rc})"));
            }
            // Writing blocks again (for as long as the sound plays), so `drain` waits
            // for the end of the sound.
            nonblock(handle, 0);
            let mut at = 0usize;
            let mut stalls = 0;
            while at < samples.len() && stalls < MAX_STALLS {
                let left = &samples[at..];
                let n = writei(handle, left.as_ptr().cast(), left.len() as c_ulong);
                if n > 0 {
                    at += n as usize;
                    stalls = 0;
                    continue;
                }
                stalls += 1;
                if n < 0 && recover(handle, n as c_int, 1) < 0 {
                    break;
                }
            }
            drain(handle);
            close(handle);
        }
        Ok(())
    }
}

/// Tests never open an audio device; other systems have no sound.
#[cfg(any(test, not(any(windows, target_os = "macos", target_os = "linux"))))]
mod output {
    use super::Pcm;

    pub fn play(_: &Pcm) -> Result<(), String> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_message_sound_is_a_short_mono_wav() {
        let pcm = message_sound().expect("the shipped sound parses");
        assert_eq!(pcm.rate, 48_000);
        let secs = pcm.duration().as_secs_f32();
        assert!((0.2..1.0).contains(&secs), "{secs} s");
        // Not clipped, not silent, and it ends in silence (no click).
        let peak = pcm.samples.iter().fold(0f32, |m, s| m.max(s.abs()));
        assert!(peak > 0.1 && peak < 0.9, "peak {peak}");
        assert!(pcm.samples.last().unwrap().abs() < 0.01);
    }

    #[test]
    fn other_wav_files_are_refused() {
        assert_eq!(parse_wav(b"RIFF\0\0\0\0WAVE"), None);
        assert_eq!(parse_wav(b"not a wav file"), None);
        let mut stereo = MESSAGE_WAV.to_vec();
        stereo[22] = 2; // channels
        assert_eq!(parse_wav(&stereo), None);
        // A chunk that claims more bytes than there are.
        assert_eq!(parse_wav(&MESSAGE_WAV[..60]), None);
    }

    #[test]
    fn resampling_keeps_the_length_in_time() {
        let pcm = Pcm {
            rate: 48_000,
            samples: vec![0.5; 4800],
        };
        assert_eq!(resample(&pcm, 44_100).len(), 4410);
        assert_eq!(resample(&pcm, 96_000).len(), 9600);
        assert!(resample(&pcm, 0).is_empty());
        assert!(resample(&pcm, 44_100)
            .iter()
            .all(|s| (*s - 0.5).abs() < 1e-6));
    }
}
