use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock, PoisonError};

use super::mediacodec::AMEDIA_ERROR_UNSUPPORTED;

mod ndk_surface;

pub const MAX_STAND_INS: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Answer {
    Zero,
    MinusOne,
    MediaUnsupported,
    FloatZero,
}

impl Answer {
    fn of(function: &str) -> Option<Self> {
        ndk_surface::FUNCTIONS
            .binary_search_by_key(&function, |(known, _)| known)
            .ok()
            .map(|index| ndk_surface::FUNCTIONS[index].1)
    }

    fn integer(self) -> isize {
        match self {
            Self::Zero | Self::FloatZero => 0,
            Self::MinusOne => -1,
            Self::MediaUnsupported => AMEDIA_ERROR_UNSUPPORTED as isize,
        }
    }
}

struct StandIn {
    function: String,
    answer: Option<Answer>,
    called: AtomicBool,
}

static STAND_INS: [OnceLock<StandIn>; MAX_STAND_INS] = [const { OnceLock::new() }; MAX_STAND_INS];

static BINDING: Mutex<()> = Mutex::new(());

pub fn for_function(function: &str) -> Option<u64> {
    let _binding = BINDING.lock().unwrap_or_else(PoisonError::into_inner);
    STAND_INS.iter().enumerate().find_map(|(slot, stand_in)| {
        let bound = stand_in.get_or_init(|| StandIn {
            function: function.to_owned(),
            answer: Answer::of(function),
            called: AtomicBool::new(false),
        });
        (bound.function == function).then(|| entry(slot, bound.answer))
    })
}

pub fn bound() -> Vec<(String, Option<Answer>)> {
    STAND_INS
        .iter()
        .filter_map(OnceLock::get)
        .map(|stand_in| (stand_in.function.clone(), stand_in.answer))
        .collect()
}

fn entry(slot: usize, answer: Option<Answer>) -> u64 {
    match answer {
        Some(Answer::FloatZero) => FLOAT_ENTRIES[slot] as usize as u64,
        Some(Answer::Zero | Answer::MinusOne | Answer::MediaUnsupported) | None => {
            INTEGER_ENTRIES[slot] as usize as u64
        }
    }
}

fn called(slot: usize) -> Answer {
    let stand_in = STAND_INS[slot]
        .get()
        .expect("a stand-in is bound before its address is handed out");
    let Some(answer) = stand_in.answer else {
        crate::diagnostics::record_status(
            tracing::Level::ERROR,
            &format!(
                "Roblox called {}, which this Eclipse does not provide",
                stand_in.function
            ),
        );
        std::process::abort();
    };
    if !stand_in.called.swap(true, Ordering::Relaxed) {
        tracing::warn!(
            target: "eclipse::loader",
            function = %stand_in.function,
            "Roblox called an Android function Eclipse does not implement; it answers that this \
             device does not support it"
        );
    }
    answer
}

extern "C" fn integer_entry<const SLOT: usize>() -> isize {
    called(SLOT).integer()
}

extern "C" fn float_entry<const SLOT: usize>() -> f64 {
    called(SLOT);
    0.0
}

macro_rules! entries {
    ($entry:ident) => {
        entries!($entry; 0 8 16 24 32 40 48 56)
    };
    ($entry:ident; $($row:literal)*) => {
        [$(
            $entry::<{ $row }>,
            $entry::<{ $row + 1 }>,
            $entry::<{ $row + 2 }>,
            $entry::<{ $row + 3 }>,
            $entry::<{ $row + 4 }>,
            $entry::<{ $row + 5 }>,
            $entry::<{ $row + 6 }>,
            $entry::<{ $row + 7 }>,
        )*]
    };
}

static INTEGER_ENTRIES: [extern "C" fn() -> isize; MAX_STAND_INS] = entries!(integer_entry);

static FLOAT_ENTRIES: [extern "C" fn() -> f64; MAX_STAND_INS] = entries!(float_entry);

#[cfg(test)]
mod tests;
