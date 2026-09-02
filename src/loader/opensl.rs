use std::collections::VecDeque;
use std::ffi::{c_void, CStr};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

pub const SL_RESULT_SUCCESS: u32 = 0x0000_0000;

pub const SL_RESULT_PARAMETER_INVALID: u32 = 0x0000_0002;

pub const SL_RESULT_MEMORY_FAILURE: u32 = 0x0000_0003;

pub const SL_RESULT_BUFFER_INSUFFICIENT: u32 = 0x0000_0007;

pub const SL_RESULT_FEATURE_UNSUPPORTED: u32 = 0x0000_000C;

pub const SL_RESULT_PRECONDITIONS_VIOLATED: u32 = 0x0000_0001;

const SL_OBJECT_STATE_UNREALIZED: u32 = 0x0000_0001;
const SL_OBJECT_STATE_REALIZED: u32 = 0x0000_0002;

const SL_PLAYSTATE_STOPPED: u32 = 0x0000_0001;
const SL_PLAYSTATE_PAUSED: u32 = 0x0000_0002;
const SL_PLAYSTATE_PLAYING: u32 = 0x0000_0003;

const SL_BOOLEAN_FALSE: u32 = 0x0000_0000;
const SL_BOOLEAN_TRUE: u32 = 0x0000_0001;

const SL_ANDROID_STREAM_MEDIA: i32 = 3;
const SL_ANDROID_PERFORMANCE_LATENCY: u32 = 1;

const SL_DATALOCATOR_ANDROIDSIMPLEBUFFERQUEUE: u32 = 0x8000_07BD;

const SL_DATALOCATOR_BUFFERQUEUE: u32 = 0x0000_0006;

const SL_DATALOCATOR_OUTPUTMIX: u32 = 0x0000_0004;

const SL_DATAFORMAT_PCM: u32 = 0x0000_0002;

#[repr(C)]
struct SlDataLocatorBufferQueue {
    locator_type: u32,
    num_buffers: u32,
}

#[repr(C)]
struct SlDataLocatorOutputMix {
    locator_type: u32,
    output_mix: *mut c_void,
}

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

#[repr(C)]
struct SlDataSource {
    p_locator: *const c_void,
    p_format: *const c_void,
}

#[repr(C)]
struct SlDataSink {
    p_locator: *const c_void,
    p_format: *const c_void,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PcmFormat {
    pub channels: u32,

    pub sample_rate: u32,

    pub bits_per_sample: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormatError {
    NotPcm,

    BadChannels,

    BadSampleRate,

    BadBitsPerSample,
}

impl PcmFormat {
    pub fn from_sl_pcm(
        channels: u32,
        samples_per_sec_millihz: u32,
        bits_per_sample: u32,
    ) -> Result<Self, FormatError> {
        if channels == 0 || channels > 2 {
            return Err(FormatError::BadChannels);
        }

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

pub fn pcm_to_f32(bytes: &[u8], bits: u32, out: &mut Vec<f32>) {
    match bits {
        16 => {
            for frame in bytes.as_chunks::<2>().0 {
                let s = i16::from_le_bytes([frame[0], frame[1]]);
                out.push(s as f32 / 32768.0);
            }
        }
        8 => {
            for &b in bytes {
                out.push((b as f32 - 128.0) / 128.0);
            }
        }

        _ => {}
    }
}

#[derive(Clone, Copy)]
struct BufferQueueCallback {
    func: extern "C" fn(*mut c_void, *mut c_void),

    context: usize,

    caller: usize,
}

unsafe impl Send for BufferQueueCallback {}

struct PcmRing {
    queue: VecDeque<Vec<f32>>,

    front_pos: usize,

    play_state: u32,

    callback: Option<BufferQueueCallback>,

    drained_buffers: u64,

    callback_fires: u64,

    volume_level_mb: i16,

    muted: bool,

    stereo_position_enabled: bool,
    stereo_position: i16,
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
            volume_level_mb: 0,
            muted: false,
            stereo_position_enabled: false,
            stereo_position: 0,
        }
    }

    fn queued_samples(&self) -> usize {
        let total: usize = self.queue.iter().map(Vec::len).sum();
        total - self.front_pos
    }
}

fn fill_output(ring: &mut PcmRing, out: &mut [f32]) -> Vec<BufferQueueCallback> {
    let mut to_fire: Vec<BufferQueueCallback> = Vec::new();
    if ring.play_state != SL_PLAYSTATE_PLAYING {
        out.fill(0.0);
        return to_fire;
    }
    let mut written = 0usize;
    while written < out.len() {
        let Some(front) = ring.queue.front() else {
            out[written..].fill(0.0);
            break;
        };
        if ring.front_pos >= front.len() {
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
    let gain = if ring.muted {
        0.0
    } else {
        10.0_f32.powf(f32::from(ring.volume_level_mb) / 2000.0)
    };
    if gain != 1.0 {
        out.iter_mut().for_each(|sample| *sample *= gain);
    }
    to_fire
}

enum ObjectKind {
    Engine,

    OutputMix,

    Player(Box<PlayerState>),
}

struct PlayerState {
    format: PcmFormat,
    ring: Arc<Mutex<PcmRing>>,

    stream: Option<cpal::Stream>,

    stream_type: i32,
    performance_mode: u32,
}

#[repr(C)]
struct ObjectState {
    object_vtable: *const ObjectItfVtable,

    id: u64,

    engine_itf: *const EngineItfVtable,
    play_itf: *const PlayItfVtable,
    bufferqueue_itf: *const BufferQueueItfVtable,
    volume_itf: *const VolumeItfVtable,
    android_config_itf: *const AndroidConfigurationItfVtable,

    state: u32,

    kind: ObjectKind,
}

unsafe impl Send for ObjectState {}

struct ObjectSlot {
    generation: u32,
    state: Option<Box<ObjectState>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectError {
    OutOfRange,

    Stale,

    Poisoned,
}

impl<T> From<PoisonError<T>> for ObjectError {
    fn from(_: PoisonError<T>) -> Self {
        ObjectError::Poisoned
    }
}

#[derive(Default)]
struct ObjectRegistry {
    slots: Vec<ObjectSlot>,
}

fn registry() -> &'static Mutex<ObjectRegistry> {
    static REG: OnceLock<Mutex<ObjectRegistry>> = OnceLock::new();
    REG.get_or_init(|| Mutex::new(ObjectRegistry::default()))
}

impl ObjectRegistry {
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

    fn next_id(&mut self) -> u64 {
        if let Some(idx) = self.slots.iter().position(|s| s.state.is_none()) {
            let gen = self.slots[idx].generation;
            return ((gen as u64) << 32) | idx as u64;
        }
        let idx = self.slots.len() as u64;

        (1u64 << 32) | idx
    }
}

type SlObjectItf = *mut c_void;

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

#[repr(C)]
struct BufferQueueItfVtable {
    enqueue: extern "C" fn(*mut c_void, *const c_void, u32) -> u32,
    clear: extern "C" fn(*mut c_void) -> u32,
    get_state: extern "C" fn(*mut c_void, *mut SlBufferQueueState) -> u32,
    register_callback: extern "C" fn(*mut c_void, *const c_void, *mut c_void) -> u32,
}

#[repr(C)]
struct VolumeItfVtable {
    set_volume_level: extern "C" fn(*mut c_void, i16) -> u32,
    get_volume_level: extern "C" fn(*mut c_void, *mut i16) -> u32,
    get_max_volume_level: extern "C" fn(*mut c_void, *mut i16) -> u32,
    set_mute: extern "C" fn(*mut c_void, u32) -> u32,
    get_mute: extern "C" fn(*mut c_void, *mut u32) -> u32,
    enable_stereo_position: extern "C" fn(*mut c_void, u32) -> u32,
    is_stereo_position_enabled: extern "C" fn(*mut c_void, *mut u32) -> u32,
    set_stereo_position: extern "C" fn(*mut c_void, i16) -> u32,
    get_stereo_position: extern "C" fn(*mut c_void, *mut i16) -> u32,
}

#[repr(C)]
struct AndroidConfigurationItfVtable {
    set_configuration: extern "C" fn(*mut c_void, *const i8, *const c_void, u32) -> u32,
    get_configuration: extern "C" fn(*mut c_void, *const i8, *mut u32, *mut c_void) -> u32,
    acquire_java_proxy: extern "C" fn(*mut c_void, u32, *mut c_void) -> u32,
    release_java_proxy: extern "C" fn(*mut c_void, u32) -> u32,
}

#[repr(C)]
struct SlBufferQueueState {
    count: u32,
    index: u32,
}

unsafe fn object_from_itf_field(field_self: *mut c_void, field_offset: usize) -> *mut ObjectState {
    unsafe { (field_self as *mut u8).sub(field_offset) as *mut ObjectState }
}

static ITF_OFFSETS: OnceLock<ItfOffsets> = OnceLock::new();

#[derive(Clone, Copy)]
struct ItfOffsets {
    engine: usize,
    play: usize,
    bufferqueue: usize,
    volume: usize,
    android_config: usize,
}

fn itf_offsets() -> ItfOffsets {
    *ITF_OFFSETS.get_or_init(|| {
        let sample = ObjectState {
            object_vtable: std::ptr::null(),
            id: 0,
            engine_itf: std::ptr::null(),
            play_itf: std::ptr::null(),
            bufferqueue_itf: std::ptr::null(),
            volume_itf: std::ptr::null(),
            android_config_itf: std::ptr::null(),
            state: 0,
            kind: ObjectKind::OutputMix,
        };
        let base = std::ptr::addr_of!(sample) as usize;
        ItfOffsets {
            engine: std::ptr::addr_of!(sample.engine_itf) as usize - base,
            play: std::ptr::addr_of!(sample.play_itf) as usize - base,
            bufferqueue: std::ptr::addr_of!(sample.bufferqueue_itf) as usize - base,
            volume: std::ptr::addr_of!(sample.volume_itf) as usize - base,
            android_config: std::ptr::addr_of!(sample.android_config_itf) as usize - base,
        }
    })
}

struct VtableWrap<T: 'static>(T);

unsafe impl<T> Sync for VtableWrap<T> {}

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

fn volume_vtable() -> *const VolumeItfVtable {
    static V: OnceLock<VtableWrap<VolumeItfVtable>> = OnceLock::new();
    &V.get_or_init(|| {
        VtableWrap(VolumeItfVtable {
            set_volume_level: volume_set_level,
            get_volume_level: volume_get_level,
            get_max_volume_level: volume_get_max_level,
            set_mute: volume_set_mute,
            get_mute: volume_get_mute,
            enable_stereo_position: volume_enable_stereo_position,
            is_stereo_position_enabled: volume_is_stereo_position_enabled,
            set_stereo_position: volume_set_stereo_position,
            get_stereo_position: volume_get_stereo_position,
        })
    })
    .0
}

fn android_configuration_vtable() -> *const AndroidConfigurationItfVtable {
    static V: OnceLock<VtableWrap<AndroidConfigurationItfVtable>> = OnceLock::new();
    &V.get_or_init(|| {
        VtableWrap(AndroidConfigurationItfVtable {
            set_configuration: android_config_set,
            get_configuration: android_config_get,
            acquire_java_proxy: android_config_acquire_java_proxy,
            release_java_proxy: android_config_release_java_proxy,
        })
    })
    .0
}

fn with_object<R>(
    obj: SlObjectItf,
    f: impl FnOnce(&mut ObjectState, &mut ObjectRegistry) -> R,
) -> Result<R, ObjectError> {
    if obj.is_null() {
        return Err(ObjectError::OutOfRange);
    }
    let mut reg = registry().lock()?;

    let id = unsafe { (*(obj as *const ObjectState)).id };
    let idx = (id & 0xFFFF_FFFF) as usize;
    let gen = (id >> 32) as u32;
    let slot = reg.slots.get(idx).ok_or(ObjectError::OutOfRange)?;
    if slot.generation != gen || slot.state.is_none() {
        return Err(ObjectError::Stale);
    }

    let mut state = reg.slots[idx].state.take().expect("checked Some above");
    let r = f(&mut state, &mut reg);
    reg.slots[idx].state = Some(state);
    Ok(r)
}

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
        let which = crate::loader::native_provider::sl_iid_index(iid as usize);

        if st.state != SL_OBJECT_STATE_REALIZED && which != Some(0) {
            return Err(SL_RESULT_PRECONDITIONS_VIOLATED);
        }

        let base = (st as *mut ObjectState) as *mut u8;
        let (field_ptr, present) = match which {
            Some(0) => (
                unsafe { base.add(off.android_config) },
                !st.android_config_itf.is_null(),
            ),

            Some(3) => (unsafe { base.add(off.engine) }, !st.engine_itf.is_null()),

            Some(4) => (unsafe { base.add(off.play) }, !st.play_itf.is_null()),

            Some(1) | Some(2) => (
                unsafe { base.add(off.bufferqueue) },
                !st.bufferqueue_itf.is_null(),
            ),

            Some(6) => (unsafe { base.add(off.volume) }, !st.volume_itf.is_null()),
            _ => (std::ptr::null_mut(), false),
        };
        if !present || field_ptr.is_null() {
            return Err(SL_RESULT_PARAMETER_INVALID);
        }
        Ok(field_ptr)
    });
    match r {
        Ok(Ok(field_ptr)) => {
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
    let _ = with_object_destroy(obj);
}

fn with_object_destroy(obj: SlObjectItf) -> Result<(), ObjectError> {
    if obj.is_null() {
        return Err(ObjectError::OutOfRange);
    }
    let mut reg = registry().lock()?;

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

fn engine_self_id(self_itf: *mut c_void) -> Option<u64> {
    if self_itf.is_null() {
        return None;
    }
    let off = itf_offsets();

    let obj = unsafe { object_from_itf_field(self_itf, off.engine) };

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

    unsafe { *(p_mix as *mut *mut c_void) = obj_ptr };
    tracing::info!("OpenSL: output mix created");
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

    let src = unsafe { &*(p_audio_src as *const SlDataSource) };
    if src.p_locator.is_null() || src.p_format.is_null() {
        return SL_RESULT_PARAMETER_INVALID;
    }

    let loc_type = unsafe { *(src.p_locator as *const u32) };
    if loc_type != SL_DATALOCATOR_ANDROIDSIMPLEBUFFERQUEUE && loc_type != SL_DATALOCATOR_BUFFERQUEUE
    {
        tracing::warn!(
            locator_type = format_args!("{loc_type:#010x}"),
            "OpenSL: unsupported player source locator"
        );
        return SL_RESULT_FEATURE_UNSUPPORTED;
    }

    let bq_loc = unsafe { &*(src.p_locator as *const SlDataLocatorBufferQueue) };
    let _num_buffers = bq_loc.num_buffers;

    let fmt_type = unsafe { *(src.p_format as *const u32) };
    if fmt_type != SL_DATAFORMAT_PCM {
        tracing::warn!(
            format_type = format_args!("{fmt_type:#010x}"),
            "OpenSL: unsupported player source format"
        );
        return SL_RESULT_FEATURE_UNSUPPORTED;
    }

    let pcm = unsafe { &*(src.p_format as *const SlDataFormatPcm) };
    let format =
        match PcmFormat::from_sl_pcm(pcm.num_channels, pcm.samples_per_sec, pcm.bits_per_sample) {
            Ok(f) => f,
            Err(_) => return SL_RESULT_FEATURE_UNSUPPORTED,
        };

    let snk = unsafe { &*(p_audio_snk as *const SlDataSink) };
    if snk.p_locator.is_null() {
        return SL_RESULT_PARAMETER_INVALID;
    }

    let snk_loc_type = unsafe { *(snk.p_locator as *const u32) };
    if snk_loc_type != SL_DATALOCATOR_OUTPUTMIX {
        tracing::warn!(
            locator_type = format_args!("{snk_loc_type:#010x}"),
            "OpenSL: unsupported player sink locator"
        );
        return SL_RESULT_FEATURE_UNSUPPORTED;
    }

    let _ = unsafe { &*(snk.p_locator as *const SlDataLocatorOutputMix) };

    let ring = Arc::new(Mutex::new(PcmRing::new()));

    let stream_result = start_host_stream(&ring, format);
    let host_stream_live = stream_result.is_ok();
    if let Err(error) = stream_result {
        tracing::warn!(?error, "OpenSL: host output stream unavailable");
    }
    let stream = stream_result.ok();
    let player = Box::new(PlayerState {
        format,
        ring,
        stream,
        stream_type: SL_ANDROID_STREAM_MEDIA,
        performance_mode: SL_ANDROID_PERFORMANCE_LATENCY,
    });
    let obj_ptr = match mint_object(ObjectKind::Player(player)) {
        Ok(p) => p,
        Err(_) => return SL_RESULT_MEMORY_FAILURE,
    };

    unsafe { *(p_player as *mut *mut c_void) = obj_ptr };
    tracing::info!(
        channels = format.channels,
        sample_rate = format.sample_rate,
        bits_per_sample = format.bits_per_sample,
        host_stream_live,
        "OpenSL: audio player created"
    );
    SL_RESULT_SUCCESS
}

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
        unsafe { *p_supported = SL_BOOLEAN_FALSE };
    }
    SL_RESULT_SUCCESS
}

fn with_player_ring<R>(
    self_itf: *mut c_void,
    field_off: usize,
    f: impl FnOnce(&mut PcmRing, PcmFormat) -> R,
) -> Result<R, ObjectError> {
    if self_itf.is_null() {
        return Err(ObjectError::OutOfRange);
    }

    let obj = unsafe { object_from_itf_field(self_itf, field_off) };

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
    };
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
            unsafe { *p_state = s };
            SL_RESULT_SUCCESS
        }
        Err(_) => SL_RESULT_PARAMETER_INVALID,
    }
}

extern "C" fn play_get_duration(_s: *mut c_void, p_msec: *mut u32) -> u32 {
    if !p_msec.is_null() {
        unsafe { *p_msec = 0xFFFF_FFFF };
    }
    SL_RESULT_SUCCESS
}

extern "C" fn play_get_position(self_itf: *mut c_void, p_msec: *mut u32) -> u32 {
    if p_msec.is_null() {
        return SL_RESULT_PARAMETER_INVALID;
    }
    let off = itf_offsets().play;
    match with_player_ring(self_itf, off, |ring, _fmt| ring.drained_buffers) {
        Ok(_n) => {
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
        unsafe { *p_msec = 0 };
    }
    SL_RESULT_SUCCESS
}

extern "C" fn play_set_update_period(_s: *mut c_void, _msec: u32) -> u32 {
    SL_RESULT_SUCCESS
}

extern "C" fn play_get_update_period(_s: *mut c_void, p_msec: *mut u32) -> u32 {
    if !p_msec.is_null() {
        unsafe { *p_msec = 0 };
    }
    SL_RESULT_SUCCESS
}

extern "C" fn bq_enqueue(self_itf: *mut c_void, buffer: *const c_void, size: u32) -> u32 {
    if buffer.is_null() {
        return SL_RESULT_PARAMETER_INVALID;
    }
    let off = itf_offsets().bufferqueue;

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

    let caller = self_itf as usize;
    match with_player_ring(self_itf, off, |ring, _| {
        if cb.is_null() {
            ring.callback = None;
        } else {
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

extern "C" fn volume_set_level(self_itf: *mut c_void, level: i16) -> u32 {
    if level > 0 {
        return SL_RESULT_PARAMETER_INVALID;
    }
    match with_player_ring(self_itf, itf_offsets().volume, |ring, _| {
        ring.volume_level_mb = level;
    }) {
        Ok(()) => SL_RESULT_SUCCESS,
        Err(_) => SL_RESULT_PARAMETER_INVALID,
    }
}

extern "C" fn volume_get_level(self_itf: *mut c_void, p_level: *mut i16) -> u32 {
    if p_level.is_null() {
        return SL_RESULT_PARAMETER_INVALID;
    }
    match with_player_ring(self_itf, itf_offsets().volume, |ring, _| {
        ring.volume_level_mb
    }) {
        Ok(level) => {
            unsafe { *p_level = level };
            SL_RESULT_SUCCESS
        }
        Err(_) => SL_RESULT_PARAMETER_INVALID,
    }
}

extern "C" fn volume_get_max_level(_self_itf: *mut c_void, p_level: *mut i16) -> u32 {
    if p_level.is_null() {
        return SL_RESULT_PARAMETER_INVALID;
    }

    unsafe { *p_level = 0 };
    SL_RESULT_SUCCESS
}

extern "C" fn volume_set_mute(self_itf: *mut c_void, mute: u32) -> u32 {
    match with_player_ring(self_itf, itf_offsets().volume, |ring, _| {
        ring.muted = mute != SL_BOOLEAN_FALSE;
    }) {
        Ok(()) => SL_RESULT_SUCCESS,
        Err(_) => SL_RESULT_PARAMETER_INVALID,
    }
}

extern "C" fn volume_get_mute(self_itf: *mut c_void, p_mute: *mut u32) -> u32 {
    if p_mute.is_null() {
        return SL_RESULT_PARAMETER_INVALID;
    }
    match with_player_ring(self_itf, itf_offsets().volume, |ring, _| ring.muted) {
        Ok(muted) => {
            unsafe {
                *p_mute = if muted {
                    SL_BOOLEAN_TRUE
                } else {
                    SL_BOOLEAN_FALSE
                }
            };
            SL_RESULT_SUCCESS
        }
        Err(_) => SL_RESULT_PARAMETER_INVALID,
    }
}

extern "C" fn volume_enable_stereo_position(self_itf: *mut c_void, enable: u32) -> u32 {
    match with_player_ring(self_itf, itf_offsets().volume, |ring, _| {
        ring.stereo_position_enabled = enable != SL_BOOLEAN_FALSE;
    }) {
        Ok(()) => SL_RESULT_SUCCESS,
        Err(_) => SL_RESULT_PARAMETER_INVALID,
    }
}

extern "C" fn volume_is_stereo_position_enabled(self_itf: *mut c_void, p_enabled: *mut u32) -> u32 {
    if p_enabled.is_null() {
        return SL_RESULT_PARAMETER_INVALID;
    }
    match with_player_ring(self_itf, itf_offsets().volume, |ring, _| {
        ring.stereo_position_enabled
    }) {
        Ok(enabled) => {
            unsafe {
                *p_enabled = if enabled {
                    SL_BOOLEAN_TRUE
                } else {
                    SL_BOOLEAN_FALSE
                }
            };
            SL_RESULT_SUCCESS
        }
        Err(_) => SL_RESULT_PARAMETER_INVALID,
    }
}

extern "C" fn volume_set_stereo_position(self_itf: *mut c_void, position: i16) -> u32 {
    if !(-1000..=1000).contains(&position) {
        return SL_RESULT_PARAMETER_INVALID;
    }
    match with_player_ring(self_itf, itf_offsets().volume, |ring, _| {
        ring.stereo_position = position;
    }) {
        Ok(()) => SL_RESULT_SUCCESS,
        Err(_) => SL_RESULT_PARAMETER_INVALID,
    }
}

extern "C" fn volume_get_stereo_position(self_itf: *mut c_void, p_position: *mut i16) -> u32 {
    if p_position.is_null() {
        return SL_RESULT_PARAMETER_INVALID;
    }
    match with_player_ring(self_itf, itf_offsets().volume, |ring, _| {
        ring.stereo_position
    }) {
        Ok(position) => {
            unsafe { *p_position = position };
            SL_RESULT_SUCCESS
        }
        Err(_) => SL_RESULT_PARAMETER_INVALID,
    }
}

fn with_player_config_state<R>(
    self_itf: *mut c_void,
    field_off: usize,
    f: impl FnOnce(&mut PlayerState) -> R,
) -> Result<R, ObjectError> {
    if self_itf.is_null() {
        return Err(ObjectError::OutOfRange);
    }

    let obj = unsafe { object_from_itf_field(self_itf, field_off) };
    with_object(obj.cast(), |state, _| {
        let ObjectKind::Player(player) = &mut state.kind else {
            return Err(ObjectError::OutOfRange);
        };
        Ok(f(player))
    })?
}

fn android_config_key(config_key: *const i8) -> Result<&'static [u8], ()> {
    if config_key.is_null() {
        return Err(());
    }

    let key = unsafe { CStr::from_ptr(config_key) };
    Ok(match key.to_bytes() {
        b"androidPlaybackStreamType" => b"androidPlaybackStreamType",
        b"androidPerformanceMode" => b"androidPerformanceMode",
        _ => return Err(()),
    })
}

extern "C" fn android_config_set(
    self_itf: *mut c_void,
    config_key: *const i8,
    value: *const c_void,
    value_size: u32,
) -> u32 {
    let Ok(key) = android_config_key(config_key) else {
        return SL_RESULT_PARAMETER_INVALID;
    };
    if value.is_null() || value_size < std::mem::size_of::<u32>() as u32 {
        return SL_RESULT_PARAMETER_INVALID;
    }

    let raw = unsafe { std::ptr::read_unaligned(value.cast::<u32>()) };
    with_player_config_state(self_itf, itf_offsets().android_config, |player| match key {
        b"androidPlaybackStreamType" if raw <= 5 => {
            player.stream_type = raw as i32;
            SL_RESULT_SUCCESS
        }
        b"androidPerformanceMode" if raw <= 3 => {
            player.performance_mode = raw;
            SL_RESULT_SUCCESS
        }
        _ => SL_RESULT_PARAMETER_INVALID,
    })
    .unwrap_or(SL_RESULT_PARAMETER_INVALID)
}

extern "C" fn android_config_get(
    self_itf: *mut c_void,
    config_key: *const i8,
    value_size: *mut u32,
    value: *mut c_void,
) -> u32 {
    let Ok(key) = android_config_key(config_key) else {
        return SL_RESULT_PARAMETER_INVALID;
    };
    if value_size.is_null() {
        return SL_RESULT_PARAMETER_INVALID;
    }

    let capacity = unsafe { *value_size };

    unsafe { *value_size = std::mem::size_of::<u32>() as u32 };
    if value.is_null() {
        return SL_RESULT_SUCCESS;
    }
    if capacity < std::mem::size_of::<u32>() as u32 {
        return SL_RESULT_BUFFER_INSUFFICIENT;
    }
    let result =
        with_player_config_state(self_itf, itf_offsets().android_config, |player| match key {
            b"androidPlaybackStreamType" => player.stream_type as u32,
            b"androidPerformanceMode" => player.performance_mode,
            _ => unreachable!("android_config_key rejects unknown keys"),
        });
    match result {
        Ok(raw) => {
            unsafe { std::ptr::write_unaligned(value.cast::<u32>(), raw) };
            SL_RESULT_SUCCESS
        }
        Err(_) => SL_RESULT_PARAMETER_INVALID,
    }
}

extern "C" fn android_config_acquire_java_proxy(
    _self_itf: *mut c_void,
    _proxy_type: u32,
    proxy: *mut c_void,
) -> u32 {
    if !proxy.is_null() {
        unsafe { *(proxy as *mut *mut c_void) = std::ptr::null_mut() };
    }
    SL_RESULT_FEATURE_UNSUPPORTED
}

extern "C" fn android_config_release_java_proxy(_self_itf: *mut c_void, _proxy_type: u32) -> u32 {
    SL_RESULT_FEATURE_UNSUPPORTED
}

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

fn mint_object(kind: ObjectKind) -> Result<*mut c_void, ObjectError> {
    let mut reg = registry().lock()?;
    let id = reg.next_id();

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
        volume_itf: match &kind {
            ObjectKind::Player(_) => volume_vtable(),
            _ => std::ptr::null(),
        },
        android_config_itf: match &kind {
            ObjectKind::Player(_) => android_configuration_vtable(),
            _ => std::ptr::null(),
        },
        state: SL_OBJECT_STATE_UNREALIZED,
        kind,
    });
    reg.insert(state, id);

    let ptr = reg.slots[(id & 0xFFFF_FFFF) as usize]
        .state
        .as_ref()
        .map(|b| (b.as_ref() as *const ObjectState) as *mut c_void)
        .ok_or(ObjectError::OutOfRange)?;
    Ok(ptr)
}

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
    let _ = format;

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

            for cb in to_fire {
                (cb.func)(cb.caller as *mut c_void, cb.context as *mut c_void);
            }
        },
        err_fn,
        None,
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioHostError {
    NoDevice,

    NoConfig,

    UnsupportedSampleFormat,

    BuildFailed,

    PlayFailed,
}

pub(crate) unsafe extern "C" fn eclipse_sl_create_engine(
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
            unsafe { *(p_engine as *mut *mut c_void) = obj };
            tracing::info!("OpenSL: engine created");
            SL_RESULT_SUCCESS
        }
        Err(_) => SL_RESULT_MEMORY_FAILURE,
    }
}

pub fn run_audio_test() -> Result<String, String> {
    use std::time::{Duration, Instant};

    static CB_FIRES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    CB_FIRES.store(0, std::sync::atomic::Ordering::SeqCst);
    extern "C" fn on_buffer_done(_caller: *mut c_void, _ctx: *mut c_void) {
        CB_FIRES.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }

    let mut engine: *mut c_void = std::ptr::null_mut();

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

    let cleanup = |objs: &[*mut c_void]| {
        for &o in objs {
            if !o.is_null() {
                obj_destroy(o);
            }
        }
    };

    if let Err(e) = realize_for_test(engine) {
        cleanup(&[engine]);
        return Err(e);
    }
    let eng_itf = match get_interface_for_test(engine, 3) {
        Ok(p) => p,
        Err(e) => {
            cleanup(&[engine]);
            return Err(e);
        }
    };

    let mut mix: *mut c_void = std::ptr::null_mut();

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

    let mut bq_loc = SlDataLocatorBufferQueue {
        locator_type: SL_DATALOCATOR_ANDROIDSIMPLEBUFFERQUEUE,
        num_buffers: 2,
    };
    let mut pcm = SlDataFormatPcm {
        format_type: SL_DATAFORMAT_PCM,
        num_channels: 1,
        samples_per_sec: 44_100_000,
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

    let has_device = player_has_host_stream(player).unwrap_or(false);

    let play_itf = match get_interface_for_test(player, 4) {
        Ok(p) => p,
        Err(e) => {
            cleanup(&[player, mix, engine]);
            return Err(e);
        }
    };
    let bq_itf = match get_interface_for_test(player, 1) {
        Ok(p) => p,
        Err(e) => {
            cleanup(&[player, mix, engine]);
            return Err(e);
        }
    };

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

    let r = unsafe {
        let vt = *(play_itf as *const *const PlayItfVtable);
        ((*vt).set_play_state)(play_itf, SL_PLAYSTATE_PLAYING)
    };
    if r != SL_RESULT_SUCCESS {
        cleanup(&[player, mix, engine]);
        return Err(format!("SetPlayState(PLAYING) failed: SLresult={r:#x}"));
    }

    let pcm_bytes = generate_sine_pcm16(440.0, 44_100, 2205);

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
        let queued = player_queued_samples(player).unwrap_or(0);
        cleanup(&[player, mix, engine]);
        return Ok(format!(
            "SKIP (no host audio device): full OpenSL path built (engine→mix→player), \
             {} PCM samples enqueued with 0 SL errors; no device to play them",
            queued
        ));
    }

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

fn realize_for_test(obj: *mut c_void) -> Result<(), String> {
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

fn get_interface_for_test(obj: *mut c_void, index: usize) -> Result<*mut c_void, String> {
    let iid_addr = crate::loader::native_provider::sl_iid_addr_for_test(index);

    let iid_value = unsafe { *(iid_addr as *const *const c_void) };
    let mut itf: *mut c_void = std::ptr::null_mut();

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

fn with_player_state<R>(player: *mut c_void, f: impl FnOnce(&PlayerState) -> R) -> Option<R> {
    if player.is_null() {
        return None;
    }

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

#[cfg(test)]
pub(crate) fn destroy_object_for_test(obj: *mut c_void) {
    obj_destroy(obj);
}

#[cfg(test)]
mod tests;
