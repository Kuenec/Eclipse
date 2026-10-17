#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct FaultRegisters {
    pub(super) pc: u64,
    pub(super) sp: u64,
    pub(super) fp: u64,
    pub(super) extra: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RegisterValue {
    Code(u64),
    Data(u64),
}

impl FaultRegisters {
    pub(super) fn named(&self) -> [(&'static [u8], RegisterValue); 4] {
        [
            (Self::PC_NAME, RegisterValue::Code(self.pc)),
            (Self::SP_NAME, RegisterValue::Data(self.sp)),
            (Self::FP_NAME, RegisterValue::Data(self.fp)),
            (Self::EXTRA_NAME, Self::extra_value(self.extra)),
        ]
    }
}

#[cfg(target_arch = "x86_64")]
impl FaultRegisters {
    pub(super) const PC_NAME: &'static [u8] = b"rip";
    const SP_NAME: &'static [u8] = b"rsp";
    const FP_NAME: &'static [u8] = b"rbp";
    const EXTRA_NAME: &'static [u8] = b"err";

    const fn extra_value(error_code: u64) -> RegisterValue {
        RegisterValue::Data(error_code)
    }

    pub(super) fn read(context: &libc::ucontext_t) -> Self {
        let register = |index: libc::c_int| context.uc_mcontext.gregs[index as usize] as u64;
        Self {
            pc: register(libc::REG_RIP),
            sp: register(libc::REG_RSP),
            fp: register(libc::REG_RBP),
            extra: register(libc::REG_ERR),
        }
    }
}

#[cfg(target_arch = "aarch64")]
impl FaultRegisters {
    pub(super) const PC_NAME: &'static [u8] = b"pc";
    const SP_NAME: &'static [u8] = b"sp";
    const FP_NAME: &'static [u8] = b"fp";
    const EXTRA_NAME: &'static [u8] = b"lr";

    const fn extra_value(link_register: u64) -> RegisterValue {
        RegisterValue::Code(link_register)
    }

    const FRAME_POINTER: usize = 29;
    const LINK_REGISTER: usize = 30;

    pub(super) fn read(context: &libc::ucontext_t) -> Self {
        let mcontext = &context.uc_mcontext;
        Self {
            pc: mcontext.pc,
            sp: mcontext.sp,
            fp: mcontext.regs[Self::FRAME_POINTER],
            extra: clear_pac_bits(mcontext.regs[Self::LINK_REGISTER]),
        }
    }
}

#[cfg(target_arch = "x86_64")]
pub(super) fn clear_pac_bits(return_address: u64) -> u64 {
    return_address
}

#[cfg(target_arch = "aarch64")]
pub(super) fn clear_pac_bits(return_address: u64) -> u64 {
    let mut link_register = return_address;
    unsafe {
        std::arch::asm!(
            "xpaclri",
            inout("x30") link_register,
            options(pure, nomem, nostack, preserves_flags)
        );
    }
    link_register
}

#[cfg(all(test, target_arch = "aarch64"))]
pub(super) fn sign_return_address(return_address: u64, modifier: u64) -> u64 {
    let signed;
    unsafe {
        std::arch::asm!(
            "pacia1716",
            inout("x17") return_address => signed,
            in("x16") modifier,
            options(pure, nomem, nostack, preserves_flags)
        );
    }
    signed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_arch = "x86_64")]
    fn context_holding(registers: FaultRegisters) -> libc::ucontext_t {
        let mut context: libc::ucontext_t = unsafe { std::mem::zeroed() };
        let gregs = &mut context.uc_mcontext.gregs;
        gregs[libc::REG_RIP as usize] = registers.pc as i64;
        gregs[libc::REG_RSP as usize] = registers.sp as i64;
        gregs[libc::REG_RBP as usize] = registers.fp as i64;
        gregs[libc::REG_ERR as usize] = registers.extra as i64;
        context
    }

    #[cfg(target_arch = "aarch64")]
    fn context_holding(registers: FaultRegisters) -> libc::ucontext_t {
        let mut context: libc::ucontext_t = unsafe { std::mem::zeroed() };
        let mcontext = &mut context.uc_mcontext;
        mcontext.pc = registers.pc;
        mcontext.sp = registers.sp;
        mcontext.regs[FaultRegisters::FRAME_POINTER] = registers.fp;
        mcontext.regs[FaultRegisters::LINK_REGISTER] = registers.extra;
        context
    }

    #[test]
    fn fault_registers_read_the_context() {
        let registers = FaultRegisters {
            pc: 0x7f12_3456_7000,
            sp: 0x7ffd_0000_1000,
            fp: 0x7ffd_0000_1040,
            extra: 0x7f12_3456_8004,
        };
        assert_eq!(FaultRegisters::read(&context_holding(registers)), registers);
    }

    #[cfg(target_arch = "aarch64")]
    #[test]
    fn signed_return_addresses_are_masked() {
        let return_address = clear_pac_bits as *const () as usize as u64;
        let signed = sign_return_address(return_address, 0x5eed);
        assert_eq!(clear_pac_bits(signed), return_address);

        let context = context_holding(FaultRegisters {
            extra: signed,
            ..FaultRegisters::default()
        });
        assert_eq!(FaultRegisters::read(&context).extra, return_address);
    }
}
