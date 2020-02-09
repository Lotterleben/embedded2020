use std::borrow::Cow;

mod util;

use heck::SnakeCase;
use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote};

use crate::{
    codegen,
    ir::{Device, Instances, Peripheral, Register},
};

pub fn device(device: &Device<'_>) -> String {
    let mut items = vec![];

    items.push(codegen::common(&device.name, &device.extra_docs));

    for periph in &device.peripherals {
        items.push(codegen::peripheral(periph));
    }

    quote!(#(#items)*).to_string()
}

fn common(name: &str, extra_docs: &Option<Cow<'_, str>>) -> TokenStream2 {
    let mut doc = format!("{} register API", name);
    if let Some(extra_docs) = extra_docs {
        doc.push_str("\n\n");
        doc.push_str(extra_docs);
    }
    quote!(
        #![allow(intra_doc_link_resolution_failure)]
        #![deny(missing_docs)]
        #![deny(rust_2018_compatibility)]
        #![deny(rust_2018_idioms)]
        #![deny(warnings)]
        #![doc = #doc]
        #![no_std]

        use core::marker::PhantomData;

        /// An instance of a peripheral
        pub trait Peripheral {
            /// The base address of the peripheral instance
            fn base_address() -> usize;
        }

        struct NotSendOrSync {
            inner: PhantomData<*mut ()>,
        }

        impl NotSendOrSync {
            fn new() -> Self {
                Self {
                    inner: PhantomData,
                }
            }
        }
    )
}

// TODO gate each peripheral family (e.g. `UARTx`) behind a Cargo feature
fn peripheral(peripheral: &Peripheral<'_>) -> TokenStream2 {
    let base_addr = match peripheral.instances {
        Instances::Single { base_address } => util::hex(base_address),
        _ => unimplemented!(),
    };

    let mut items = vec![];
    let mut field_decls = vec![];
    let mut field_exprs = vec![];

    for reg in &peripheral.registers {
        items.push(codegen::register(reg));

        let doc = reg
            .description
            .as_ref()
            .map(|s| Cow::from(&**s))
            .unwrap_or_else(|| format!("{} register", reg.name).into());
        let name = format_ident!("{}", *reg.name);
        field_decls.push(quote!(
            #[doc = #doc]
            pub #name: #name
        ));
        field_exprs.push(quote!(
            #name: #name::new()
        ));
    }

    let doc = format!("Singleton handle to the {} registers", peripheral.name);
    items.push(quote!(
        use core::sync::atomic::{AtomicBool, Ordering};

        const BASE_ADDRESS: usize = #base_addr;

        #[allow(non_snake_case)]
        #[doc = #doc]
        pub struct Registers {
            #(#field_decls,)*
        }

        unsafe impl Send for Registers {}

        impl Registers {
            /// # Safety
            /// Singleton
            unsafe fn new() -> Self {
                Self {
                    #(#field_exprs,)*
                }
            }

            fn taken() -> &'static AtomicBool {
                static TAKEN: AtomicBool = AtomicBool::new(false);
                &TAKEN
            }

            /// Grants temporary access to the peripheral, without checking if it has already been
            /// taken
            #[inline(always)]
            pub fn borrow_unchecked<T>(f: impl FnOnce(&Self) -> T) -> T {
                f(unsafe{ &Self::new() })
            }

            /// Seals the peripheral making it impossible to `take` it
            pub fn seal() {
                Self::taken().store(true, Ordering::Relaxed)
            }

            /// Takes ownership of the peripheral
            ///
            /// This constructor returns the `Some` variant only once
            pub fn take() -> Option<Self> {
                let taken = Self::taken();

                if taken
                    .compare_exchange_weak(false, true, Ordering::Relaxed, Ordering::Relaxed)
                    .is_ok()
                {
                    Some(unsafe{Self::new()})
                } else {
                    None
                }
            }
        }
    ));

    let doc = peripheral.description.as_ref().unwrap_or(&peripheral.name);
    let name = format_ident!("{}", *peripheral.name);
    let mod_name = util::ident(&peripheral.name.to_snake_case());
    quote!(
        #[allow(non_camel_case_types)]
        #[doc = #doc]
        pub type #name = #mod_name::Registers;

        #[doc = #doc]
        pub mod #mod_name {
            #(#items)*
        }
    )
}

fn register(register: &Register<'_>) -> TokenStream2 {
    let name = format_ident!("{}", *register.name);
    let mod_name = util::ident(&register.name.to_snake_case());

    let rty = util::width2ty(register.width);
    let mut mod_items = vec![];

    let mut rmethods = vec![];
    if register.access.can_read() {
        let mut chain = vec![];
        let methods = register
            .r_fields
            .iter()
            .map(|field| {
                let fty = util::bitwidth2ty(field.width);
                let field_name = format_ident!("{}", *field.name);
                let offset = util::unsuffixed(field.offset);
                let mask = util::hex(field.mask());
                let doc = field
                    .description
                    .as_ref()
                    .map(|s| Cow::from(&**s))
                    .unwrap_or_else(|| {
                        Cow::from(format!(
                            "Returns the contents of the bitfield {}",
                            field.name
                        ))
                    });

                let fname = &field.name;
                let adapter = if field.width < 4 {
                    format_ident!("Bin{}", field.width)
                } else {
                    format_ident!(
                        "Hex{}",
                        (field.width - 1) / 4 + 1
                    )
                };
                chain.push(
                    quote!(field(#fname, &regen_ufmt::#adapter(self.#field_name()))?),
                );
                quote!(
                    #[allow(non_snake_case)]
                    #[doc = #doc]
                    pub fn #field_name(self) -> #fty {
                        const OFFSET: u8 = #offset;
                        const MASK: #fty = #mask;
                        ((self.inner >> OFFSET) as #fty) & MASK
                    }
                )
            })
            .collect::<Vec<_>>();

        if !methods.is_empty() {
            let rname = &register.name;
            mod_items.push(quote!(
                /// View into the readable bitfields
                #[derive(Clone, Copy)]
                pub struct R {
                    inner: #rty,
                }

                impl From<#rty> for R {
                    fn from(bits: #rty) -> Self {
                        R { inner: bits }
                    }
                }

                impl From<R> for #rty {
                    fn from(r: R) -> Self {
                        r.inner
                    }
                }

                impl R {
                    #(#methods)*
                }

                #[cfg(feature = "udebug")]
                impl ufmt::uDebug for R {
                    fn fmt<W>(
                        &self,
                        f: &mut ufmt::Formatter<'_, W>,
                    ) -> Result<(), W::Error>
                    where
                        W: ufmt::uWrite + ?Sized,
                    {
                        f.debug_struct(#rname)? #(. #chain)* .finish()
                    }
                }
            ));

            rmethods.push(quote!(
                /// Reads the contents of the register in a single, volatile instruction
                pub fn read(&self) -> R {
                    R::from(unsafe { Self::address().read_volatile() })
                }
            ));
        } else {
            rmethods.push(quote!(
                /// Reads the contents of the register in a single, volatile instruction
                pub fn read(&self) -> #rty {
                    unsafe {
                        Self::address().read_volatile()
                    }
                }
            ));
        }
    }

    if register.access.can_write() {
        let (unsafety, safe) = if register.access.write_is_unsafe() {
            (quote!(unsafe), quote!())
        } else {
            (quote!(), quote!(unsafe))
        };

        let methods = register
            .w_fields
            .iter()
            .map(|field| {
                let fty = util::bitwidth2ty(field.width);
                let field_name = format_ident!("{}", &*field.name);
                let offset = util::unsuffixed(field.offset);
                let mask = util::hex(field.mask());
                let doc = field
                    .description
                    .as_ref()
                    .map(|s| Cow::from(&**s))
                    .unwrap_or_else(|| {
                        Cow::from(format!(
                            "Sets the contents of the bitfield {} to `val`",
                            field.name
                        ))
                    });

                quote!(
                    #[doc = #doc]
                    #[allow(non_snake_case)]
                    pub fn #field_name(&mut self, val: #fty) -> &mut Self {
                        const OFFSET: u8 = #offset;
                        const MASK: #fty = #mask;
                        self.inner &= !((MASK as #rty) << OFFSET);
                        self.inner |= ((val & MASK) as #rty) << OFFSET;
                        self
                    }
                )
            })
            .collect::<Vec<_>>();

        if !methods.is_empty() {
            mod_items.push(quote!(
                /// View into the writable bitfields
                #[derive(Clone, Copy)]
                pub struct W {
                    inner: #rty,
                }

                impl From<W> for #rty {
                    fn from(w: W) -> Self {
                        w.inner
                    }
                }

                impl W {
                    /// Writable view with all bitfields set to zero
                    pub fn zero() -> W {
                        W { inner: 0 }
                    }

                    #(#methods)*
                }
            ));

            rmethods.push(quote!(
                /// Writes the bits set by `f` to the register in a single, volatile instruction
                #[inline(always)]
                pub #unsafety fn write(&self, f: impl FnOnce(&mut W) -> &mut W) {
                    let mut w = W::zero();
                    f(&mut w);
                    #safe { Self::address().write_volatile(w.into()) }
                }
            ));
        } else {
            rmethods.push(quote!(
                /// Writes `bits` to the register in a single, volatile instruction
                pub #unsafety fn write(&self, bits: #rty) {
                    #safe { Self::address().write_volatile(bits) }
                }
            ));
        }
    }

    if register.access.can_read() && register.access.can_write() {
        let (unsafety, safe) = if register.access.write_is_unsafe() {
            (quote!(unsafe), quote!())
        } else {
            (quote!(), quote!(unsafe))
        };

        match (register.r_fields.is_empty(), register.w_fields.is_empty()) {
            (true, true) => {
                rmethods.push(quote!(
                    /// Updates the contents of the register using the closure `f`
                    ///
                    /// This performs a `read` operation followed by a `write` operation
                    #[inline(always)]
                    pub #unsafety fn rmw(&self, f: impl FnOnce(#rty) -> #rty) {
                        self.write(f(self.read()))
                    }
                ));
            }

            (false, false) => {
                let r2wmask = util::r2wmask(register);
                let inner = if r2wmask == 0 {
                    quote!(r.inner)
                } else {
                    let r2wmask = util::hex(r2wmask);
                    quote!(r.inner & !(#r2wmask))
                };
                mod_items.push(quote!(
                    impl From<R> for W {
                        fn from(r: R) -> W {
                            W {
                                inner: #inner,
                            }
                        }
                    }
                ));

                rmethods.push(quote!(
                    /// Updates the contents of the register using the closure `f`
                    ///
                    /// This performs a `read` operation followed by a `write` operation
                    #[inline(always)]
                    pub #unsafety fn rmw(
                        &self,
                        f: impl FnOnce(R, &mut W) -> &mut W,
                    ) {
                        let r = self.read();
                        let mut w = r.into();
                        f(r, &mut w);
                        #safe { Self::address().write_volatile(w.into()) }
                    }
                ));
            }

            _ => unimplemented!(),
        }
    }

    let address = if register.offset == 0 {
        quote!(super::BASE_ADDRESS)
    } else {
        let offset = util::hex(register.offset);
        quote!((super::BASE_ADDRESS + #offset))
    };
    let doc = register
        .description
        .as_ref()
        .map(|s| Cow::from(&**s))
        .unwrap_or_else(|| format!("{} register", register.name).into());
    let pty = if register.access.can_write() {
        quote!(*mut #rty)
    } else {
        quote!(*const #rty)
    };
    quote!(
        #[allow(non_camel_case_types)]
        #[doc = #doc]
        pub type #name = #mod_name::Register;

        #[doc = #doc]
        pub mod #mod_name {
            use crate::NotSendOrSync;

            /// Singleton handle to the register
            pub struct Register {
                _not_send_or_sync: NotSendOrSync,
            }

            impl Register {
                /// # Safety
                /// Singleton
                pub(crate) unsafe fn new() -> Self {
                    Self { _not_send_or_sync: NotSendOrSync::new() }
                }

                /// Returns the address of this register
                pub fn address() -> #pty {
                    #address as *mut _
                }

                #(#rmethods)*
            }

            #(#mod_items)*
        }
    )
}
