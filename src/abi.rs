#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
compile_error!(
    "Eclipse runs Roblox's Android x86_64 or arm64-v8a client natively, so it builds only for \
     x86_64 and aarch64 hosts"
);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Abi {
    X86_64,
    Arm64V8a,
}

impl Abi {
    #[cfg(target_arch = "x86_64")]
    pub const HOST: Self = Self::X86_64;

    #[cfg(target_arch = "aarch64")]
    pub const HOST: Self = Self::Arm64V8a;

    pub const fn android_name(self) -> &'static str {
        match self {
            Self::X86_64 => "x86_64",
            Self::Arm64V8a => "arm64-v8a",
        }
    }

    pub const fn split_name(self) -> &'static str {
        match self {
            Self::X86_64 => "config.x86_64",
            Self::Arm64V8a => "config.arm64_v8a",
        }
    }

    pub const fn split_apk(self) -> &'static str {
        match self {
            Self::X86_64 => "split_config.x86_64.apk",
            Self::Arm64V8a => "split_config.arm64_v8a.apk",
        }
    }

    pub const fn xapk_split_entry(self) -> &'static str {
        match self {
            Self::X86_64 => "config.x86_64.apk",
            Self::Arm64V8a => "config.arm64_v8a.apk",
        }
    }

    pub const fn art_isa(self) -> &'static str {
        match self {
            Self::X86_64 => "x86_64",
            Self::Arm64V8a => "arm64",
        }
    }

    pub const fn gnu_triple(self) -> &'static str {
        match self {
            Self::X86_64 => "x86_64-linux-gnu",
            Self::Arm64V8a => "aarch64-linux-gnu",
        }
    }

    pub const fn elf_machine(self) -> u16 {
        match self {
            Self::X86_64 => 62,
            Self::Arm64V8a => 183,
        }
    }

    pub fn from_elf_machine(machine: u16) -> Option<Self> {
        [Self::X86_64, Self::Arm64V8a]
            .into_iter()
            .find(|abi| abi.elf_machine() == machine)
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::X86_64 => "x86-64",
            Self::Arm64V8a => "ARM64",
        }
    }

    pub const fn other(self) -> Self {
        match self {
            Self::X86_64 => Self::Arm64V8a,
            Self::Arm64V8a => Self::X86_64,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abi_accessors_name_both_abis() {
        let table = [
            (
                Abi::X86_64,
                [
                    "x86_64",
                    "config.x86_64",
                    "split_config.x86_64.apk",
                    "config.x86_64.apk",
                    "x86_64",
                    "x86_64-linux-gnu",
                    "x86-64",
                ],
                62,
                Abi::Arm64V8a,
            ),
            (
                Abi::Arm64V8a,
                [
                    "arm64-v8a",
                    "config.arm64_v8a",
                    "split_config.arm64_v8a.apk",
                    "config.arm64_v8a.apk",
                    "arm64",
                    "aarch64-linux-gnu",
                    "ARM64",
                ],
                183,
                Abi::X86_64,
            ),
        ];
        for (abi, names, machine, other) in table {
            assert_eq!(
                [
                    abi.android_name(),
                    abi.split_name(),
                    abi.split_apk(),
                    abi.xapk_split_entry(),
                    abi.art_isa(),
                    abi.gnu_triple(),
                    abi.label(),
                ],
                names,
                "{abi:?}"
            );
            assert_eq!(abi.elf_machine(), machine, "{abi:?}");
            assert_eq!(Abi::from_elf_machine(machine), Some(abi), "{abi:?}");
            assert_eq!(abi.other(), other, "{abi:?}");
        }
        assert_eq!(Abi::from_elf_machine(40), None, "EM_ARM is 32-bit Arm");
    }
}
