//! USB device

use core::{
    cmp, mem, ops, ptr, slice,
    sync::atomic::{AtomicBool, AtomicU8, Ordering},
    task::Poll,
};

use binfmt::derive::binDebug;
use pac::{
    usbd::{epdatastatus, epinen, epouten, eventcause},
    POWER, USBD,
};
use pool::Box;
use usb2::{cdc::acm, GetDescriptor, Request, StandardRequest};

use crate::{atomic::Atomic, mem::P, Interrupt1, NotSendOrSync};

include!(concat!(env!("OUT_DIR"), "/descs.rs"));

static EPIN1_BUSY: AtomicBool = AtomicBool::new(false);
static EPOUT1_STATE: Atomic<EpOut1State> = Atomic::new();
static EPOUT1_SIZE: AtomicU8 = AtomicU8::new(0);

#[tasks::declare]
mod task {
    use core::mem::MaybeUninit;

    use pac::{CLOCK, USBD};
    use pool::Node;

    use crate::{clock, errata, mem::P, Interrupt0, Interrupt1};

    use super::{Ep0State, PowerEvent, PowerState, UsbdEvent};

    static mut PCSTATE: PowerState = PowerState::Off;

    // NOTE(unsafe) all interrupts are still globally masked (`CPSID I`)
    fn init() {
        #[uninit(unsafe)]
        static mut PACKETS: [MaybeUninit<Node<[u8; P::SIZE]>>; 3] = [
            MaybeUninit::uninit(),
            MaybeUninit::uninit(),
            MaybeUninit::uninit(),
        ];

        for packet in PACKETS {
            P::manage(packet)
        }

        // reserve peripherals for HAL use
        pac::POWER::seal();
        pac::USBD::seal();

        CLOCK::borrow_unchecked(|clock| unsafe { clock.INTENSET.write(|w| w.HFCLKSTARTED(1)) });
        pac::POWER::borrow_unchecked(|power| unsafe {
            power
                .INTENSET
                .write(|w| w.USBDETECTED(1).USBREMOVED(1).USBPWRRDY(1));
        });
        pac::USBD::borrow_unchecked(|usbd| unsafe {
            usbd.INTENSET.write(|w| {
                w.ENDEPIN1(1)
                    .EP0DATADONE(1)
                    .EP0SETUP(1)
                    .EPDATA(1)
                    .USBEVENT(1)
                    .USBRESET(1)
                    .ENDEPOUT1(1)
            });
        });

        unsafe {
            crate::unmask0(&[Interrupt0::POWER_CLOCK]);
            crate::unmask1(&[Interrupt1::USBD]);
        }
    }

    fn POWER() -> Option<()> {
        semidap::trace!("POWER");

        let event = PowerEvent::next();
        if let Some(event) = event {
            semidap::debug!("-> {}", event);
        }

        match PCSTATE {
            PowerState::Off => {
                if event? != PowerEvent::USBDETECTED {
                    #[cfg(debug_assertions)]
                    super::unreachable()
                }

                // turn on the USB peripheral
                unsafe { errata::e187a() }
                USBD::borrow_unchecked(|usbd| usbd.ENABLE.write(|w| w.ENABLE(1)));

                semidap::info!("enabled the USB peripheral");

                *PCSTATE = PowerState::RampUp {
                    clock: clock::is_stable(),
                    power: false,
                    usb: false,
                };
            }

            PowerState::RampUp { clock, power, usb } => {
                if !*clock && event.is_none() {
                    *clock = true;
                } else if !*power && event? == PowerEvent::USBPWRRDY {
                    *power = true;
                    semidap::info!("USB power supply ready");
                } else {
                    #[cfg(debug_assertions)]
                    super::unreachable()
                }

                if *clock && *power && *usb {
                    *PCSTATE = PowerState::Ready;
                    super::connect();
                }
            }

            // TODO handle powering down the HFXO?
            PowerState::Ready => super::todo(),
        }

        None
    }

    fn USBD() -> Option<()> {
        static mut USB_STATE: usb2::State = usb2::State::Default;
        static mut EP0_STATE: Ep0State = Ep0State::Idle;

        semidap::trace!("USBD");

        let event = UsbdEvent::next()?;

        semidap::debug!("-> {}", event);

        match PCSTATE {
            PowerState::Off =>
            {
                #[cfg(debug_assertions)]
                super::unreachable()
            }

            PowerState::RampUp { clock, power, usb } => {
                if !*usb && event == UsbdEvent::USBEVENT {
                    #[cfg(debug_assertions)]
                    if super::EVENTCAUSE().READY() == 0 {
                        super::unreachable();
                    }

                    *usb = true;
                    semidap::info!("USB controller is ready");

                    if *clock && *power && *usb {
                        *PCSTATE = PowerState::Ready;
                        super::connect();
                    }
                } else {
                    #[cfg(debug_assertions)]
                    super::unreachable()
                }
            }

            PowerState::Ready => match event {
                UsbdEvent::USBEVENT => {
                    let eventcause = super::EVENTCAUSE();

                    if eventcause.SUSPEND() != 0 {
                        super::suspend();
                    } else if eventcause.RESUME() != 0 {
                        super::resume()
                    } else {
                        super::todo()
                    }
                }

                UsbdEvent::USBRESET => {
                    semidap::info!("USB reset");

                    match USB_STATE {
                        usb2::State::Default | usb2::State::Address { .. } => {
                            *USB_STATE = usb2::State::Default;
                        }

                        usb2::State::Configured { .. } => {
                            // TODO need to cancel existing transfers?
                            // TODO disable end points
                            super::todo()
                        }
                    }
                }

                UsbdEvent::EP0SETUP => {
                    #[cfg(debug_assertions)]
                    if *EP0_STATE != Ep0State::Idle {
                        super::unreachable()
                    }

                    if super::ep0setup(USB_STATE, EP0_STATE).is_err() {
                        super::EP0STALL()
                    }
                }

                UsbdEvent::EP0DATADONE => {
                    semidap::info!("EPIN0: data transmitted");

                    match EP0_STATE {
                        Ep0State::Write { leftover } => {
                            if *leftover != 0 {
                                super::continue_epin0(leftover);
                            } else {
                                *EP0_STATE = Ep0State::Idle;
                            }
                        }

                        Ep0State::Idle =>
                        {
                            #[cfg(debug_assertions)]
                            super::unreachable()
                        }
                    }
                }

                // TODO remove
                UsbdEvent::ENDEPIN2 => {
                    // nothing to do here
                }

                UsbdEvent::ENDEPOUT2 => super::todo(),

                // TODO remove?
                UsbdEvent::EPDATA => {
                    let status = super::EPDATASTATUS();
                    semidap::info!("{}", status);
                    if status.EPIN2() != 0 {
                        use core::sync::atomic::{AtomicU8, Ordering};

                        static X: AtomicU8 = AtomicU8::new(0);

                        let x = X.load(Ordering::Relaxed);
                        if x < 3 {
                            USBD::borrow_unchecked(|usbd| {
                                usbd.EPIN2_MAXCNT.write(|w| w.MAXCNT(0));
                                usbd.TASKS_STARTEPIN2.write(|w| w.TASKS_STARTEPIN(1));
                            });
                            X.store(x + 1, Ordering::Relaxed);
                        }
                    }
                    if status.EPOUT2() != 0 {
                        USBD::borrow_unchecked(|usbd| {
                            semidap::info!("{}", usbd.SIZE_EPOUT2.read());
                            // fetch next packet
                            usbd.SIZE_EPOUT2.write(|w| w.SIZE(0));
                        });
                    }
                }
            },
        }

        None
    }
}

fn ep0setup(usb_state: &mut usb2::State, ep_state: &mut Ep0State) -> Result<(), ()> {
    let bmrequesttype = BMREQUESTTYPE();
    let brequest = BREQUEST();
    let wvalue = WVALUE();
    let windex = WINDEX();
    let wlength = WLENGTH();

    let req = Request::parse(bmrequesttype, brequest, wvalue, windex, wlength).map_err(|_| {
        semidap::error!(
            "EP0SETUP: unknown request ({}, {}, {}, {}, {})",
            bmrequesttype,
            brequest,
            wvalue,
            windex,
            wlength
        );
    })?;

    match req {
        Request::Standard(StandardRequest::GetDescriptor { descriptor, length }) => {
            semidap::info!("GET_DESCRIPTOR [{}] ..", length as u8);

            match descriptor {
                GetDescriptor::Device => {
                    semidap::info!("GET_DESCRIPTOR Device");

                    start_epin0(
                        DEVICE_DESC.get(..length.into()).unwrap_or(&DEVICE_DESC),
                        ep_state,
                    );
                }

                GetDescriptor::DeviceQualifier => {
                    semidap::warn!("GET_DESCRIPTOR DeviceQualifier is not supported");
                    return Err(());
                }

                GetDescriptor::Configuration { index } => {
                    semidap::info!("GET_DESCRIPTOR Configuration {}", index);

                    if index == 0 {
                        start_epin0(
                            CONFIG_DESC.get(..length.into()).unwrap_or(&CONFIG_DESC),
                            ep_state,
                        );
                    } else {
                        semidap::error!("out of bounds GET_DESCRIPTOR Configuration request");
                        return Err(());
                    }
                }

                GetDescriptor::String { .. } => {
                    semidap::error!("requested string descriptor doesn't exist");
                    return Err(())
                }

                _ => {
                    semidap::error!("unsupported GET_DESCRIPTOR {}", wvalue);
                    todo();
                }
            }
        }

        Request::Standard(StandardRequest::SetAddress {
            address: new_address,
        }) => {
            // nothing to do here; the hardware will complete the transaction
            semidap::info!(
                "SET_ADDRESS {}",
                new_address.map(|nz| nz.get()).unwrap_or(0)
            );

            match *usb_state {
                usb2::State::Default => {
                    if let Some(address) = new_address {
                        // move to the Address state
                        *usb_state = usb2::State::Address(address);

                        semidap::info!("moving to the Address state");
                    } else {
                        // stay in the Default state
                    }
                }

                usb2::State::Address(curr_address) => {
                    if let Some(new_address) = new_address {
                        if new_address != curr_address {
                            *usb_state = usb2::State::Address(new_address);

                            semidap::info!("changing host assigned address");
                        }
                    } else {
                        *usb_state = usb2::State::Default;

                        semidap::info!("returning to the Default state");
                    }
                }

                usb2::State::Configured { .. } => {
                    semidap::error!("invalid request in the Configured state");
                    return Err(());
                }
            }

            // nothing else to do here; the hardware will complete the transaction
        }

        Request::Standard(StandardRequest::SetConfiguration { value }) => {
            semidap::info!(
                "SET_CONFIGURATION {}",
                value.map(|nz| nz.get()).unwrap_or(0)
            );

            match *usb_state {
                usb2::State::Default => {
                    semidap::error!("invalid request in the Default state");
                    return Err(());
                }

                usb2::State::Address(address) => {
                    if let Some(value) = value {
                        if value == CONFIG_VAL {
                            semidap::info!("moving to the Configured state");
                            *usb_state = usb2::State::Configured { address, value };

                            USBD::borrow_unchecked(|usbd| {
                                usbd.EPINEN.write(|w| w.IN0(1).IN1(1).IN2(1));
                                usbd.EPOUTEN.write(|w| w.OUT0(1).OUT2(1));
                                usbd.SIZE_EPOUT2.write(|w| w.SIZE(0));

                                // FIXME remove
                                #[repr(align(4))]
                                struct Align4([u8; 6]);
                                static S: Align4 = Align4([b'H', b'e', b'l', b'l', b'o', b'\n']);
                                usbd.EPIN2_PTR.write(|w| w.PTR(S.0.as_ptr() as u32));
                                usbd.EPIN2_MAXCNT.write(|w| w.MAXCNT(S.0.len() as u8));
                            })
                        } else {
                            semidap::error!("requested configuration is not supported");
                            return Err(());
                        }
                    } else {
                        // stay in the Address state
                    }
                }

                usb2::State::Configured {
                    address,
                    value: curr_value,
                } => {
                    if let Some(new_value) = value {
                        if new_value == curr_value {
                            // no change
                        } else {
                            // other configurations are not supported
                            semidap::error!("requested configuration is not supported");
                            return Err(());
                        }
                    } else {
                        // TODO disable endpoints and transfers
                        semidap::info!("returning to the Address state");
                        *usb_state = usb2::State::Address(address);
                    }
                }
            }

            // issue a status stage to acknowledge the request
            ep0status()
        }

        Request::Acm(acm::Request::GetLineCoding { interface }) => {
            semidap::info!("GET_LINE_CODING {}", interface);

            return Err(());
        }

        Request::Acm(acm::Request::SetLineCoding { interface }) => {
            semidap::info!("SET_LINE_CODING {}", interface);

            // FIXME we should probably read the host data
            return Err(());
        }

        Request::Acm(acm::Request::SetControlLineState(cls)) => {
            semidap::info!(
                "SET_CONTROL_LINE_STATE {} rts={} dte_present={}",
                cls.interface,
                cls.rts as u8,
                cls.dte_present as u8
            );

            static ONCE: AtomicBool = AtomicBool::new(false);
            if !ONCE.load(Ordering::Relaxed) {
                USBD::borrow_unchecked(|usbd| {
                    usbd.TASKS_STARTEPIN2.write(|w| w.TASKS_STARTEPIN(1));
                });
                ONCE.store(true, Ordering::Relaxed);
            }

            // issue a status stage to acknowledge the request
            semidap::info!("ACM request acknowledged");
            ep0status()
        }

        _ => {
            semidap::error!("EP0SETUP: request is not supported");
            return Err(());
        }
    }

    Ok(())
}

fn start_epin0(bytes: &'static [u8], ep_state: &mut Ep0State) {
    #[cfg(debug_assertions)]
    semidap::assert!(
        *ep_state == Ep0State::Idle,
        "tried to start a control read transfer before the previous one finished"
    );

    let len = bytes.len() as u16;

    let maxcnt = if len <= MAX_PACKET_SIZE0.into() {
        // done in a single transfer
        short_ep0datadone_ep0status();
        *ep_state = Ep0State::Write { leftover: 0 };
        len as u8
    } else {
        unshort_ep0datadone_ep0status();
        let maxcnt = MAX_PACKET_SIZE0;
        *ep_state = Ep0State::Write {
            leftover: len - u16::from(maxcnt),
        };
        maxcnt
    };

    semidap::info!("EPIN0: sending {}B of data", maxcnt);

    USBD::borrow_unchecked(|usbd| {
        usbd.EPIN0_MAXCNT.write(|w| w.MAXCNT(maxcnt));
        usbd.EPIN0_PTR.write(|w| w.PTR(bytes.as_ptr() as u32));

        usbd.TASKS_STARTEPIN0.write(|w| w.TASKS_STARTEPIN(1));
    })
}

fn continue_epin0(leftover: &mut u16) {
    USBD::borrow_unchecked(|usbd| {
        usbd.EPIN0_PTR
            .rmw(|r, w| w.PTR(r.PTR() + u32::from(MAX_PACKET_SIZE0)));

        let max_packet_size0 = u16::from(MAX_PACKET_SIZE0);
        if *leftover <= max_packet_size0 {
            let maxcnt = *leftover as u8;
            semidap::info!("EPIN0: sending last {}B of data", maxcnt);
            short_ep0datadone_ep0status();
            usbd.EPIN0_MAXCNT.write(|w| w.MAXCNT(maxcnt));
            *leftover = 0;
        } else {
            semidap::info!("EPIN0: sending next {}B of data", MAX_PACKET_SIZE0);
            *leftover -= max_packet_size0;
        }

        usbd.TASKS_STARTEPIN0.write(|w| w.TASKS_STARTEPIN(1));
    })
}

/// Bulk IN endpoint 1
pub struct BulkIn {
    _not_send_or_sync: NotSendOrSync,
}

/// Bulk OUT endpoint 1
pub struct BulkOut {
    _not_send_or_sync: NotSendOrSync,
}

/// Claims the USB interface
pub fn claim() -> (BulkIn, BulkOut) {
    static ONCE: AtomicBool = AtomicBool::new(false);

    if ONCE
        .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
        .is_ok()
    {
        (
            BulkIn {
                _not_send_or_sync: NotSendOrSync::new(),
            },
            BulkOut {
                _not_send_or_sync: NotSendOrSync::new(),
            },
        )
    } else {
        semidap::panic!("`usbd` interface has already been claimed")
    }
}

impl BulkOut {
    /// Reads a packet from the host
    pub async fn read(&mut self) -> Packet {
        // wait until the endpoint has been enabled
        crate::poll_fn(|| {
            if EPOUTEN().OUT1() != 0 {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
        .await;

        let mut packet = Packet::new().await;

        let mut needs_len = true;
        let epstart = || {
            USBD::borrow_unchecked(|usbd| {
                const NO_DATA: u8 = u8::max_value();
                let mut size = NO_DATA;
                let state = EPOUT1_STATE.load();
                match state {
                    EpOut1State::Idle | EpOut1State::DataReady => {
                        usbd.EPOUT1_PTR
                            .write(|w| w.PTR(packet.data_ptr_mut() as u32));

                        if state == EpOut1State::DataReady {
                            size = SIZE_EPOUT1();
                            EPOUT1_MAXCNT(size);
                            packet.set_len(size);
                            needs_len = false;
                            EPOUT1_STATE.store(EpOut1State::TransferInProgress);
                        } else {
                            semidap::info!("EPOUT1: buffer ready");
                            EPOUT1_STATE.store(EpOut1State::BufferReady);
                        }
                    }

                    EpOut1State::BufferReady | EpOut1State::TransferInProgress =>
                    {
                        #[cfg(debug_assertions)]
                        unreachable()
                    }
                }

                if size != NO_DATA {
                    // NOTE the following operation handles the buffer to the `USBD` task
                    crate::dma_start();
                    // start DMA transfer
                    STARTEPOUT1();
                    semidap::info!("EPOUT1: transfer started ({}B)", size);
                }
            })
        };
        unsafe { crate::atomic1(Interrupt1::USBD, epstart) }

        crate::poll_fn(|| {
            match EPOUT1_STATE.load() {
                EpOut1State::Idle | EpOut1State::DataReady => {
                    // NOTE the `USBD` task has handled the buffer back to us
                    crate::dma_end();
                    Poll::Ready(())
                }

                EpOut1State::BufferReady | EpOut1State::TransferInProgress => Poll::Pending,
            }
        })
        .await;

        if needs_len {
            packet.set_len(EPOUT1_SIZE.load(Ordering::Relaxed));
        }

        packet
    }
}

impl BulkIn {
    /// Sends a packet to the host
    pub async fn write(&mut self, packet: Packet) {
        // wait until the endpoint has been enabled
        crate::poll_fn(|| {
            if EPINEN().IN1() != 0 {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
        .await;

        crate::poll_fn(|| {
            if EPIN1_BUSY.load(Ordering::Relaxed) {
                Poll::Pending
            } else {
                Poll::Ready(())
            }
        })
        .await;

        USBD::borrow_unchecked(|usbd| {
            let len = packet.len();

            usbd.EPIN1_PTR.write(|w| w.PTR(packet.data_ptr() as u32));
            mem::forget(packet);
            usbd.EPIN1_MAXCNT.write(|w| w.MAXCNT(len));
            EPIN1_BUSY.store(true, Ordering::Relaxed);

            semidap::info!("EPIN1: transfer started ({}B)", len);

            crate::dma_start();
            usbd.TASKS_STARTEPIN1.write(|w| w.TASKS_STARTEPIN(1));
        });
    }
}

/// USB packet
pub struct Packet {
    buffer: Box<P>,
    len: u8,
}

impl Packet {
    /// How much data this packet can hold
    pub const CAPACITY: u8 = 64;

    const PADDING: usize = 4;

    /// Returns an empty USB packet
    pub async fn new() -> Self {
        Self {
            buffer: P::alloc().await,
            len: 0,
        }
    }

    /// Fills the packet with given `src` data
    ///
    /// NOTE `src` data will be truncated to `Self::CAPACITY` bytes
    pub fn copy_from_slice(&mut self, src: &[u8]) {
        let len = cmp::min(src.len(), Packet::CAPACITY as usize);
        unsafe { ptr::copy_nonoverlapping(src.as_ptr(), self.data_ptr_mut(), len) }
        self.len = len as u8;
    }

    /// Returns the size of this packet
    pub fn len(&self) -> u8 {
        self.len
    }

    /// Changes the `len` of the packet
    ///
    /// NOTE `len` will be truncated to `Self::CAPACITY` bytes
    pub fn set_len(&mut self, len: u8) {
        self.len = cmp::min(len, Packet::CAPACITY);
    }

    #[cfg(feature = "radio")]
    pub(crate) unsafe fn from_parts(buffer: Box<P>, len: u8) -> Self {
        Self { buffer, len }
    }

    fn data_ptr(&self) -> *const u8 {
        unsafe { self.buffer.as_ptr().add(Self::PADDING) }
    }

    fn data_ptr_mut(&mut self) -> *mut u8 {
        unsafe { self.buffer.as_mut_ptr().add(Self::PADDING) }
    }
}

impl ops::Deref for Packet {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        unsafe { slice::from_raw_parts(self.data_ptr(), self.len.into()) }
    }
}

impl ops::DerefMut for Packet {
    fn deref_mut(&mut self) -> &mut [u8] {
        unsafe { slice::from_raw_parts_mut(self.data_ptr_mut(), self.len.into()) }
    }
}

#[cfg(feature = "radio")]
impl From<Packet> for crate::radio::Packet {
    fn from(packet: Packet) -> crate::radio::Packet {
        crate::radio::Packet::from_parts(packet.buffer, packet.len)
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Ep0State {
    Idle,
    Write { leftover: u16 },
}

#[allow(dead_code)]
#[derive(Clone, Copy, PartialEq)]
#[repr(u8)]
enum EpOut1State {
    Idle = 0,
    DataReady = 1,
    BufferReady = 2,
    TransferInProgress = 3,
}

derive!(EpOut1State);

#[derive(Clone, Copy)]
enum PowerState {
    Off,
    RampUp { clock: bool, power: bool, usb: bool },
    Ready,
}

#[derive(Clone, Copy, PartialEq, binDebug)]
enum PowerEvent {
    USBDETECTED,
    USBREMOVED,
    USBPWRRDY,
}

impl PowerEvent {
    fn next() -> Option<Self> {
        POWER::borrow_unchecked(|power| {
            if power.EVENTS_USBDETECTED.read().bits() != 0 {
                power.EVENTS_USBDETECTED.zero();
                return Some(PowerEvent::USBDETECTED);
            }

            if power.EVENTS_USBREMOVED.read().bits() != 0 {
                power.EVENTS_USBREMOVED.zero();
                return Some(PowerEvent::USBREMOVED);
            }

            if power.EVENTS_USBPWRRDY.read().bits() != 0 {
                power.EVENTS_USBPWRRDY.zero();
                return Some(PowerEvent::USBPWRRDY);
            }

            None
        })
    }
}

#[derive(Clone, Copy, binDebug, PartialEq)]
enum UsbdEvent {
    ENDEPIN2,
    ENDEPOUT2,
    EP0SETUP,
    EP0DATADONE,
    EPDATA,
    USBEVENT,
    USBRESET,
}

impl UsbdEvent {
    fn next() -> Option<Self> {
        USBD::borrow_unchecked(|usbd| {
            if usbd.EVENTS_USBEVENT.read().bits() != 0 {
                usbd.EVENTS_USBEVENT.zero();
                return Some(UsbdEvent::USBEVENT);
            }

            if usbd.EVENTS_USBRESET.read().bits() != 0 {
                usbd.EVENTS_USBRESET.zero();
                return Some(UsbdEvent::USBRESET);
            }

            if usbd.EVENTS_EP0DATADONE.read().bits() != 0 {
                usbd.EVENTS_EP0DATADONE.zero();
                return Some(UsbdEvent::EP0DATADONE);
            }

            if usbd.EVENTS_EP0SETUP.read().bits() != 0 {
                usbd.EVENTS_EP0SETUP.zero();
                return Some(UsbdEvent::EP0SETUP);
            }

            if usbd.EVENTS_ENDEPIN2.read().bits() != 0 {
                usbd.EVENTS_ENDEPIN2.zero();
                return Some(UsbdEvent::ENDEPIN2);
            }

            if usbd.EVENTS_ENDEPOUT2.read().bits() != 0 {
                usbd.EVENTS_ENDEPOUT2.zero();
                return Some(UsbdEvent::ENDEPOUT2);
            }

            if usbd.EVENTS_EPDATA.read().bits() != 0 {
                usbd.EVENTS_EPDATA.zero();
                return Some(UsbdEvent::EPDATA);
            }

            if cfg!(debug_assertions) {
                unreachable()
            } else {
                None
            }
        })
    }
}

fn unreachable() -> ! {
    disconnect();
    semidap::panic!("unreachable")
}

fn todo() -> ! {
    disconnect();
    semidap::panic!("unimplemented")
}

fn short_ep0datadone_ep0status() {
    USBD::borrow_unchecked(|usbd| {
        usbd.SHORTS.rmw(|_, w| w.EP0DATADONE_EP0STATUS(1));
    });
}

fn unshort_ep0datadone_ep0status() {
    USBD::borrow_unchecked(|usbd| {
        usbd.SHORTS.rmw(|_, w| w.EP0DATADONE_EP0STATUS(0));
    });
}

#[allow(non_snake_case)]
fn EVENTCAUSE() -> eventcause::R {
    USBD::borrow_unchecked(|usbd| {
        let r = usbd.EVENTCAUSE.read();
        usbd.EVENTCAUSE.write(|w| {
            *w = r.into();
            w
        });
        semidap::debug!("{}", r);
        r
    })
}

#[allow(non_snake_case)]
fn EPDATASTATUS() -> epdatastatus::R {
    USBD::borrow_unchecked(|usbd| {
        let r = usbd.EPDATASTATUS.read();
        usbd.EPDATASTATUS.write(|w| {
            *w = r.into();
            w
        });
        r
    })
}

// NOTE(borrow_unchecked) all these are either single instruction reads w/o side effects or single
// instruction writes to registers that won't be RMW-ed
fn connect() {
    USBD::borrow_unchecked(|usbd| usbd.USBPULLUP.write(|w| w.CONNECT(1)));
    semidap::info!("pulled D+ up");
}

// simulate a disconnect so the host doesn't retry enumeration while the device is halted
fn disconnect() {
    USBD::borrow_unchecked(|usbd| usbd.USBPULLUP.zero());
    semidap::info!("detached from the bus");
}

#[allow(non_snake_case)]
fn SIZE_EPOUT1() -> u8 {
    USBD::borrow_unchecked(|usbd| usbd.SIZE_EPOUT1.read().bits())
}

#[allow(non_snake_case)]
fn EPINEN() -> epinen::R {
    USBD::borrow_unchecked(|usbd| usbd.EPINEN.read())
}

#[allow(dead_code)]
#[allow(non_snake_case)]
fn EPIN1_PTR() -> u32 {
    USBD::borrow_unchecked(|usbd| usbd.EPIN1_PTR.read().bits())
}

#[allow(non_snake_case)]
fn EPOUTEN() -> epouten::R {
    USBD::borrow_unchecked(|usbd| usbd.EPOUTEN.read())
}

#[allow(non_snake_case)]
fn EPOUT1_MAXCNT(cnt: u8) {
    USBD::borrow_unchecked(|usbd| usbd.EPOUT1_MAXCNT.write(|w| w.MAXCNT(cnt)))
}

#[allow(non_snake_case)]
fn STARTEPOUT1() {
    USBD::borrow_unchecked(|usbd| usbd.TASKS_STARTEPOUT1.write(|w| w.TASKS_STARTEPOUT(1)));
}

#[allow(non_snake_case)]
fn EP0STALL() {
    USBD::borrow_unchecked(|usbd| usbd.TASKS_EP0STALL.write(|w| w.TASKS_EP0STALL(1)));
    semidap::info!("EP0: stalled");
}

#[allow(non_snake_case)]
fn BMREQUESTTYPE() -> u8 {
    USBD::borrow_unchecked(|usbd| usbd.BMREQUESTTYPE.read().bits())
}

#[allow(non_snake_case)]
fn BREQUEST() -> u8 {
    USBD::borrow_unchecked(|usbd| usbd.BREQUEST.read().bits())
}

#[allow(non_snake_case)]
fn WVALUE() -> u16 {
    USBD::borrow_unchecked(|usbd| {
        u16::from(usbd.WVALUEL.read().bits()) | (u16::from(usbd.WVALUEH.read().bits()) << 8)
    })
}

#[allow(non_snake_case)]
fn WINDEX() -> u16 {
    USBD::borrow_unchecked(|usbd| {
        u16::from(usbd.WINDEXL.read().bits()) | (u16::from(usbd.WINDEXH.read().bits()) << 8)
    })
}

#[allow(non_snake_case)]
fn WLENGTH() -> u16 {
    USBD::borrow_unchecked(|usbd| {
        u16::from(usbd.WLENGTHL.read().bits()) | (u16::from(usbd.WLENGTHH.read().bits()) << 8)
    })
}

fn ep0status() {
    USBD::borrow_unchecked(|usbd| {
        usbd.TASKS_EP0STATUS.write(|w| w.TASKS_EP0STATUS(1));
    });
}

fn suspend() {
    semidap::info!("entering low power mode");
    USBD::borrow_unchecked(|usbd| usbd.LOWPOWER.write(|w| w.LOWPOWER(1)))
}

fn resume() {
    semidap::info!("leaving low power mode");
    USBD::borrow_unchecked(|usbd| usbd.LOWPOWER.zero())
}
