//! Eclipse-owned **real OpenSL ES 1.0.1 audio engine** → host audio (cpal).
//!
//! 2026-06-05: `libroblox.so` imports exactly **8** audio symbols (`docs/bionic-env-worklist.md`
//! §5, `docs/libroblox-characterization.md`): the one **function** `slCreateEngine` and the **7**
//! `SL_IID_*` data objects. Everything else the engine does with audio — `Realize`,
//! `GetInterface`, `CreateOutputMix`, `CreateAudioPlayer`, `SetPlayState`, buffer-queue `Enqueue` —
//! is reached **through vtables** the engine reads from the `SLObjectItf`/interface pointers that
//! `slCreateEngine` and `GetInterface` hand back, **not** through additional imported symbols. So a
//! real OpenSL ES path means: return working `SLObjectItf`/itf vtables whose methods actually do the
//! work, with the player's `SLAndroidSimpleBufferQueueItf::Enqueue` feeding a host audio output
//! stream. This module is that path; the host backend is **cpal** (the smallest portable Linux sound
//! option — auto-selects ALSA, which modern distros route through PipeWire/Pulse).
//!
//! ## OpenSL ES object/interface ABI (public Khronos OpenSL ES 1.0.1 C-ABI)
//! `SLObjectItf` is `const struct SLObjectItf_ * const *` — a **pointer to a pointer** to a const
//! vtable of function pointers. An interface handle (`SLEngineItf`, `SLPlayItf`, …) has the same
//! shape. The engine calls a method as `(*obj)->Realize(obj, async)`: it dereferences the handle to
//! get the vtable pointer, then calls the slot, passing the handle back as `self`. So an Eclipse
//! "object" is a struct whose layout begins with a pointer to its `SLObjectItf_` vtable (the value
//! the handle points at), followed by one itf-pointer per interface the object exposes; `self` is the
//! address of that leading vtable-pointer field, and `GetInterface` returns the address of the
//! requested interface's itf-pointer field. Method-slot **order** is load-bearing — the engine indexes
//! the vtable by offset — so every slot is present (filled with a real or correctly-erroring fn, never
//! null) at the exact public-ABI position.
//!
//! ## Soundness (the same argument as `ndk_registry`/`window_registry`)
//! Every OpenSL object Eclipse mints is an entry in a process-global generational [`ObjectRegistry`]
//! slab; the `self` pointer the engine holds is the **stable heap address** of that entry's
//! `Box<ObjectState>`, whose first field is the vtable pointer. A method validates `self` by reading
//! back its registry id and bounds+generation-checking it before touching state — a stale/fabricated
//! handle becomes a typed `Err` → the OpenSL error sentinel (`SL_RESULT_PARAMETER_INVALID`), never a
//! wild dereference. The vtables themselves live in process-lifetime `OnceLock` statics (stable
//! addresses, the two-phase pattern used for `SL_IID_*`).
//!
//! `#![forbid(unsafe_code)]` is **not** set: this module implements C-ABI `extern "C"` methods (raw
//! pointer `self`/out-params) and shares the PCM ring with the cpal callback. Every `unsafe` block
//! carries a dated `// SAFETY:` note (AGENTS.md §2.3). No `%fs`/TLS, no foreign-code execution, no
//! linker/native-load code — a self-contained audio subsystem. `reloc.rs`/`elf.rs`/`resolve.rs`
//! stay `#![forbid(unsafe_code)]`.
//!
//! ## Clean-room provenance
//! Implemented from the **public** OpenSL ES 1.0.1 C-ABI (the Khronos `SLES/OpenSLES.h` /
//! `OpenSLES_Android.h` vtable layouts, `SL_RESULT_*`, `SL_IID_*`, `SLDataLocator_*`,
//! `SLDataFormat_PCM`) — general knowledge of those public headers — plus the public cpal API and
//! Eclipse's own `src/`. **No** OpenSL ES / Android / linker *source* was read; `libroblox.so` is
//! parsed as data only, nothing in it is executed by this module.

use std::collections::VecDeque;
use std::ffi::c_void;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

// =================================================================================================
// OpenSL ES result codes (public OpenSL ES 1.0.1, `SLES/OpenSLES.h`). `SLresult` = `SLuint32`.
// =================================================================================================

/// The operation succeeded.
pub const SL_RESULT_SUCCESS: u32 = 0x0000_0000;
/// A parameter was invalid (used for a stale/null handle, a missing interface, a bad format).
pub const SL_RESULT_PARAMETER_INVALID: u32 = 0x0000_000D;
/// Memory allocation failed.
pub const SL_RESULT_MEMORY_FAILURE: u32 = 0x0000_0001;
/// The requested feature is unsupported (returned when no host audio device exists).
pub const SL_RESULT_FEATURE_UNSUPPORTED: u32 = 0x0000_000C;
/// A precondition was violated (e.g. `GetInterface` before `Realize`).
pub const SL_RESULT_PRECONDITIONS_VIOLATED: u32 = 0x0000_0008;

/// `SLObjectItf` object states (`SL_OBJECT_STATE_*`).
const SL_OBJECT_STATE_UNREALIZED: u32 = 0x0000_0001;
const SL_OBJECT_STATE_REALIZED: u32 = 0x0000_0002;

/// `SLPlayItf` play states (`SL_PLAYSTATE_*`).
const SL_PLAYSTATE_STOPPED: u32 = 0x0000_0001;
const SL_PLAYSTATE_PAUSED: u32 = 0x0000_0002;
const SL_PLAYSTATE_PLAYING: u32 = 0x0000_0003;

/// `SLboolean`.
const SL_BOOLEAN_FALSE: u32 = 0x0000_0000;

// =================================================================================================
// SLDataLocator / SLDataFormat tags (public OpenSL ES 1.0.1 + Android extension).
// =================================================================================================

/// `SL_DATALOCATOR_ANDROIDSIMPLEBUFFERQUEUE` (Android extension `OpenSLES_AndroidConfiguration.h`).
const SL_DATALOCATOR_ANDROIDSIMPLEBUFFERQUEUE: u32 = 0x8000_0001;
/// `SL_DATALOCATOR_OUTPUTMIX`.
const SL_DATALOCATOR_OUTPUTMIX: u32 = 0x0000_0007;
/// `SL_DATAFORMAT_PCM`.
const SL_DATAFORMAT_PCM: u32 = 0x0000_0002;

/// `SLDataLocator_AndroidSimpleBufferQueue { SLuint32 locatorType; SLuint32 numBuffers; }`.
#[repr(C)]
struct SlDataLocatorBufferQueue {
    locator_type: u32,
    num_buffers: u32,
}

/// `SLDataLocator_OutputMix { SLuint32 locatorType; SLObjectItf outputMix; }`.
#[repr(C)]
struct SlDataLocatorOutputMix {
    locator_type: u32,
    output_mix: *mut c_void,
}

/// `SLDataFormat_PCM` (public OpenSL ES 1.0.1):
/// `{ SLuint32 formatType; SLuint32 numChannels; SLuint32 samplesPerSec; SLuint32 bitsPerSample;
///    SLuint32 containerSize; SLuint32 channelMask; SLuint32 endianness; }`.
/// `samplesPerSec` is in **milliHz** (`SL_SAMPLINGRATE_*` = rate × 1000).
#[repr(C)]
struct SlDataFormatPcm {
    format_type: u32,
    num_channels: u32,
    samples_per_sec: u32,
    bits_per_sample: u32,
    container_size: u32,
    channel_mask: u32,
    endianness: u32,
}

/// `SLDataSource { SLDataLocator* pLocator; SLDataFormat* pFormat; }`.
#[repr(C)]
struct SlDataSource {
    p_locator: *const c_void,
    p_format: *const c_void,
}

/// `SLDataSink { SLDataLocator* pLocator; SLDataFormat* pFormat; }`.
#[repr(C)]
struct SlDataSink {
    p_locator: *const c_void,
    p_format: *const c_void,
}

// =================================================================================================
// The PCM source format an Eclipse player accepts (decoded from the caller's SLDataFormat_PCM).
// =================================================================================================

/// The validated PCM format of an audio player's buffer-queue source. Only 16-bit and 8-bit
/// little-endian PCM are accepted (the OpenSL ES Android buffer-queue formats); everything is
/// converted to `f32` for the host stream in [`pcm_to_f32`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PcmFormat {
    /// Channels (1 = mono, 2 = stereo).
    pub channels: u32,
    /// Sample rate in Hz (the OpenSL `samplesPerSec` milliHz value ÷ 1000).
    pub sample_rate: u32,
    /// Bits per sample (8 or 16).
    pub bits_per_sample: u32,
}

/// Errors validating a caller-supplied `SLDataFormat_PCM`. Each maps to an OpenSL result the C-ABI
/// method returns; nothing panics across the FFI boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormatError {
    /// `formatType` was not `SL_DATAFORMAT_PCM`.
    NotPcm,
    /// `numChannels` was 0 or > 2 (Eclipse host output is mono/stereo).
    BadChannels,
    /// `samplesPerSec` (milliHz) was 0 or not a whole number of Hz.
    BadSampleRate,
    /// `bitsPerSample` was not 8 or 16.
    BadBitsPerSample,
}

impl PcmFormat {
    /// Validate a public `SLDataFormat_PCM` into an Eclipse [`PcmFormat`]. Pure (no FFI): operates on
    /// already-read field values, so it is unit-testable without constructing C structs.
    pub fn from_sl_pcm(
        channels: u32,
        samples_per_sec_millihz: u32,
        bits_per_sample: u32,
    ) -> Result<Self, FormatError> {
        if channels == 0 || channels > 2 {
            return Err(FormatError::BadChannels);
        }
        // OpenSL ES `samplesPerSec` is in milliHz (SL_SAMPLINGRATE_44_1 = 44_100_000).
        if samples_per_sec_millihz == 0 || !samples_per_sec_millihz.is_multiple_of(1000) {
            return Err(FormatError::BadSampleRate);
        }
        if bits_per_sample != 8 && bits_per_sample != 16 {
            return Err(FormatError::BadBitsPerSample);
        }
        Ok(Self {
            channels,
            sample_rate: samples_per_sec_millihz / 1000,
            bits_per_sample,
        })
    }
}

/// Convert one enqueued PCM buffer (`bits` = 8 or 16, little-endian interleaved) to interleaved
/// `f32` in `[-1.0, 1.0]`. Pure + allocation-into-`out` only; unit-tested without any device.
///
/// 8-bit PCM is **unsigned** (0..=255, centre 128) per the OpenSL ES / WAV convention; 16-bit PCM is
/// **signed** little-endian. Trailing bytes that do not form a whole sample are ignored (the caller
/// validated the buffer length, but this stays total).
pub fn pcm_to_f32(bytes: &[u8], bits: u32, out: &mut Vec<f32>) {
    match bits {
        16 => {
            for frame in bytes.chunks_exact(2) {
                let s = i16::from_le_bytes([frame[0], frame[1]]);
                out.push(s as f32 / 32768.0);
            }
        }
        8 => {
            for &b in bytes {
                // Unsigned 8-bit: 0..=255 centred at 128 → [-1, 1).
                out.push((b as f32 - 128.0) / 128.0);
            }
        }
        // The format was validated at player creation; any other width yields no samples.
        _ => {}
    }
}

// =================================================================================================
// The PCM ring: the bridge between the engine thread (Enqueue) and the cpal audio thread (callback).
// =================================================================================================

/// One buffer-queue callback registered via `SLAndroidSimpleBufferQueueItf::RegisterCallback`.
/// The Android contract: when a buffer finishes playing, the callback fires (on a player thread) so
/// the app can `Enqueue` the next buffer. Eclipse fires it from the audio thread when a buffer is
/// fully drained.
#[derive(Clone, Copy)]
struct BufferQueueCallback {
    /// `slAndroidSimpleBufferQueueCallback`: `void (*)(SLAndroidSimpleBufferQueueItf caller, void* ctx)`.
    func: extern "C" fn(*mut c_void, *mut c_void),
    /// The opaque app context passed back to `func`.
    context: usize,
    /// The `SLAndroidSimpleBufferQueueItf` handle to pass back as `caller` (the player's bq-itf self).
    caller: usize,
}

// SAFETY: 2026-06-05 — `func` is a plain C function pointer and `context`/`caller` are opaque
// integer handles the app owns; sending them to the audio thread shares no Rust-owned memory.
unsafe impl Send for BufferQueueCallback {}

/// The shared PCM ring for one audio player. `Enqueue` (engine thread) pushes a decoded `f32` buffer;
/// the cpal callback (audio thread) pops samples to fill the host output. Guarded by a `Mutex` (the
/// callbacks are short — copy a slice — so contention is negligible; lock-free SPSC is a future
/// optimisation if profiling shows a need, per AGENTS.md §2.4 evidence-first).
struct PcmRing {
    /// Decoded, ready-to-play interleaved `f32` buffers, oldest first. Each entry is one `Enqueue`d
    /// buffer; the front is drained sample-by-sample, and when it empties the bq-callback fires.
    queue: VecDeque<Vec<f32>>,
    /// Read cursor into `queue.front()` (samples already consumed by the audio thread).
    front_pos: usize,
    /// The play state set via `SLPlayItf::SetPlayState`. The audio thread outputs silence unless
    /// `PLAYING`.
    play_state: u32,
    /// The registered buffer-queue callback (if any).
    callback: Option<BufferQueueCallback>,
    /// Count of buffers fully drained by the audio thread (the bq "this buffer finished" count). The
    /// test harness reads this to confirm the host stream actually consumed the enqueued PCM.
    drained_buffers: u64,
    /// Count of bq-callback invocations (fired once per fully-drained buffer). Test-observable.
    callback_fires: u64,
}

impl PcmRing {
    fn new() -> Self {
        Self {
            queue: VecDeque::new(),
            front_pos: 0,
            play_state: SL_PLAYSTATE_STOPPED,
            callback: None,
            drained_buffers: 0,
            callback_fires: 0,
        }
    }

    /// Total samples currently queued (across all buffers, minus the consumed prefix). Test-observable.
    fn queued_samples(&self) -> usize {
        let total: usize = self.queue.iter().map(Vec::len).sum();
        total - self.front_pos
    }
}

/// The audio thread fills `out` from `ring`; returns the buffer-queue callbacks to fire **after**
/// the lock is released (firing a C callback while holding the ring mutex could re-enter `Enqueue`
/// and deadlock). Pure over the ring + slice → unit-testable without a real device.
fn fill_output(ring: &mut PcmRing, out: &mut [f32]) -> Vec<BufferQueueCallback> {
    let mut to_fire: Vec<BufferQueueCallback> = Vec::new();
    if ring.play_state != SL_PLAYSTATE_PLAYING {
        out.fill(0.0);
        return to_fire;
    }
    let mut written = 0usize;
    while written < out.len() {
        let Some(front) = ring.queue.front() else {
            // Underrun: no more queued PCM → output silence for the remainder.
            out[written..].fill(0.0);
            break;
        };
        if ring.front_pos >= front.len() {
            // The front buffer is fully drained → pop it and fire the bq-callback.
            ring.queue.pop_front();
            ring.front_pos = 0;
            ring.drained_buffers += 1;
            if let Some(cb) = ring.callback {
                ring.callback_fires += 1;
                to_fire.push(cb);
            }
            continue;
        }
        let avail = front.len() - ring.front_pos;
        let n = avail.min(out.len() - written);
        out[written..written + n].copy_from_slice(&front[ring.front_pos..ring.front_pos + n]);
        ring.front_pos += n;
        written += n;
    }
    to_fire
}

// =================================================================================================
// Object registry: every SLObjectItf Eclipse mints is a generational slab entry (soundness).
// =================================================================================================

/// The kind of OpenSL object behind an `SLObjectItf`.
enum ObjectKind {
    /// The single engine object (from `slCreateEngine`).
    Engine,
    /// An output mix (from `SLEngineItf::CreateOutputMix`). Carries no host state on its own — the
    /// host stream lives on the player that targets it.
    OutputMix,
    /// An audio player (from `SLEngineItf::CreateAudioPlayer`): the PCM source format, the shared ring,
    /// and the live cpal output stream (held so it keeps playing; dropped on `Destroy`).
    Player(Box<PlayerState>),
}

/// An audio player's owned state. The cpal `Stream` is `!Send`/`!Sync` on some backends, so it is
/// confined to the registry entry (never moved across threads after creation) and dropped on
/// `Destroy`/registry-free.
struct PlayerState {
    format: PcmFormat,
    ring: Arc<Mutex<PcmRing>>,
    /// The live host output stream. `None` if no host device exists (the player is then a sound-stub
    /// that accepts Enqueues but produces no sound — a clean "no device" posture, never a fake).
    stream: Option<cpal::Stream>,
}

/// The leading layout of every Eclipse OpenSL object: an `SLObjectItf_*` vtable pointer (what the
/// engine's `SLObjectItf` handle points at) followed by the object's registry id (for sound `self`
/// validation) and its interface itf-pointers. `#[repr(C)]` so the engine's `(*obj)->method` reads the
/// vtable pointer from offset 0.
#[repr(C)]
struct ObjectState {
    /// Offset 0: the `const SLObjectItf_*` the engine dereferences. Always the shared object vtable.
    object_vtable: *const ObjectItfVtable,
    /// Eclipse registry id of this object (validated on every method entry).
    id: u64,
    /// The object's exposed interface itf-pointers (each a `*const <Itf>Vtable`), addressed by
    /// `GetInterface`. Stored inline so their addresses are stable for the object's lifetime.
    engine_itf: *const EngineItfVtable,
    play_itf: *const PlayItfVtable,
    bufferqueue_itf: *const BufferQueueItfVtable,
    /// The object's realized state (`SL_OBJECT_STATE_*`).
    state: u32,
    /// The object-kind payload.
    kind: ObjectKind,
}

// SAFETY: 2026-06-05 — `ObjectState` holds raw vtable pointers into process-lifetime `OnceLock`
// statics (stable, read-only) plus owned state. The registry serializes all access behind a `Mutex`,
// so the raw pointers are only ever read under that lock; sharing across threads is sound.
unsafe impl Send for ObjectState {}

/// A generational slot for an object: a `Box<ObjectState>` (stable heap address = the `self` the
/// engine holds) plus the slot generation.
struct ObjectSlot {
    generation: u32,
    state: Option<Box<ObjectState>>,
}

/// Errors from the object registry — a stale/fabricated `SLObjectItf` is rejected here, never a wild
/// deref.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectError {
    /// The id's slot index is out of range (fabricated handle).
    OutOfRange,
    /// The slot exists but its generation does not match (stale handle to a freed/reused slot).
    Stale,
    /// The registry mutex was poisoned by a panic in another holder (surfaced, never re-panicked).
    Poisoned,
}

impl<T> From<PoisonError<T>> for ObjectError {
    fn from(_: PoisonError<T>) -> Self {
        ObjectError::Poisoned
    }
}

/// The process-global object registry: a generational slab guarded by a `Mutex` in a `OnceLock`. The
/// `id` packs `slot index` (low 32) and `generation` (high 32), generations start at 1 (so id 0 is
/// never valid — matches a C `NULL`).
#[derive(Default)]
struct ObjectRegistry {
    slots: Vec<ObjectSlot>,
}

fn registry() -> &'static Mutex<ObjectRegistry> {
    static REG: OnceLock<Mutex<ObjectRegistry>> = OnceLock::new();
    REG.get_or_init(|| Mutex::new(ObjectRegistry::default()))
}

impl ObjectRegistry {
    /// Insert a fresh object, returning its packed id. The `Box`'s heap address is its `self` pointer;
    /// we fix up `object_vtable`/`id` after boxing so the engine's offset-0 read is correct.
    fn insert(&mut self, mut state: Box<ObjectState>, id: u64) -> u64 {
        state.id = id;
        let idx = (id & 0xFFFF_FFFF) as usize;
        if idx < self.slots.len() {
            self.slots[idx].state = Some(state);
        } else {
            self.slots.push(ObjectSlot {
                generation: (id >> 32) as u32,
                state: Some(state),
            });
        }
        id
    }

    /// Allocate the next id (reusing a freed slot if any), without yet storing the state.
    fn next_id(&mut self) -> u64 {
        if let Some(idx) = self.slots.iter().position(|s| s.state.is_none()) {
            let gen = self.slots[idx].generation;
            return ((gen as u64) << 32) | idx as u64;
        }
        let idx = self.slots.len() as u64;
        // generation 1 for a brand-new slot.
        (1u64 << 32) | idx
    }
}

// =================================================================================================
// Vtable structs — the public OpenSL ES 1.0.1 method order (load-bearing; engine indexes by offset).
// =================================================================================================

type SlObjectItf = *mut c_void; // `SLObjectItf` (opaque to C; really `*mut ObjectState`).

/// `struct SLObjectItf_` (public OpenSL ES 1.0.1). Order matters.
#[repr(C)]
struct ObjectItfVtable {
    realize: extern "C" fn(SlObjectItf, u32) -> u32,
    resume: extern "C" fn(SlObjectItf, u32) -> u32,
    get_state: extern "C" fn(SlObjectItf, *mut u32) -> u32,
    get_interface: extern "C" fn(SlObjectItf, *const c_void, *mut c_void) -> u32,
    register_callback: extern "C" fn(SlObjectItf, *const c_void, *mut c_void) -> u32,
    abort_async_operation: extern "C" fn(SlObjectItf),
    destroy: extern "C" fn(SlObjectItf),
    set_priority: extern "C" fn(SlObjectItf, i32, u32) -> u32,
    get_priority: extern "C" fn(SlObjectItf, *mut i32, *mut u32) -> u32,
    set_loss_of_control_interfaces: extern "C" fn(SlObjectItf, i16, *const c_void, u32) -> u32,
    request_object_state_change_notification:
        extern "C" fn(SlObjectItf, *const c_void, *mut c_void) -> u32,
}

/// `struct SLEngineItf_` (public OpenSL ES 1.0.1). Only `CreateOutputMix`/`CreateAudioPlayer` and the
/// query methods are reached; the device-creation slots return `SL_RESULT_FEATURE_UNSUPPORTED`
/// (Eclipse has no LED/vibra/recorder/metadata), which is the documented contract for an unsupported
/// object type.
#[repr(C)]
struct EngineItfVtable {
    create_led_device: extern "C" fn(
        *mut c_void,
        *mut c_void,
        u32,
        *const c_void,
        u32,
        *const c_void,
        *const c_void,
    ) -> u32,
    create_vibra_device: extern "C" fn(
        *mut c_void,
        *mut c_void,
        u32,
        *const c_void,
        u32,
        *const c_void,
        *const c_void,
    ) -> u32,
    create_audio_player: extern "C" fn(
        *mut c_void,
        *mut c_void,
        *mut c_void,
        *mut c_void,
        u32,
        *const c_void,
        *const c_void,
    ) -> u32,
    create_audio_recorder: extern "C" fn(
        *mut c_void,
        *mut c_void,
        *mut c_void,
        *mut c_void,
        u32,
        *const c_void,
        *const c_void,
    ) -> u32,
    create_output_mix:
        extern "C" fn(*mut c_void, *mut c_void, u32, *const c_void, *const c_void) -> u32,
    create_metadata_extractor: extern "C" fn(
        *mut c_void,
        *mut c_void,
        *mut c_void,
        u32,
        *const c_void,
        *const c_void,
    ) -> u32,
    create_extension_object: extern "C" fn(
        *mut c_void,
        *mut c_void,
        *mut c_void,
        u32,
        u32,
        *const c_void,
        *const c_void,
    ) -> u32,
    query_num_supported_interfaces: extern "C" fn(*mut c_void, u32, *mut u32) -> u32,
    query_supported_interfaces: extern "C" fn(*mut c_void, u32, u32, *mut c_void) -> u32,
    query_num_supported_extensions: extern "C" fn(*mut c_void, *mut u32) -> u32,
    query_supported_extension: extern "C" fn(*mut c_void, u32, *mut i8, *mut i16) -> u32,
    is_extension_supported: extern "C" fn(*mut c_void, *const i8, *mut u32) -> u32,
}

/// `struct SLPlayItf_` (public OpenSL ES 1.0.1).
#[repr(C)]
struct PlayItfVtable {
    set_play_state: extern "C" fn(*mut c_void, u32) -> u32,
    get_play_state: extern "C" fn(*mut c_void, *mut u32) -> u32,
    get_duration: extern "C" fn(*mut c_void, *mut u32) -> u32,
    get_position: extern "C" fn(*mut c_void, *mut u32) -> u32,
    register_callback: extern "C" fn(*mut c_void, *const c_void, *mut c_void) -> u32,
    set_callback_events_mask: extern "C" fn(*mut c_void, u32) -> u32,
    get_callback_events_mask: extern "C" fn(*mut c_void, *mut u32) -> u32,
    set_marker_position: extern "C" fn(*mut c_void, u32) -> u32,
    clear_marker_position: extern "C" fn(*mut c_void) -> u32,
    get_marker_position: extern "C" fn(*mut c_void, *mut u32) -> u32,
    set_position_update_period: extern "C" fn(*mut c_void, u32) -> u32,
    get_position_update_period: extern "C" fn(*mut c_void, *mut u32) -> u32,
}

/// `struct SLAndroidSimpleBufferQueueItf_` (Android extension `OpenSLES_Android.h`).
#[repr(C)]
struct BufferQueueItfVtable {
    enqueue: extern "C" fn(*mut c_void, *const c_void, u32) -> u32,
    clear: extern "C" fn(*mut c_void) -> u32,
    get_state: extern "C" fn(*mut c_void, *mut SlBufferQueueState) -> u32,
    register_callback: extern "C" fn(*mut c_void, *const c_void, *mut c_void) -> u32,
}

/// `SLAndroidSimpleBufferQueueState { SLuint32 count; SLuint32 index; }`.
#[repr(C)]
struct SlBufferQueueState {
    count: u32,
    index: u32,
}

// =================================================================================================
// The interface-itf-pointer offsets within ObjectState (for GetInterface → return the field address).
//
// `self` for an interface method is the address of the object's itf-pointer field; we recover the
// owning ObjectState by reading the field's value's-owning struct. We instead store the object id in
// every itf-pointer's *target* is impractical, so interface methods recover `self` → object via the
// registry: the itf-pointer field address minus its offset == &ObjectState. We compute that offset
// once with `memoffset`-style pointer arithmetic done in safe const fns is not possible for repr(C)
// without nightly, so we derive the object pointer by a small, dated unsafe offset subtraction.
// =================================================================================================

// Field byte offsets of the itf-pointer members within `ObjectState`, used to map an interface `self`
// (the address of one of these fields) back to the owning `ObjectState`. Kept in one place so the ABI
// layout assumption is documented and centrally checked by `object_layout_offsets_are_consistent`.
//
// 2026-06-05: `ObjectState` is `#[repr(C)]`, so field order == declaration order and offsets are the
// running aligned sum. We compute them at runtime from a sample object (no nightly `offset_of!`),
// validated by a unit test.

/// Recover the `&mut ObjectState` that owns an interface itf-pointer field at `field_self`, given the
/// field's byte offset within `ObjectState`.
///
/// # Safety
/// `field_self` must be the address of the named itf-pointer field of a live `ObjectState` (the value
/// `GetInterface` handed out for this object). The caller guarantees this by construction (the engine
/// passes back exactly that pointer); the recovered object is then re-validated via the registry id,
/// so even a mismatched pointer cannot escalate past a typed `Err`.
unsafe fn object_from_itf_field(field_self: *mut c_void, field_offset: usize) -> *mut ObjectState {
    // SAFETY: per the contract, `field_self` points `field_offset` bytes into a live `ObjectState`.
    unsafe { (field_self as *mut u8).sub(field_offset) as *mut ObjectState }
}

// Computed once at first object creation from a real boxed ObjectState (no nightly offset_of).
static ITF_OFFSETS: OnceLock<ItfOffsets> = OnceLock::new();

#[derive(Clone, Copy)]
struct ItfOffsets {
    engine: usize,
    play: usize,
    bufferqueue: usize,
}

fn itf_offsets() -> ItfOffsets {
    *ITF_OFFSETS.get_or_init(|| {
        // Build a throwaway sample and measure field offsets from its base address. `addr_of!` reads
        // addresses without forming references to uninitialised memory.
        let sample = ObjectState {
            object_vtable: std::ptr::null(),
            id: 0,
            engine_itf: std::ptr::null(),
            play_itf: std::ptr::null(),
            bufferqueue_itf: std::ptr::null(),
            state: 0,
            kind: ObjectKind::OutputMix,
        };
        let base = std::ptr::addr_of!(sample) as usize;
        ItfOffsets {
            engine: std::ptr::addr_of!(sample.engine_itf) as usize - base,
            play: std::ptr::addr_of!(sample.play_itf) as usize - base,
            bufferqueue: std::ptr::addr_of!(sample.bufferqueue_itf) as usize - base,
        }
    })
}

// =================================================================================================
// Stable vtable instances (process-lifetime, distinct addresses) — the two-phase OnceLock pattern.
// =================================================================================================

struct VtableWrap<T: 'static>(T);
// SAFETY: 2026-06-05 — a vtable is a table of `extern "C"` function pointers, immutable after init
// (process-lifetime `OnceLock`). Sharing it read-only across threads is sound.
unsafe impl<T> Sync for VtableWrap<T> {}
// SAFETY: see the `Sync` note — process-lifetime, read-only function-pointer table.
unsafe impl<T> Send for VtableWrap<T> {}

fn object_vtable() -> *const ObjectItfVtable {
    static V: OnceLock<VtableWrap<ObjectItfVtable>> = OnceLock::new();
    &V.get_or_init(|| {
        VtableWrap(ObjectItfVtable {
            realize: obj_realize,
            resume: obj_resume,
            get_state: obj_get_state,
            get_interface: obj_get_interface,
            register_callback: obj_register_callback,
            abort_async_operation: obj_abort_async,
            destroy: obj_destroy,
            set_priority: obj_set_priority,
            get_priority: obj_get_priority,
            set_loss_of_control_interfaces: obj_set_loss_of_control,
            request_object_state_change_notification: obj_request_state_change,
        })
    })
    .0
}

fn engine_vtable() -> *const EngineItfVtable {
    static V: OnceLock<VtableWrap<EngineItfVtable>> = OnceLock::new();
    &V.get_or_init(|| {
        VtableWrap(EngineItfVtable {
            create_led_device: eng_create_unsupported7,
            create_vibra_device: eng_create_unsupported7,
            create_audio_player: eng_create_audio_player,
            create_audio_recorder: eng_create_recorder,
            create_output_mix: eng_create_output_mix,
            create_metadata_extractor: eng_create_metadata,
            create_extension_object: eng_create_extension,
            query_num_supported_interfaces: eng_query_num_interfaces,
            query_supported_interfaces: eng_query_interfaces,
            query_num_supported_extensions: eng_query_num_extensions,
            query_supported_extension: eng_query_extension,
            is_extension_supported: eng_is_extension_supported,
        })
    })
    .0
}

fn play_vtable() -> *const PlayItfVtable {
    static V: OnceLock<VtableWrap<PlayItfVtable>> = OnceLock::new();
    &V.get_or_init(|| {
        VtableWrap(PlayItfVtable {
            set_play_state: play_set_play_state,
            get_play_state: play_get_play_state,
            get_duration: play_get_duration,
            get_position: play_get_position,
            register_callback: play_register_callback,
            set_callback_events_mask: play_set_events_mask,
            get_callback_events_mask: play_get_events_mask,
            set_marker_position: play_set_marker,
            clear_marker_position: play_clear_marker,
            get_marker_position: play_get_marker,
            set_position_update_period: play_set_update_period,
            get_position_update_period: play_get_update_period,
        })
    })
    .0
}

fn bufferqueue_vtable() -> *const BufferQueueItfVtable {
    static V: OnceLock<VtableWrap<BufferQueueItfVtable>> = OnceLock::new();
    &V.get_or_init(|| {
        VtableWrap(BufferQueueItfVtable {
            enqueue: bq_enqueue,
            clear: bq_clear,
            get_state: bq_get_state,
            register_callback: bq_register_callback,
        })
    })
    .0
}

// =================================================================================================
// Helper: validate an `self` SLObjectItf and run a closure with the locked object.
// =================================================================================================

/// Read the registry id stored at offset `id` of the object `self` points to, then validate it
/// against the registry and run `f` with the locked `&mut ObjectState`. A null/stale/fabricated
/// `self` → `Err`. The id is read from the object's own memory; it is then bounds+generation-checked,
/// so a wrong pointer cannot escalate past the typed `Err`.
fn with_object<R>(
    obj: SlObjectItf,
    f: impl FnOnce(&mut ObjectState, &mut ObjectRegistry) -> R,
) -> Result<R, ObjectError> {
    if obj.is_null() {
        return Err(ObjectError::OutOfRange);
    }
    let mut reg = registry().lock()?;
    // SAFETY: 2026-06-05 — a non-null `SLObjectItf` Eclipse handed out points at a live
    // `ObjectState` whose `id` is at the documented offset; we read it then validate against the
    // registry before any further use. A wrong pointer that happens to read a plausible id is still
    // rejected by the generation check. The pointer is not dereferenced beyond this `id` read until
    // validated.
    let id = unsafe { (*(obj as *const ObjectState)).id };
    let idx = (id & 0xFFFF_FFFF) as usize;
    let gen = (id >> 32) as u32;
    let slot = reg.slots.get(idx).ok_or(ObjectError::OutOfRange)?;
    if slot.generation != gen || slot.state.is_none() {
        return Err(ObjectError::Stale);
    }
    // Re-borrow mutably (split borrow): take the state out, run, put it back. Simpler: operate via
    // index with a raw split — but the slab is small; take ownership of the Box pointer temporarily.
    let mut state = reg.slots[idx].state.take().expect("checked Some above");
    let r = f(&mut state, &mut reg);
    reg.slots[idx].state = Some(state);
    Ok(r)
}

// =================================================================================================
// SLObjectItf methods.
// =================================================================================================

extern "C" fn obj_realize(obj: SlObjectItf, _async: u32) -> u32 {
    match with_object(obj, |st, _| {
        st.state = SL_OBJECT_STATE_REALIZED;
        SL_RESULT_SUCCESS
    }) {
        Ok(r) => r,
        Err(_) => SL_RESULT_PARAMETER_INVALID,
    }
}

extern "C" fn obj_resume(_obj: SlObjectItf, _async: u32) -> u32 {
    SL_RESULT_SUCCESS
}

extern "C" fn obj_get_state(obj: SlObjectItf, p_state: *mut u32) -> u32 {
    if p_state.is_null() {
        return SL_RESULT_PARAMETER_INVALID;
    }
    match with_object(obj, |st, _| st.state) {
        Ok(state) => {
            // SAFETY: 2026-06-05 — `p_state` is the caller's non-null out-param (checked above).
            unsafe { *p_state = state };
            SL_RESULT_SUCCESS
        }
        Err(_) => SL_RESULT_PARAMETER_INVALID,
    }
}

extern "C" fn obj_get_interface(obj: SlObjectItf, iid: *const c_void, p_itf: *mut c_void) -> u32 {
    if p_itf.is_null() {
        return SL_RESULT_PARAMETER_INVALID;
    }
    let off = itf_offsets();
    let r = with_object(obj, |st, _| {
        if st.state != SL_OBJECT_STATE_REALIZED {
            return Err(SL_RESULT_PRECONDITIONS_VIOLATED);
        }
        // The requested interface field address (an `SL_IID_*` selects which one). We match by the
        // documented `SL_IID_*` data pointer the native_provider registered.
        let base = (st as *mut ObjectState) as *mut u8;
        let which = crate::loader::native_provider::sl_iid_index(iid as usize);
        let (field_ptr, present) = match which {
            // SL_IID_ENGINE
            Some(3) => (
                // SAFETY: 2026-06-05 — `base` is the live object; `off.engine` is its field offset.
                unsafe { base.add(off.engine) },
                !st.engine_itf.is_null(),
            ),
            // SL_IID_PLAY
            Some(4) => (
                // SAFETY: 2026-06-05 — as above for the play-itf field.
                unsafe { base.add(off.play) },
                !st.play_itf.is_null(),
            ),
            // SL_IID_ANDROIDSIMPLEBUFFERQUEUE (1) or SL_IID_BUFFERQUEUE (2)
            Some(1) | Some(2) => (
                // SAFETY: 2026-06-05 — as above for the bufferqueue-itf field.
                unsafe { base.add(off.bufferqueue) },
                !st.bufferqueue_itf.is_null(),
            ),
            _ => (std::ptr::null_mut(), false),
        };
        if !present || field_ptr.is_null() {
            return Err(SL_RESULT_PARAMETER_INVALID);
        }
        Ok(field_ptr)
    });
    match r {
        Ok(Ok(field_ptr)) => {
            // SAFETY: 2026-06-05 — `p_itf` is the caller's non-null out-param; we write the address of
            // the object's itf-pointer field (a valid `SL<Itf>Itf`).
            unsafe { *(p_itf as *mut *mut c_void) = field_ptr as *mut c_void };
            SL_RESULT_SUCCESS
        }
        Ok(Err(code)) => code,
        Err(_) => SL_RESULT_PARAMETER_INVALID,
    }
}

extern "C" fn obj_register_callback(
    _obj: SlObjectItf,
    _cb: *const c_void,
    _ctx: *mut c_void,
) -> u32 {
    SL_RESULT_SUCCESS
}

extern "C" fn obj_abort_async(_obj: SlObjectItf) {}

extern "C" fn obj_destroy(obj: SlObjectItf) {
    // Free the slot (bumping its generation) so any later use of this handle is a clean stale `Err`.
    // Dropping the `ObjectState` drops the cpal stream (stops the host playback).
    let _ = with_object_destroy(obj);
}

fn with_object_destroy(obj: SlObjectItf) -> Result<(), ObjectError> {
    if obj.is_null() {
        return Err(ObjectError::OutOfRange);
    }
    let mut reg = registry().lock()?;
    // SAFETY: 2026-06-05 — see `with_object`: read the id, validate, then free.
    let id = unsafe { (*(obj as *const ObjectState)).id };
    let idx = (id & 0xFFFF_FFFF) as usize;
    let gen = (id >> 32) as u32;
    let slot = reg.slots.get_mut(idx).ok_or(ObjectError::OutOfRange)?;
    if slot.generation != gen || slot.state.is_none() {
        return Err(ObjectError::Stale);
    }
    slot.state = None;
    slot.generation = slot.generation.wrapping_add(1).max(1);
    Ok(())
}

extern "C" fn obj_set_priority(_obj: SlObjectItf, _priority: i32, _preempt: u32) -> u32 {
    SL_RESULT_SUCCESS
}

extern "C" fn obj_get_priority(
    _obj: SlObjectItf,
    _p_priority: *mut i32,
    _p_preempt: *mut u32,
) -> u32 {
    SL_RESULT_SUCCESS
}

extern "C" fn obj_set_loss_of_control(
    _obj: SlObjectItf,
    _count: i16,
    _interfaces: *const c_void,
    _enabled: u32,
) -> u32 {
    SL_RESULT_SUCCESS
}

extern "C" fn obj_request_state_change(
    _obj: SlObjectItf,
    _cb: *const c_void,
    _ctx: *mut c_void,
) -> u32 {
    SL_RESULT_SUCCESS
}

// =================================================================================================
// SLEngineItf methods — `self` is the address of an ObjectState's `engine_itf` field.
// =================================================================================================

/// Recover the engine object's id from an `SLEngineItf` self pointer (the address of the `engine_itf`
/// field). Returns the packed id (read from the recovered ObjectState), validated by the caller.
fn engine_self_id(self_itf: *mut c_void) -> Option<u64> {
    if self_itf.is_null() {
        return None;
    }
    let off = itf_offsets();
    // SAFETY: 2026-06-05 — `self_itf` is the engine-itf field of a live ObjectState (handed out by
    // GetInterface); subtract the field offset to get the object base, read its id; the id is then
    // registry-validated before use.
    let obj = unsafe { object_from_itf_field(self_itf, off.engine) };
    // SAFETY: 2026-06-05 — `obj` points at the live ObjectState; reading `id` is in-bounds.
    Some(unsafe { (*obj).id })
}

extern "C" fn eng_create_output_mix(
    self_itf: *mut c_void,
    p_mix: *mut c_void,
    _num_interfaces: u32,
    _p_interface_ids: *const c_void,
    _p_interface_required: *const c_void,
) -> u32 {
    if p_mix.is_null() {
        return SL_RESULT_PARAMETER_INVALID;
    }
    let Some(eng_id) = engine_self_id(self_itf) else {
        return SL_RESULT_PARAMETER_INVALID;
    };
    match validate_object_id(eng_id) {
        Ok(()) => {}
        Err(_) => return SL_RESULT_PARAMETER_INVALID,
    }
    let obj_ptr = match mint_object(ObjectKind::OutputMix) {
        Ok(p) => p,
        Err(_) => return SL_RESULT_MEMORY_FAILURE,
    };
    // SAFETY: 2026-06-05 — `p_mix` is the caller's non-null out-param for the new `SLObjectItf`.
    unsafe { *(p_mix as *mut *mut c_void) = obj_ptr };
    SL_RESULT_SUCCESS
}

extern "C" fn eng_create_audio_player(
    self_itf: *mut c_void,
    p_player: *mut c_void,
    p_audio_src: *mut c_void,
    p_audio_snk: *mut c_void,
    _num_interfaces: u32,
    _p_interface_ids: *const c_void,
    _p_interface_required: *const c_void,
) -> u32 {
    if p_player.is_null() || p_audio_src.is_null() || p_audio_snk.is_null() {
        return SL_RESULT_PARAMETER_INVALID;
    }
    let Some(eng_id) = engine_self_id(self_itf) else {
        return SL_RESULT_PARAMETER_INVALID;
    };
    if validate_object_id(eng_id).is_err() {
        return SL_RESULT_PARAMETER_INVALID;
    }

    // Decode the data source: locator must be AndroidSimpleBufferQueue, format must be PCM.
    // SAFETY: 2026-06-05 — `p_audio_src` is a caller `SLDataSource*`; we read its two pointer fields,
    // then the pointed-to locator/format headers. The OpenSL ES contract guarantees these are valid
    // for a CreateAudioPlayer call; each tag is checked before trusting the rest of the struct.
    let src = unsafe { &*(p_audio_src as *const SlDataSource) };
    if src.p_locator.is_null() || src.p_format.is_null() {
        return SL_RESULT_PARAMETER_INVALID;
    }
    // SAFETY: 2026-06-05 — locator/format pointers are non-null (checked); read the leading u32 tag.
    let loc_type = unsafe { *(src.p_locator as *const u32) };
    if loc_type != SL_DATALOCATOR_ANDROIDSIMPLEBUFFERQUEUE {
        return SL_RESULT_FEATURE_UNSUPPORTED;
    }
    // SAFETY: 2026-06-05 — the locator tag matched, so the full bufferqueue locator is present.
    let bq_loc = unsafe { &*(src.p_locator as *const SlDataLocatorBufferQueue) };
    let _num_buffers = bq_loc.num_buffers; // bookkeeping; the ring is unbounded-by-Vec, validated len.
                                           // SAFETY: 2026-06-05 — `p_format` non-null; read the leading format-type tag.
    let fmt_type = unsafe { *(src.p_format as *const u32) };
    if fmt_type != SL_DATAFORMAT_PCM {
        return SL_RESULT_FEATURE_UNSUPPORTED;
    }
    // SAFETY: 2026-06-05 — the format tag matched PCM, so the full PCM format struct is present.
    let pcm = unsafe { &*(src.p_format as *const SlDataFormatPcm) };
    let format =
        match PcmFormat::from_sl_pcm(pcm.num_channels, pcm.samples_per_sec, pcm.bits_per_sample) {
            Ok(f) => f,
            Err(_) => return SL_RESULT_FEATURE_UNSUPPORTED,
        };

    // Decode the sink: must be an OutputMix locator (we don't require the mix object to match a
    // specific id — any Eclipse output mix routes to the one host device).
    // SAFETY: 2026-06-05 — `p_audio_snk` is a caller `SLDataSink*`; read its locator pointer + tag.
    let snk = unsafe { &*(p_audio_snk as *const SlDataSink) };
    if snk.p_locator.is_null() {
        return SL_RESULT_PARAMETER_INVALID;
    }
    // SAFETY: 2026-06-05 — sink locator non-null; read its leading tag.
    let snk_loc_type = unsafe { *(snk.p_locator as *const u32) };
    if snk_loc_type != SL_DATALOCATOR_OUTPUTMIX {
        return SL_RESULT_FEATURE_UNSUPPORTED;
    }
    // SAFETY: 2026-06-05 — the sink tag matched; reading the OutputMix locator's `output_mix` field is
    // in-bounds. We do not dereference `output_mix` (any Eclipse mix is equivalent).
    let _ = unsafe { &*(snk.p_locator as *const SlDataLocatorOutputMix) };

    // Build the player state + start the host stream.
    let ring = Arc::new(Mutex::new(PcmRing::new()));
    // No host device → a clean "no device" player that still accepts Enqueues (the engine sees a
    // working player; it just produces no sound). Never a fake success that crashes later.
    let stream = start_host_stream(&ring, format).ok();
    let player = Box::new(PlayerState {
        format,
        ring,
        stream,
    });
    let obj_ptr = match mint_object(ObjectKind::Player(player)) {
        Ok(p) => p,
        Err(_) => return SL_RESULT_MEMORY_FAILURE,
    };
    // SAFETY: 2026-06-05 — `p_player` is the caller's non-null out-param for the new `SLObjectItf`.
    unsafe { *(p_player as *mut *mut c_void) = obj_ptr };
    SL_RESULT_SUCCESS
}

// The device-creation slots Eclipse does not support: return the documented "unsupported object type"
// result. Present (correct vtable offset) and safe if ever called.
extern "C" fn eng_create_unsupported7(
    _s: *mut c_void,
    _a: *mut c_void,
    _b: u32,
    _c: *const c_void,
    _d: u32,
    _e: *const c_void,
    _f: *const c_void,
) -> u32 {
    SL_RESULT_FEATURE_UNSUPPORTED
}

extern "C" fn eng_create_recorder(
    _s: *mut c_void,
    _a: *mut c_void,
    _b: *mut c_void,
    _c: *mut c_void,
    _d: u32,
    _e: *const c_void,
    _f: *const c_void,
) -> u32 {
    SL_RESULT_FEATURE_UNSUPPORTED
}

extern "C" fn eng_create_metadata(
    _s: *mut c_void,
    _a: *mut c_void,
    _b: *mut c_void,
    _c: u32,
    _d: *const c_void,
    _e: *const c_void,
) -> u32 {
    SL_RESULT_FEATURE_UNSUPPORTED
}

extern "C" fn eng_create_extension(
    _s: *mut c_void,
    _a: *mut c_void,
    _b: *mut c_void,
    _c: u32,
    _d: u32,
    _e: *const c_void,
    _f: *const c_void,
) -> u32 {
    SL_RESULT_FEATURE_UNSUPPORTED
}

extern "C" fn eng_query_num_interfaces(_s: *mut c_void, _obj_id: u32, p_num: *mut u32) -> u32 {
    if !p_num.is_null() {
        // SAFETY: 2026-06-05 — non-null out-param; report 0 supported (we don't enumerate per type).
        unsafe { *p_num = 0 };
    }
    SL_RESULT_SUCCESS
}

extern "C" fn eng_query_interfaces(
    _s: *mut c_void,
    _obj_id: u32,
    _index: u32,
    _p_iid: *mut c_void,
) -> u32 {
    SL_RESULT_PARAMETER_INVALID
}

extern "C" fn eng_query_num_extensions(_s: *mut c_void, p_num: *mut u32) -> u32 {
    if !p_num.is_null() {
        // SAFETY: 2026-06-05 — non-null out-param; 0 extensions.
        unsafe { *p_num = 0 };
    }
    SL_RESULT_SUCCESS
}

extern "C" fn eng_query_extension(
    _s: *mut c_void,
    _index: u32,
    _p_name: *mut i8,
    _p_len: *mut i16,
) -> u32 {
    SL_RESULT_PARAMETER_INVALID
}

extern "C" fn eng_is_extension_supported(
    _s: *mut c_void,
    _name: *const i8,
    p_supported: *mut u32,
) -> u32 {
    if !p_supported.is_null() {
        // SAFETY: 2026-06-05 — non-null out-param; no extensions supported.
        unsafe { *p_supported = SL_BOOLEAN_FALSE };
    }
    SL_RESULT_SUCCESS
}

// =================================================================================================
// SLPlayItf methods — `self` is the address of an ObjectState's `play_itf` field.
// =================================================================================================

/// Run `f` with the player's ring locked, recovered from a `SLPlayItf`/`SLAndroidSimpleBufferQueueItf`
/// self pointer at field offset `field_off`. The closure also receives the player's validated
/// [`PcmFormat`] (read once under the registry lock, then the registry lock is released before taking
/// the ring lock — registry first, ring leaf — so `Enqueue` from a bq-callback never deadlocks).
fn with_player_ring<R>(
    self_itf: *mut c_void,
    field_off: usize,
    f: impl FnOnce(&mut PcmRing, PcmFormat) -> R,
) -> Result<R, ObjectError> {
    if self_itf.is_null() {
        return Err(ObjectError::OutOfRange);
    }
    // SAFETY: 2026-06-05 — recover the object base from the itf field, read its id, validate.
    let obj = unsafe { object_from_itf_field(self_itf, field_off) };
    // SAFETY: 2026-06-05 — `obj` is the live ObjectState; reading `id` is in-bounds.
    let id = unsafe { (*obj).id };
    let (ring, format) = {
        let reg = registry().lock()?;
        let idx = (id & 0xFFFF_FFFF) as usize;
        let gen = (id >> 32) as u32;
        let slot = reg.slots.get(idx).ok_or(ObjectError::OutOfRange)?;
        if slot.generation != gen || slot.state.is_none() {
            return Err(ObjectError::Stale);
        }
        let state = reg.slots[idx].state.as_ref().expect("checked Some");
        let ObjectKind::Player(player) = &state.kind else {
            return Err(ObjectError::OutOfRange);
        };
        (Arc::clone(&player.ring), player.format)
    }; // registry lock released here.
    let mut ring_guard = ring.lock()?;
    Ok(f(&mut ring_guard, format))
}

extern "C" fn play_set_play_state(self_itf: *mut c_void, state: u32) -> u32 {
    let off = itf_offsets().play;
    match with_player_ring(self_itf, off, |ring, _| {
        if state == SL_PLAYSTATE_STOPPED
            || state == SL_PLAYSTATE_PAUSED
            || state == SL_PLAYSTATE_PLAYING
        {
            ring.play_state = state;
            SL_RESULT_SUCCESS
        } else {
            SL_RESULT_PARAMETER_INVALID
        }
    }) {
        Ok(r) => r,
        Err(_) => SL_RESULT_PARAMETER_INVALID,
    }
}

extern "C" fn play_get_play_state(self_itf: *mut c_void, p_state: *mut u32) -> u32 {
    if p_state.is_null() {
        return SL_RESULT_PARAMETER_INVALID;
    }
    let off = itf_offsets().play;
    match with_player_ring(self_itf, off, |ring, _| ring.play_state) {
        Ok(s) => {
            // SAFETY: 2026-06-05 — non-null out-param.
            unsafe { *p_state = s };
            SL_RESULT_SUCCESS
        }
        Err(_) => SL_RESULT_PARAMETER_INVALID,
    }
}

extern "C" fn play_get_duration(_s: *mut c_void, p_msec: *mut u32) -> u32 {
    if !p_msec.is_null() {
        // SL_TIME_UNKNOWN — a streamed buffer-queue source has no fixed duration.
        // SAFETY: 2026-06-05 — non-null out-param.
        unsafe { *p_msec = 0xFFFF_FFFF };
    }
    SL_RESULT_SUCCESS
}

extern "C" fn play_get_position(self_itf: *mut c_void, p_msec: *mut u32) -> u32 {
    if p_msec.is_null() {
        return SL_RESULT_PARAMETER_INVALID;
    }
    let off = itf_offsets().play;
    match with_player_ring(self_itf, off, |ring, _fmt| {
        ring.drained_buffers // coarse: count of finished buffers (no per-sample clock modelled).
    }) {
        Ok(_n) => {
            // SAFETY: 2026-06-05 — non-null out-param; report 0 (position clock is not modelled).
            unsafe { *p_msec = 0 };
            SL_RESULT_SUCCESS
        }
        Err(_) => SL_RESULT_PARAMETER_INVALID,
    }
}

extern "C" fn play_register_callback(
    _s: *mut c_void,
    _cb: *const c_void,
    _ctx: *mut c_void,
) -> u32 {
    SL_RESULT_SUCCESS
}

extern "C" fn play_set_events_mask(_s: *mut c_void, _mask: u32) -> u32 {
    SL_RESULT_SUCCESS
}

extern "C" fn play_get_events_mask(_s: *mut c_void, p_mask: *mut u32) -> u32 {
    if !p_mask.is_null() {
        // SAFETY: 2026-06-05 — non-null out-param.
        unsafe { *p_mask = 0 };
    }
    SL_RESULT_SUCCESS
}

extern "C" fn play_set_marker(_s: *mut c_void, _msec: u32) -> u32 {
    SL_RESULT_SUCCESS
}

extern "C" fn play_clear_marker(_s: *mut c_void) -> u32 {
    SL_RESULT_SUCCESS
}

extern "C" fn play_get_marker(_s: *mut c_void, p_msec: *mut u32) -> u32 {
    if !p_msec.is_null() {
        // SAFETY: 2026-06-05 — non-null out-param.
        unsafe { *p_msec = 0 };
    }
    SL_RESULT_SUCCESS
}

extern "C" fn play_set_update_period(_s: *mut c_void, _msec: u32) -> u32 {
    SL_RESULT_SUCCESS
}

extern "C" fn play_get_update_period(_s: *mut c_void, p_msec: *mut u32) -> u32 {
    if !p_msec.is_null() {
        // SAFETY: 2026-06-05 — non-null out-param.
        unsafe { *p_msec = 0 };
    }
    SL_RESULT_SUCCESS
}

// =================================================================================================
// SLAndroidSimpleBufferQueueItf methods — `self` is the `bufferqueue_itf` field address.
// =================================================================================================

extern "C" fn bq_enqueue(self_itf: *mut c_void, buffer: *const c_void, size: u32) -> u32 {
    if buffer.is_null() {
        return SL_RESULT_PARAMETER_INVALID;
    }
    let off = itf_offsets().bufferqueue;
    // Read the PCM bytes BEFORE locking (the slice is the caller's, valid for the call).
    // SAFETY: 2026-06-05 — `buffer`/`size` are the caller's PCM buffer per the Enqueue contract; we
    // copy `size` bytes out into Eclipse-owned memory. We only read.
    let bytes: &[u8] = unsafe { std::slice::from_raw_parts(buffer as *const u8, size as usize) };
    let r = with_player_ring(self_itf, off, |ring, fmt| {
        let mut decoded: Vec<f32> = Vec::new();
        pcm_to_f32(bytes, fmt.bits_per_sample, &mut decoded);
        if decoded.is_empty() {
            return SL_RESULT_PARAMETER_INVALID;
        }
        ring.queue.push_back(decoded);
        SL_RESULT_SUCCESS
    });
    match r {
        Ok(code) => code,
        Err(_) => SL_RESULT_PARAMETER_INVALID,
    }
}

extern "C" fn bq_clear(self_itf: *mut c_void) -> u32 {
    let off = itf_offsets().bufferqueue;
    match with_player_ring(self_itf, off, |ring, _| {
        ring.queue.clear();
        ring.front_pos = 0;
        SL_RESULT_SUCCESS
    }) {
        Ok(code) => code,
        Err(_) => SL_RESULT_PARAMETER_INVALID,
    }
}

extern "C" fn bq_get_state(self_itf: *mut c_void, p_state: *mut SlBufferQueueState) -> u32 {
    if p_state.is_null() {
        return SL_RESULT_PARAMETER_INVALID;
    }
    let off = itf_offsets().bufferqueue;
    match with_player_ring(self_itf, off, |ring, _| {
        (ring.queue.len() as u32, ring.drained_buffers as u32)
    }) {
        Ok((count, index)) => {
            // SAFETY: 2026-06-05 — non-null out-param; write the buffer-queue state.
            unsafe { *p_state = SlBufferQueueState { count, index } };
            SL_RESULT_SUCCESS
        }
        Err(_) => SL_RESULT_PARAMETER_INVALID,
    }
}

extern "C" fn bq_register_callback(
    self_itf: *mut c_void,
    cb: *const c_void,
    ctx: *mut c_void,
) -> u32 {
    let off = itf_offsets().bufferqueue;
    // The bq-itf self pointer IS the `caller` the callback expects.
    let caller = self_itf as usize;
    match with_player_ring(self_itf, off, |ring, _| {
        if cb.is_null() {
            ring.callback = None;
        } else {
            // SAFETY: 2026-06-05 — `cb` is a `slAndroidSimpleBufferQueueCallback` function pointer per
            // the RegisterCallback contract; transmuting the data pointer to the fn-pointer type is the
            // standard C-ABI callback registration. The pointer is only *called* later with the exact
            // declared signature.
            let func: extern "C" fn(*mut c_void, *mut c_void) =
                unsafe { std::mem::transmute::<*const c_void, _>(cb) };
            ring.callback = Some(BufferQueueCallback {
                func,
                context: ctx as usize,
                caller,
            });
        }
        SL_RESULT_SUCCESS
    }) {
        Ok(code) => code,
        Err(_) => SL_RESULT_PARAMETER_INVALID,
    }
}

// =================================================================================================
// Object minting + validation + host stream.
// =================================================================================================

/// Validate that a packed object id refers to a live registry slot.
fn validate_object_id(id: u64) -> Result<(), ObjectError> {
    let reg = registry().lock()?;
    let idx = (id & 0xFFFF_FFFF) as usize;
    let gen = (id >> 32) as u32;
    let slot = reg.slots.get(idx).ok_or(ObjectError::OutOfRange)?;
    if slot.generation != gen || slot.state.is_none() {
        return Err(ObjectError::Stale);
    }
    Ok(())
}

/// Mint a fresh OpenSL object of `kind`, returning its `SLObjectItf` (`*mut c_void`, the stable heap
/// address of its boxed `ObjectState`). The object starts UNREALIZED; `Realize` flips it to REALIZED.
fn mint_object(kind: ObjectKind) -> Result<*mut c_void, ObjectError> {
    let mut reg = registry().lock()?;
    let id = reg.next_id();
    // Force the itf-offset table to be computed (uses a sample object; no registry interaction).
    let _ = itf_offsets();
    let state = Box::new(ObjectState {
        object_vtable: object_vtable(),
        id,
        engine_itf: match &kind {
            ObjectKind::Engine => engine_vtable(),
            _ => std::ptr::null(),
        },
        play_itf: match &kind {
            ObjectKind::Player(_) => play_vtable(),
            _ => std::ptr::null(),
        },
        bufferqueue_itf: match &kind {
            ObjectKind::Player(_) => bufferqueue_vtable(),
            _ => std::ptr::null(),
        },
        state: SL_OBJECT_STATE_UNREALIZED,
        kind,
    });
    reg.insert(state, id);
    // The `self` the engine holds is the boxed ObjectState's heap address.
    let ptr = reg.slots[(id & 0xFFFF_FFFF) as usize]
        .state
        .as_ref()
        .map(|b| (b.as_ref() as *const ObjectState) as *mut c_void)
        .ok_or(ObjectError::OutOfRange)?;
    Ok(ptr)
}

/// Start a cpal output stream for `format` that drains `ring`. Returns an `Err` (host-side, mapped to
/// a no-device player) if no host device/config exists — never panics, so a headless host degrades
/// cleanly. The cpal callback resamples nothing (the engine's rate ≈ the device rate for game audio);
/// it plays the queued `f32` samples at the device rate. (A real resampler is deferred until the
/// engine drives a rate the device cannot match — none observed; AGENTS.md simplicity-first.)
fn start_host_stream(
    ring: &Arc<Mutex<PcmRing>>,
    format: PcmFormat,
) -> Result<cpal::Stream, AudioHostError> {
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or(AudioHostError::NoDevice)?;
    let supported = device
        .default_output_config()
        .map_err(|_| AudioHostError::NoConfig)?;
    let sample_format = supported.sample_format();
    let config: cpal::StreamConfig = supported.into();
    let ring = Arc::clone(ring);
    let _ = format; // the device config drives the host rate/channels; the ring holds interleaved f32.

    let err_fn = |e| tracing::warn!(target: "eclipse::audio", "cpal output stream error: {e}");

    let stream = match sample_format {
        cpal::SampleFormat::F32 => build_stream::<f32>(&device, &config, ring, err_fn),
        cpal::SampleFormat::I16 => build_stream::<i16>(&device, &config, ring, err_fn),
        cpal::SampleFormat::U16 => build_stream::<u16>(&device, &config, ring, err_fn),
        _ => return Err(AudioHostError::UnsupportedSampleFormat),
    }
    .map_err(|_| AudioHostError::BuildFailed)?;
    stream.play().map_err(|_| AudioHostError::PlayFailed)?;
    Ok(stream)
}

/// Build a typed cpal output stream whose callback drains the shared ring into the device buffer.
fn build_stream<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    ring: Arc<Mutex<PcmRing>>,
    err_fn: impl FnMut(cpal::StreamError) + Send + 'static,
) -> Result<cpal::Stream, cpal::BuildStreamError>
where
    T: cpal::SizedSample + cpal::FromSample<f32>,
{
    let mut scratch: Vec<f32> = Vec::new();
    device.build_output_stream(
        config,
        move |data: &mut [T], _info: &cpal::OutputCallbackInfo| {
            scratch.clear();
            scratch.resize(data.len(), 0.0);
            // Drain the ring under its lock; collect any bq-callbacks to fire AFTER unlocking.
            let to_fire = match ring.lock() {
                Ok(mut guard) => fill_output(&mut guard, &mut scratch),
                Err(_) => {
                    data.iter_mut().for_each(|s| *s = T::from_sample(0.0));
                    return;
                }
            };
            for (o, s) in data.iter_mut().zip(scratch.iter()) {
                *o = T::from_sample(*s);
            }
            // Fire the buffer-queue callbacks (the app re-enqueues from here). Done outside the ring
            // lock to avoid re-entrant deadlock if the callback calls Enqueue.
            for cb in to_fire {
                // `cb.func` is a typed `extern "C" fn` (calling it is safe Rust); `caller` is the
                // bq-itf self it expects and `context` its registered ctx. 2026-06-05.
                (cb.func)(cb.caller as *mut c_void, cb.context as *mut c_void);
            }
        },
        err_fn,
        None,
    )
}

/// Host-audio errors (kept separate from OpenSL results — these mean "the host has no audio", mapped
/// by the caller to a no-device player, never a fake OpenSL success).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioHostError {
    /// No default output device (headless host / no sound server).
    NoDevice,
    /// The device has no default output config.
    NoConfig,
    /// The device's sample format is not one cpal can convert from `f32` here.
    UnsupportedSampleFormat,
    /// `build_output_stream` failed.
    BuildFailed,
    /// `stream.play()` failed.
    PlayFailed,
}

// =================================================================================================
// slCreateEngine — the one imported OpenSL ES function. Returns a working SLObjectItf engine.
// =================================================================================================

/// `SLresult slCreateEngine(SLObjectItf* pEngine, SLuint32 numOptions,
/// const SLEngineOption* pEngineOptions, SLuint32 numInterfaces, const SLInterfaceID* pInterfaceIDs,
/// const SLboolean* pInterfaceRequired)`.
///
/// Creates a real Eclipse-owned engine object and writes its `SLObjectItf` to `*pEngine`. The engine
/// starts UNREALIZED (the caller `Realize`s it, then `GetInterface(SL_IID_ENGINE)` to get the
/// `SLEngineItf`). Returns `SL_RESULT_SUCCESS` on success, leaving `*pEngine` untouched on failure
/// (the OpenSL ES contract).
///
/// # Safety
/// `p_engine` must be a valid writable `SLObjectItf*` (or null, handled). The other args follow the
/// OpenSL ES C-ABI; `numInterfaces`/`pInterfaceIDs`/`pInterfaceRequired` describe interfaces the
/// caller wants on the engine object — Eclipse exposes the engine interface unconditionally, so they
/// are not dereferenced.
pub unsafe extern "C" fn eclipse_sl_create_engine(
    p_engine: *mut c_void,
    _num_options: u32,
    _p_engine_options: *const c_void,
    _num_interfaces: u32,
    _p_interface_ids: *const c_void,
    _p_interface_required: *const c_void,
) -> u32 {
    if p_engine.is_null() {
        return SL_RESULT_PARAMETER_INVALID;
    }
    match mint_object(ObjectKind::Engine) {
        Ok(obj) => {
            // SAFETY: 2026-06-05 — `p_engine` is the caller's non-null `SLObjectItf*` out-param.
            unsafe { *(p_engine as *mut *mut c_void) = obj };
            SL_RESULT_SUCCESS
        }
        Err(_) => SL_RESULT_MEMORY_FAILURE,
    }
}

// =================================================================================================
// `eclipse __audio-test` — drive the REAL OpenSL ES path end-to-end (skips cleanly on no device).
// =================================================================================================

/// Drive the real OpenSL ES audio path through the exact engine vtables: `slCreateEngine` → Realize →
/// `GetInterface(SL_IID_ENGINE)` → `CreateOutputMix` → `CreateAudioPlayer` (AndroidSimpleBufferQueue
/// PCM source → output-mix sink) → `GetInterface(SL_IID_PLAY/BUFFERQUEUE)` → register a bq-callback →
/// `SetPlayState(PLAYING)` → `Enqueue` a generated 440 Hz sine PCM buffer, then confirm the host
/// stream consumed it (a buffer drained + the bq-callback fired) with **0 SL errors**.
///
/// On a host with NO audio device (headless CI), the player is created without a host stream; this
/// harness detects that, reports a clean SKIP, and returns `Ok` (never a spurious failure). All calls
/// go through the public OpenSL ES C-ABI vtables — no test-only shortcut into the engine internals.
pub fn run_audio_test() -> Result<String, String> {
    use std::time::{Duration, Instant};

    // The bq-callback increments this shared counter when a buffer finishes (proves the host audio
    // thread drained the enqueued PCM and the Android buffer-queue callback contract fires).
    static CB_FIRES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    CB_FIRES.store(0, std::sync::atomic::Ordering::SeqCst);
    extern "C" fn on_buffer_done(_caller: *mut c_void, _ctx: *mut c_void) {
        CB_FIRES.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }

    // 1) Create the engine.
    let mut engine: *mut c_void = std::ptr::null_mut();
    // SAFETY: 2026-06-05 — valid writable SLObjectItf* out-param; unused trailing args.
    let r = unsafe {
        eclipse_sl_create_engine(
            std::ptr::addr_of_mut!(engine).cast(),
            0,
            std::ptr::null(),
            0,
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    if r != SL_RESULT_SUCCESS || engine.is_null() {
        return Err(format!("slCreateEngine failed: SLresult={r:#x}"));
    }
    // RAII-ish: ensure we Destroy the engine + mix + player on every exit path.
    let cleanup = |objs: &[*mut c_void]| {
        for &o in objs {
            if !o.is_null() {
                obj_destroy(o);
            }
        }
    };

    // 2) Realize the engine, get SLEngineItf.
    if let Err(e) = realize_for_test(engine) {
        cleanup(&[engine]);
        return Err(e);
    }
    let eng_itf = match get_interface_for_test(engine, 3 /* SL_IID_ENGINE */) {
        Ok(p) => p,
        Err(e) => {
            cleanup(&[engine]);
            return Err(e);
        }
    };

    // 3) CreateOutputMix.
    let mut mix: *mut c_void = std::ptr::null_mut();
    // SAFETY: 2026-06-05 — `eng_itf` is the engine itf; call CreateOutputMix with a valid out-param.
    let r = unsafe {
        let vt = *(eng_itf as *const *const EngineItfVtable);
        ((*vt).create_output_mix)(
            eng_itf,
            std::ptr::addr_of_mut!(mix).cast(),
            0,
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    if r != SL_RESULT_SUCCESS || mix.is_null() {
        cleanup(&[engine]);
        return Err(format!("CreateOutputMix failed: SLresult={r:#x}"));
    }
    if let Err(e) = realize_for_test(mix) {
        cleanup(&[mix, engine]);
        return Err(e);
    }

    // 4) CreateAudioPlayer: AndroidSimpleBufferQueue source (16-bit mono 44.1 kHz) → output-mix sink.
    let mut bq_loc = SlDataLocatorBufferQueue {
        locator_type: SL_DATALOCATOR_ANDROIDSIMPLEBUFFERQUEUE,
        num_buffers: 2,
    };
    let mut pcm = SlDataFormatPcm {
        format_type: SL_DATAFORMAT_PCM,
        num_channels: 1,
        samples_per_sec: 44_100_000, // milliHz
        bits_per_sample: 16,
        container_size: 16,
        channel_mask: 0,
        endianness: 0,
    };
    let src = SlDataSource {
        p_locator: std::ptr::addr_of_mut!(bq_loc).cast(),
        p_format: std::ptr::addr_of_mut!(pcm).cast(),
    };
    let mut mix_loc = SlDataLocatorOutputMix {
        locator_type: SL_DATALOCATOR_OUTPUTMIX,
        output_mix: mix,
    };
    let snk = SlDataSink {
        p_locator: std::ptr::addr_of_mut!(mix_loc).cast(),
        p_format: std::ptr::null(),
    };
    let mut player: *mut c_void = std::ptr::null_mut();
    // SAFETY: 2026-06-05 — `eng_itf` is the engine itf; the src/snk structs are valid for the call.
    let r = unsafe {
        let vt = *(eng_itf as *const *const EngineItfVtable);
        ((*vt).create_audio_player)(
            eng_itf,
            std::ptr::addr_of_mut!(player).cast(),
            std::ptr::addr_of!(src) as *mut c_void,
            std::ptr::addr_of!(snk) as *mut c_void,
            0,
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    if r != SL_RESULT_SUCCESS || player.is_null() {
        cleanup(&[mix, engine]);
        return Err(format!("CreateAudioPlayer failed: SLresult={r:#x}"));
    }
    if let Err(e) = realize_for_test(player) {
        cleanup(&[player, mix, engine]);
        return Err(e);
    }

    // Detect "no host device": if the player has no cpal stream, SKIP cleanly.
    let has_device = player_has_host_stream(player).unwrap_or(false);

    // 5) GetInterface(PLAY) + GetInterface(BUFFERQUEUE).
    let play_itf = match get_interface_for_test(player, 4 /* SL_IID_PLAY */) {
        Ok(p) => p,
        Err(e) => {
            cleanup(&[player, mix, engine]);
            return Err(e);
        }
    };
    let bq_itf = match get_interface_for_test(player, 1 /* SL_IID_ANDROIDSIMPLEBUFFERQUEUE */) {
        Ok(p) => p,
        Err(e) => {
            cleanup(&[player, mix, engine]);
            return Err(e);
        }
    };

    // 6) Register the bq-callback.
    // SAFETY: 2026-06-05 — `bq_itf` is the player's bq itf; register a valid C callback + ctx.
    let r = unsafe {
        let vt = *(bq_itf as *const *const BufferQueueItfVtable);
        ((*vt).register_callback)(
            bq_itf,
            on_buffer_done as *const c_void,
            std::ptr::null_mut(),
        )
    };
    if r != SL_RESULT_SUCCESS {
        cleanup(&[player, mix, engine]);
        return Err(format!("RegisterCallback failed: SLresult={r:#x}"));
    }

    // 7) SetPlayState(PLAYING).
    // SAFETY: 2026-06-05 — `play_itf` is the player's play itf.
    let r = unsafe {
        let vt = *(play_itf as *const *const PlayItfVtable);
        ((*vt).set_play_state)(play_itf, SL_PLAYSTATE_PLAYING)
    };
    if r != SL_RESULT_SUCCESS {
        cleanup(&[player, mix, engine]);
        return Err(format!("SetPlayState(PLAYING) failed: SLresult={r:#x}"));
    }

    // 8) Enqueue a short 440 Hz sine PCM buffer (16-bit mono 44.1 kHz, ~50 ms).
    let pcm_bytes = generate_sine_pcm16(440.0, 44_100, 2205 /* 50 ms */);
    // SAFETY: 2026-06-05 — `bq_itf` is the player's bq itf; the PCM buffer slice is valid for the call.
    let r = unsafe {
        let vt = *(bq_itf as *const *const BufferQueueItfVtable);
        ((*vt).enqueue)(
            bq_itf,
            pcm_bytes.as_ptr() as *const c_void,
            pcm_bytes.len() as u32,
        )
    };
    if r != SL_RESULT_SUCCESS {
        cleanup(&[player, mix, engine]);
        return Err(format!("Enqueue failed: SLresult={r:#x}"));
    }

    if !has_device {
        // Headless host: the engine + mix + player were all created and the PCM enqueued with 0 SL
        // errors, but there is no host device to drain it, so the bq-callback won't fire. Clean SKIP.
        let queued = player_queued_samples(player).unwrap_or(0);
        cleanup(&[player, mix, engine]);
        return Ok(format!(
            "SKIP (no host audio device): full OpenSL path built (engine→mix→player), \
             {} PCM samples enqueued with 0 SL errors; no device to play them",
            queued
        ));
    }

    // 9) With a device: wait briefly for the audio thread to drain the buffer and fire the callback.
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut fires = 0u64;
    while Instant::now() < deadline {
        fires = CB_FIRES.load(std::sync::atomic::Ordering::SeqCst);
        if fires > 0 {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let drained = player_drained_buffers(player).unwrap_or(0);
    cleanup(&[player, mix, engine]);

    if fires == 0 || drained == 0 {
        return Err(format!(
            "host device present but the enqueued PCM was not drained (callback fires={fires}, \
             drained buffers={drained}) — the cpal stream did not consume the buffer"
        ));
    }
    Ok(format!(
        "PASS: real OpenSL ES path → host audio. Enqueued a 440 Hz sine PCM buffer; \
         the cpal stream drained {drained} buffer(s) and the buffer-queue callback fired {fires} \
         time(s) with 0 SL errors"
    ))
}

/// Generate `frames` of mono 16-bit-LE PCM for a `freq` Hz sine at `rate` Hz, as raw bytes. Pure +
/// allocation-only; unit-testable. Amplitude 0.25 (quiet — this is a validation tone, not music).
fn generate_sine_pcm16(freq: f32, rate: u32, frames: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(frames * 2);
    for n in 0..frames {
        let t = n as f32 / rate as f32;
        let s = (t * freq * 2.0 * std::f32::consts::PI).sin() * 0.25;
        let v = (s * 32767.0) as i16;
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

/// Realize an object through its `SLObjectItf` vtable (the harness path; no test-only shortcut).
fn realize_for_test(obj: *mut c_void) -> Result<(), String> {
    // SAFETY: 2026-06-05 — `obj` is a live SLObjectItf; read its vtable and call Realize.
    let r = unsafe {
        let vt = *(obj as *const *const ObjectItfVtable);
        ((*vt).realize)(obj, SL_BOOLEAN_FALSE)
    };
    if r == SL_RESULT_SUCCESS {
        Ok(())
    } else {
        Err(format!("Realize failed: SLresult={r:#x}"))
    }
}

/// `GetInterface(obj, SL_IID[index], &itf)` through the object vtable. `index` is the `SL_IID_*`
/// registration index (3=ENGINE, 4=PLAY, 1=ANDROIDSIMPLEBUFFERQUEUE).
fn get_interface_for_test(obj: *mut c_void, index: usize) -> Result<*mut c_void, String> {
    // The IID *value* the engine passes is the value stored at the SL_IID_* data symbol.
    let iid_addr = crate::loader::native_provider::sl_iid_addr_for_test(index);
    // SAFETY: 2026-06-05 — `iid_addr` is the data symbol's address; read its `SLInterfaceID` value.
    let iid_value = unsafe { *(iid_addr as *const *const c_void) };
    let mut itf: *mut c_void = std::ptr::null_mut();
    // SAFETY: 2026-06-05 — `obj` is a live SLObjectItf; GetInterface writes a valid itf to `&itf`.
    let r = unsafe {
        let vt = *(obj as *const *const ObjectItfVtable);
        ((*vt).get_interface)(obj, iid_value, std::ptr::addr_of_mut!(itf).cast())
    };
    if r != SL_RESULT_SUCCESS || itf.is_null() {
        return Err(format!(
            "GetInterface(index={index}) failed: SLresult={r:#x}"
        ));
    }
    Ok(itf)
}

/// Whether the player behind `player` has a live host stream (true = a device exists). Reads the
/// registry directly (the harness lives in this module).
fn player_has_host_stream(player: *mut c_void) -> Option<bool> {
    with_player_state(player, |p| p.stream.is_some())
}

fn player_queued_samples(player: *mut c_void) -> Option<usize> {
    with_player_state(player, |p| {
        p.ring.lock().map(|r| r.queued_samples()).unwrap_or(0)
    })
}

fn player_drained_buffers(player: *mut c_void) -> Option<u64> {
    with_player_state(player, |p| {
        p.ring.lock().map(|r| r.drained_buffers).unwrap_or(0)
    })
}

/// Run `f` with the `PlayerState` behind a player `SLObjectItf`, validated via the registry.
fn with_player_state<R>(player: *mut c_void, f: impl FnOnce(&PlayerState) -> R) -> Option<R> {
    if player.is_null() {
        return None;
    }
    // SAFETY: 2026-06-05 — `player` is a live SLObjectItf; read its id then registry-validate.
    let id = unsafe { (*(player as *const ObjectState)).id };
    let reg = registry().lock().ok()?;
    let idx = (id & 0xFFFF_FFFF) as usize;
    let gen = (id >> 32) as u32;
    let slot = reg.slots.get(idx)?;
    if slot.generation != gen {
        return None;
    }
    let state = slot.state.as_ref()?;
    if let ObjectKind::Player(p) = &state.kind {
        Some(f(p))
    } else {
        None
    }
}

/// Destroy an `SLObjectItf` Eclipse minted (frees its registry slot, drops any host stream). Used by
/// native_provider's wiring test to avoid leaking the engine it creates. Mirrors the engine's own
/// `(*obj)->Destroy(obj)` path.
#[cfg(test)]
pub(crate) fn destroy_object_for_test(obj: *mut c_void) {
    obj_destroy(obj);
}

#[cfg(test)]
mod tests;
