use core::{mem, ptr};

use cm::{DCB, DWT, NVIC};
use pac::{p0, CLOCK, P0, RTC0};

use crate::led;

#[no_mangle]
unsafe extern "C" fn Reset() {
    // NOTE(borrow_unchecked) interrupts disabled; this runs before user code
    // enable the cycle counter and start it with an initial count of 0
    DCB::borrow_unchecked(|dcb| dcb.DEMCR.rmw(|_, w| w.TRCENA(1)));
    DWT::borrow_unchecked(|dwt| {
        dwt.CYCCNT.write(0);
        dwt.CTRL.rmw(|_, w| w.CYCCNTENA(1));
    });

    CLOCK::borrow_unchecked(|clock| {
        // use the external crystal (LFXO) as the low-frequency clock (LFCLK) source
        clock.LFCLKSRC.write(|w| w.SRC(1));

        // start the LFXO
        clock.TASKS_LFCLKSTART.write(|w| w.TASKS_LFCLKSTART(1));
    });

    // start the RTC with a counter of 0
    RTC0::borrow_unchecked(|rtc| {
        rtc.TASKS_CLEAR.write(|w| w.TASKS_CLEAR(1));
        rtc.TASKS_START.write(|w| w.TASKS_START(1));
    });

    // zero .bss
    extern "C" {
        static mut _sbss: u32;
        static mut _ebss: u32;
    }

    let sbss = &mut _sbss as *mut u32;
    let ebss = &mut _ebss as *mut u32;
    ptr::write_bytes(
        sbss,
        0,
        (ebss as usize - sbss as usize) / mem::size_of::<u32>(),
    );

    // init .data
    #[cfg(feature = "flash")]
    {
        extern "C" {
            static mut _sdata: u32;
            static mut _edata: u32;
            static mut _sidata: u32;
        }

        let sdata = &mut _sdata as *mut u32;
        let edata = &mut _edata as *mut u32;
        let sidata = &_sidata as *const u32;
        ptr::copy_nonoverlapping(
            sidata,
            sdata,
            (edata as usize - sdata as usize) / mem::size_of::<u32>(),
        );
    }

    // NOTE this is a memory barrier -- .bss will be zeroed before the code that comes after this
    asm::disable_irq();

    // seal some peripherals so they cannot be used from the application
    CLOCK::seal();
    DCB::seal();
    DWT::seal();
    NVIC::seal();
    P0::seal();
    RTC0::seal();

    // configure I/O pins
    // set outputs high (LEDs off)
    p0::OUTSET::address().write_volatile(led::RED | led::BLUE | led::GREEN);
    // set pins as output
    p0::DIRSET::address().write_volatile(led::RED | led::BLUE | led::GREEN);

    // run initializers
    extern "C" {
        static _sinit: usize;
        static _einit: usize;
    }

    let mut sinit = &_sinit as *const usize;
    let einit = &_einit as *const usize;
    while sinit < einit {
        let f: unsafe extern "C" fn() = mem::transmute(sinit.read());
        f();
        sinit = sinit.add(1);
    }

    extern "Rust" {
        fn main() -> !;
    }

    asm::enable_irq();

    main()
}

#[no_mangle]
fn __semidap_timestamp() -> u32 {
    crate::cyccnt() >> 6
}

#[repr(C)]
union Vector {
    stack_pointer: *const u32,
    handler: unsafe extern "C" fn(),
    reserved: usize,
}

extern "C" {
    static __stack_top__: u32;

    // Cortex-M exceptions
    fn NMI();
    fn HardFault();
    fn MemManage();
    fn BusFault();
    fn UsageFault();
    fn SVCall();
    fn DebugMonitor();
    fn PendSV();
    fn SysTick();

    // nRF52840 interrupts
    fn POWER_CLOCK();
    fn RADIO();
    fn UARTE0_UART0();
    fn SPIM0_SPIS0_TWIM0_TWIS0_SPI0_TWI0();
    fn SPIM1_SPIS1_TWIM1_TWIS1_SPI1_TWI1();
    fn NFCT();
    fn GPIOTE();
    fn SAADC();
    fn TIMER0();
    fn TIMER1();
    fn TIMER2();
    fn RTC0();
    fn TEMP();
    fn RNG();
    fn ECB();
    fn CCM_AAR();
    fn WDT();
    fn RTC1();
    fn QDEC();
    fn COMP_LPCOMP();
    fn SWI0_EGU0();
    fn SWI1_EGU1();
    fn SWI2_EGU2();
    fn SWI3_EGU3();
    fn SWI4_EGU4();
    fn SWI5_EGU5();
    fn TIMER3();
    fn TIMER4();
    fn PWM0();
    fn PDM();
    fn MWU();
    fn PWM1();
    fn PWM2();
    fn SPIM2_SPIS2_SPI2();
    fn RTC2();
    fn I2S();
    fn FPU();
    fn USBD();
    fn UARTE1();
    fn QSPI();
    fn CRYPTOCELL();
    fn PWM3();
    fn SPIM3();
}

#[link_section = ".vectors"]
#[no_mangle]
static mut VECTORS: [Vector; 64] = [
    // Cortex-M exceptions
    Vector {
        stack_pointer: unsafe { &__stack_top__ as *const u32 },
    },
    Vector { handler: Reset },
    Vector { handler: NMI },
    Vector { handler: HardFault },
    Vector { handler: MemManage },
    Vector { handler: BusFault },
    Vector {
        handler: UsageFault,
    },
    Vector { reserved: 0 },
    Vector { reserved: 0 },
    Vector { reserved: 0 },
    Vector { reserved: 0 },
    Vector { handler: SVCall },
    Vector {
        handler: DebugMonitor,
    },
    Vector { reserved: 0 },
    Vector { handler: PendSV },
    Vector { handler: SysTick },
    // nRF52840 interrupts
    Vector {
        handler: POWER_CLOCK, // 0
    },
    Vector {
        handler: RADIO, // 1
    },
    Vector {
        handler: UARTE0_UART0, // 2
    },
    Vector {
        handler: SPIM0_SPIS0_TWIM0_TWIS0_SPI0_TWI0, // 3
    },
    Vector {
        handler: SPIM1_SPIS1_TWIM1_TWIS1_SPI1_TWI1, // 4
    },
    Vector {
        handler: NFCT, // 5
    },
    Vector {
        handler: GPIOTE, // 6
    },
    Vector {
        handler: SAADC, // 7
    },
    Vector {
        handler: TIMER0, // 8
    },
    Vector {
        handler: TIMER1, // 9
    },
    Vector {
        handler: TIMER2, // 10
    },
    Vector {
        handler: RTC0, // 11
    },
    Vector {
        handler: TEMP, // 12
    },
    Vector {
        handler: RNG, // 13
    },
    Vector {
        handler: ECB, // 14
    },
    Vector {
        handler: CCM_AAR, // 15
    },
    Vector {
        handler: WDT, // 16
    },
    Vector {
        handler: RTC1, // 17
    },
    Vector {
        handler: QDEC, // 18
    },
    Vector {
        handler: COMP_LPCOMP, // 19
    },
    Vector {
        handler: SWI0_EGU0, // 20
    },
    Vector {
        handler: SWI1_EGU1, // 21
    },
    Vector {
        handler: SWI2_EGU2, // 22
    },
    Vector {
        handler: SWI3_EGU3, // 23
    },
    Vector {
        handler: SWI4_EGU4, // 24
    },
    Vector {
        handler: SWI5_EGU5, // 25
    },
    Vector {
        handler: TIMER3, // 26
    },
    Vector {
        handler: TIMER4, // 27
    },
    Vector {
        handler: PWM0, // 28
    },
    Vector {
        handler: PDM, // 29
    },
    Vector { reserved: 0 }, // 30
    Vector { reserved: 0 }, // 31
    Vector {
        handler: MWU, // 32
    },
    Vector {
        handler: PWM1, // 33
    },
    Vector {
        handler: PWM2, // 34
    },
    Vector {
        handler: SPIM2_SPIS2_SPI2, // 35
    },
    Vector {
        handler: RTC2, // 36
    },
    Vector {
        handler: I2S, // 37
    },
    Vector {
        handler: FPU, // 38
    },
    Vector {
        handler: USBD, // 39
    },
    Vector {
        handler: UARTE1, // 40
    },
    Vector {
        handler: QSPI, // 41
    },
    Vector {
        handler: CRYPTOCELL, // 42
    },
    Vector { reserved: 0 }, // 43
    Vector { reserved: 0 }, // 44
    Vector {
        handler: PWM3, // 45
    },
    Vector { reserved: 0 }, // 46
    Vector {
        handler: SPIM3, // 47
    },
];
