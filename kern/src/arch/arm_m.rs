use zerocopy::FromBytes;

use crate::task;
use crate::app;
use crate::umem::USlice;

#[repr(C)]
#[derive(Debug, Default)]
pub struct SavedState {

    // NOTE: the following fields must be kept contiguous!
    r4: u32,
    r5: u32,
    r6: u32,
    r7: u32,
    r8: u32,
    r9: u32,
    r10: u32,
    r11: u32,
    // NOTE: the above fields must be kept contiguous!
    psp: u32,
}

impl task::ArchState for SavedState {
    fn stack_pointer(&self) -> u32 {
        self.psp
    }

    /// Reads syscall argument register 0.
    fn arg0(&self) -> u32 {
        self.r4
    }
    fn arg1(&self) -> u32 {
        self.r5
    }
    fn arg2(&self) -> u32 {
        self.r6
    }
    fn arg3(&self) -> u32 {
        self.r7
    }
    fn arg4(&self) -> u32 {
        self.r8
    }
    fn arg5(&self) -> u32 {
        self.r9
    }
    fn arg6(&self) -> u32 {
        self.r10
    }
    fn arg7(&self) -> u32 {
        self.r11
    }

    /// Writes syscall return argument 0.
    fn ret0(&mut self, x: u32) {
        self.r4 = x
    }
    fn ret1(&mut self, x: u32) {
        self.r5 = x
    }
    fn ret2(&mut self, x: u32) {
        self.r6 = x
    }
    fn ret3(&mut self, x: u32) {
        self.r7 = x
    }
    fn ret4(&mut self, x: u32) {
        self.r8 = x
    }
    fn ret5(&mut self, x: u32) {
        self.r9 = x
    }
}

/// Stuff placed on the stack at exception entry whether or not an FPU is
/// present.
#[derive(Debug, FromBytes, Default)]
#[repr(C)]
pub struct BaseExceptionFrame {
    r0: u32,
    r1: u32,
    r2: u32,
    r3: u32,
    r12: u32,
    lr: u32,
    pc: u32,
    xpsr: u32,
}

/// Extended version for FPU.
#[derive(Debug, FromBytes, Default)]
#[repr(C)]
pub struct ExtendedExceptionFrame {
    base: BaseExceptionFrame,
    fpu_regs: [u32; 16],
    fpscr: u32,
    reserved: u32,
}

/// Initially we just set the Thumb Mode bit, the minimum required.
const INITIAL_PSR: u32 = 1 << 24;

const INITIAL_FPSCR: u32 = 0;

pub fn reinitialize(task: &mut task::Task) {
    task.save = SavedState::default();
    // Modern ARMv7-M machines require 8-byte stack alignment.
    assert!(task.descriptor.initial_stack & 0x7 == 0);

    // The remaining state is stored on the stack.
    // TODO: this assumes availability of an FPU.
    // Use checked operations to get a reference to the exception frame.
    let frame_size = core::mem::size_of::<ExtendedExceptionFrame>();
    let mut uslice: USlice<ExtendedExceptionFrame> = USlice::from_raw(
        task.descriptor.initial_stack as usize - frame_size,
        1,
    )
    .unwrap();
    assert!(task.can_write(&uslice));

    let frame = unsafe { &mut uslice.assume_writable()[0] };

    // Conservatively/defensively zero the entire frame.
    *frame = ExtendedExceptionFrame::default();
    // Now fill in the bits we actually care about.
    frame.base.pc = task.descriptor.entry_point | 1; // for thumb
    frame.base.xpsr = INITIAL_PSR;
    frame.base.lr = 0xFFFF_FFFF; // trap on return from main
    frame.fpscr = INITIAL_FPSCR;

    // Set the initial stack pointer, *not* to the stack top, but to the base of
    // this frame.
    task.save.psp = frame as *const _ as u32;
}

pub fn apply_memory_protection(task: &task::Task) {
    // We are manufacturing authority to interact with the MPU here, because we
    // can't thread a cortex-specific peripheral through an
    // architecture-independent API. This approach might bear revisiting later.
    let mpu = unsafe {
        // At least by not taking a &mut we're confident we're not violating
        // aliasing....
        &*cortex_m::peripheral::MPU::ptr()
    };

    for (i, region) in task.region_table.iter().enumerate() {
        // This MPU requires that all regions are 32-byte aligned...in part
        // because it stuffs extra stuff into the bottom five bits.
        debug_assert_eq!(region.base & 0x1F, 0);

        let rbar = (i as u32)  // region number
            | (1 << 4)  // honor the region number
            | region.base;
        let ratts = region.attributes;
        let xn = !ratts.contains(app::RegionAttributes::EXECUTE);
        // These AP encodings are chosen such that we never deny *privileged*
        // code (i.e. us) access to the memory.
        let ap = if ratts.contains(app::RegionAttributes::WRITE) {
            0b011
        } else if ratts.contains(app::RegionAttributes::READ) {
            0b010
        } else {
            0b001
        };
        let (tex, scb) = if ratts.contains(app::RegionAttributes::DEVICE) {
            (0b000, 0b111)
        } else {
            (0b001, 0b111)
        };
        // This is a bit of a hack; it works if the size is a power of two, but
        // will undersize the region if it isn't. We really need to validate the
        // regions at boot time with architecture-specific logic....
        let l2size = 30 - region.size.leading_zeros();
        let rasr = (xn as u32) << 28
            | ap << 24
            | tex << 19
            | scb << 16
            | l2size << 1
            | (1 << 0);  // enable
        unsafe {
            mpu.rbar.write(rbar);
            mpu.rasr.write(rasr);
        }
    }
}

pub fn start_first_task(task: &task::Task) -> ! {
    // We are manufacturing authority to interact with the MPU here, because we
    // can't thread a cortex-specific peripheral through an
    // architecture-independent API. This approach might bear revisiting later.
    let mpu = unsafe {
        // At least by not taking a &mut we're confident we're not violating
        // aliasing....
        &*cortex_m::peripheral::MPU::ptr()
    };

    const ENABLE: u32 = 0b001;
    const PRIVDEFENA: u32 = 0b100;
    // Safety: this has no memory safety implications. The worst it can do is
    // cause us to fault, which is safe. The register API doesn't know this.
    unsafe {
        mpu.ctrl.write(ENABLE | PRIVDEFENA);
    }

    unsafe {
        asm! { "
            msr PSP, $0             @ set the user stack pointer
            ldm $1, {r4-r11}        @ restore the callee-save registers
            svc #0xFF               @ branch into user mode (svc # ignored)
            udf #0xad               @ should not return
        "
            :
            : "r"(task.save.psp),
              "r"(&task.save.r4)
            : "memory", "r4", "r5", "r6", "r7", "r8", "r9", "r10", "r11"
            : "volatile"
        }
    }
    unreachable!()
}

#[allow(non_snake_case)]
#[naked]
#[no_mangle]
pub unsafe fn SVCall() {
    asm! {"
        cmp lr, #0xFFFFFFF9     @ is it coming from inside the kernel?
        beq 1f                  @ if so, we're starting the first task;
                                @ jump ahead.
        @ the common case is handled by branch-not-taken as it's faster

        udf #0xad               @ svcs are not implemented yet

    1:  @ starting up the first task.
        movs r0, #1             @ get bitmask to...
        msr CONTROL, r0         @ ...shed privs from thread mode.
                                @ note: now barrier here because exc return
                                @ serves as barrier

        mov lr, #0xFFFFFFED     @ materialize EXC_RETURN value to
                                @ return into thread mode, PSP, FP on

        bx lr                   @ branch into user mode
        "
        :
        :
        :
        : "volatile"
    }
}