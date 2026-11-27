//! The runtime areas, one module each: their interfaces (provider, interface ids) and slot
//! functions. Slot functions take the object first (`This`), then the public API's arguments.

pub mod queue;
pub mod xdisplay;
pub mod xgame;
pub mod xlocalstorage;
pub mod xnetworking;
pub mod xpackage;
pub mod xstore;
pub mod xsystem;
pub mod xuser;

pub type This = *const crate::object::Object;

/// Builds a `Slot` from a function, its public API name and its console arguments after `this`.
macro_rules! slot {
    ($offset:literal, $name:literal, $func:path, $args:literal) => {
        $crate::object::Slot {
            offset: $offset,
            name: $name,
            func: $crate::object::Fp($func as *const ()),
            args: $args,
        }
    };
}
pub(crate) use slot;

#[cfg(test)]
pub(crate) mod test {
    use crate::object::{self, InterfaceDef};

    /// Slot `offset` of `def`'s object, as a function of type `F`.
    pub fn method<F: Copy>(def: &'static InterfaceDef, offset: usize) -> F {
        let p = object::of(def).slot(offset);
        assert_eq!(size_of::<F>(), size_of::<*const ()>());
        // SAFETY: the caller names the slot's signature.
        unsafe { std::mem::transmute_copy(&p) }
    }

    pub fn this(def: &'static InterfaceDef) -> super::This {
        object::of(def)
    }
}
